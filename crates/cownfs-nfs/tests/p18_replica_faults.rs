//! Phase 1e: replication fault-injection and read-only replica tests.
//!
//! 1. A replication that dies mid-transfer (no COMMIT) must leave the
//!    replica serving the old consistent snapshot — never a torn one.
//! 2. A replica image served by a read-only NFS server exposes the
//!    replicated data and rejects mutations with ROFS.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_read_only_server_on, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn test_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cownfs-repl-fault-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cownfs-replicate"))
}

/// Send HELLO + SNAPSHOT + a few BLOCKs, then die without COMMIT.
/// Returns the receiver child (which should exit with an error).
fn partial_send(replica_img: &PathBuf, port: u16, blocks_to_send: usize) -> std::process::Child {
    let rx = Command::new(bin())
        .args([
            "receive",
            replica_img.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
        ])
        .spawn()
        .expect("spawn receive");
    std::thread::sleep(Duration::from_millis(200));

    let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    // HELLO
    s.write_all(&0x434F_5752u32.to_be_bytes()).unwrap();
    s.write_all(&1u32.to_be_bytes()).unwrap();
    // SNAPSHOT (tag 1) with dummy roots
    s.write_all(&[1u8]).unwrap();
    for _ in 0..4 {
        s.write_all(&123u64.to_be_bytes()).unwrap();
        s.write_all(&1u32.to_be_bytes()).unwrap();
    }
    s.write_all(&999u64.to_be_bytes()).unwrap(); // generation
    s.write_all(&[0u8; 16]).unwrap(); // uuid
    s.write_all(&512u64.to_be_bytes()).unwrap(); // block_count (must match)
                                                 // A few BLOCKs (tag 2) — garbage data blocks, NOT the superblock.
    for i in 0..blocks_to_send {
        s.write_all(&[2u8]).unwrap();
        s.write_all(&(100u64 + i as u64).to_be_bytes()).unwrap();
        s.write_all(&[0xABu8; 4096]).unwrap();
    }
    // Die without COMMIT.
    drop(s);
    rx
}

fn wait_for_exit(mut child: std::process::Child) -> bool {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(status) => return status.success(),
            None => {
                if start.elapsed() > Duration::from_secs(10) {
                    child.kill().ok();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[test]
fn torn_replication_leaves_old_snapshot() {
    let dir = test_dir("torn");
    let primary = dir.join("primary.img");
    let replica = dir.join("replica.img");

    // Primary with committed data.
    let mut fs = Fs::format(&primary, 512).unwrap();
    let ino = fs
        .create(ROOT_INO, b"stable.txt", 0o644, 1000, 1000)
        .unwrap();
    fs.write(ino, 0, b"stable data").unwrap();
    fs.commit().unwrap();
    let gen_before = fs.generation();
    drop(fs);
    std::fs::copy(&primary, &replica).unwrap();

    // Partial replication that dies before COMMIT.
    let port = free_port();
    let rx = partial_send(&replica, port, 5);
    let ok = wait_for_exit(rx);
    assert!(!ok, "receiver should fail on truncated stream");

    // Replica must still serve the old consistent snapshot.
    let fs2 = Fs::open(&replica).unwrap();
    assert_eq!(fs2.generation(), gen_before, "generation must not advance");
    let (ino, _) = fs2.lookup(ROOT_INO, b"stable.txt").unwrap().unwrap();
    assert_eq!(fs2.read(ino, 0, 99).unwrap(), b"stable data");
    // fsck-level sanity: the image opens without error.
    drop(fs2);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_only_replica_serves_replicated_data() {
    let dir = test_dir("ro-nfs");
    let primary = dir.join("primary.img");
    let replica = dir.join("replica.img");
    let state = dir.join("repl.state");

    // Primary with data.
    let mut fs = Fs::format(&primary, 512).unwrap();
    let ino = fs
        .create(ROOT_INO, b"shared.txt", 0o644, 1000, 1000)
        .unwrap();
    fs.write(ino, 0, b"replicated content").unwrap();
    fs.commit().unwrap();
    drop(fs);
    std::fs::copy(&primary, &replica).unwrap();

    // Replicate.
    let port = free_port();
    let rx = Command::new(bin())
        .args([
            "receive",
            replica.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
        ])
        .spawn()
        .expect("spawn receive");
    std::thread::sleep(Duration::from_millis(200));
    let out = Command::new(bin())
        .args([
            "send",
            primary.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
            "--state",
            state.to_str().unwrap(),
        ])
        .output()
        .expect("run send");
    assert!(out.status.success());
    assert!(wait_for_exit(rx));

    // Serve the replica read-only over NFS.
    let srv = spawn_read_only_server_on(&replica);
    let mut c = NfsClient::connect(&srv.addr);
    let _id = establish_client(&mut c, b"replica-ro");

    // READ the replicated file.
    let (ino2, _) = {
        let fs2 = Fs::open(&replica).unwrap();
        fs2.lookup(ROOT_INO, b"shared.txt").unwrap().unwrap()
    };
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino2);
    ops.read(0, 99);
    let (st, resp) = c.call(b"ro-replica-read", ops);
    assert_eq!(st, NFS4_OK, "read from replica should succeed");
    match &resp[1] {
        Reply::Read { data, .. } => assert_eq!(data, b"replicated content"),
        r => panic!("expected Read reply, got {r:?}"),
    }

    // Mutations are rejected with ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"nope", 0o755);
    let (st, _) = c.call(b"ro-replica-create", ops);
    assert_eq!(st, NFS4ERR_ROFS, "replica must be read-only");

    std::fs::remove_dir_all(&dir).ok();
}
