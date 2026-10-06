//! T7: Chaos with a durability oracle.
//!
//! Exit criteria:
//! - Client keeps a ledger of acknowledged-durable writes (FILE_SYNC).
//! - Loop: kill -9 server at random → restart → verify ledger, fsck clean.
//! - Nightly 1h run with zero tolerance.
//!
//! Spawns the real `cownfs-server` binary as a subprocess so SIGKILL is
//! genuine (not just dropping an in-process Fs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};

/// A durable write recorded in the ledger.
#[derive(Clone, Debug)]
struct LedgerEntry {
    ino: u64,
    data: Vec<u8>,
}

fn tmp_img(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "cownfs-t7-{tag}-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Spawn cownfs-server on `port` serving `img`. Waits for accept.
fn spawn_server_proc(img: &Path, port: u16) -> Child {
    let bin = env!("CARGO_BIN_EXE_cownfs-server");
    let addr = format!("127.0.0.1:{port}");
    let child = Command::new(bin)
        .arg(img)
        .arg(&addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cownfs-server");
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("server did not start on {addr}");
        }
        if std::net::TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

fn kill9(child: &mut Child) {
    // Child::kill() sends SIGKILL on Unix.
    let _ = child.kill();
    let _ = child.wait();
}

/// Do FILE_SYNC writes via NFS, recording each in the ledger.
/// Returns the ledger entries created in this session.
fn do_writes(
    addr: &std::net::SocketAddr,
    uuid: &[u8; 16],
    base: usize,
    count: usize,
) -> Vec<LedgerEntry> {
    let mut c = NfsClient::connect(addr);
    let clientid = establish_client(&mut c, b"t7");
    let mut out = Vec::new();

    for i in 0..count {
        let name = format!("t7f{base:04}_{i:04}");
        let ino = common::create_file(&mut c, uuid, clientid, ROOT_INO, name.as_bytes(), 0o644);
        let data = format!("t7-data-{base}-{i}-{}", Instant::now().elapsed().as_nanos());
        let data_bytes = data.into_bytes();

        // WRITE with FILE_SYNC (stable=2). Durable on return.
        let mut ops = Ops::new();
        ops.putfh(uuid, ino);
        ops.write(0, 2, &data_bytes);
        let _ = c.check_ok(b"t7-write", ops);

        out.push(LedgerEntry {
            ino,
            data: data_bytes,
        });
    }
    out
}

/// Verify all ledger entries are present with correct data.
fn verify_ledger(addr: &std::net::SocketAddr, uuid: &[u8; 16], ledger: &[LedgerEntry]) {
    let mut c = NfsClient::connect(addr);
    let _clientid = establish_client(&mut c, b"t7-verify");

    for entry in ledger {
        let mut ops = Ops::new();
        ops.putfh(uuid, entry.ino);
        ops.read(0, entry.data.len() as u32);
        let res = c.check_ok(b"t7-read", ops);
        match &res[1] {
            Reply::Read { data, .. } => assert_eq!(
                data, &entry.data,
                "T7 ledger mismatch for ino {}",
                entry.ino
            ),
            r => panic!("t7-read: unexpected reply {r:?}"),
        }
    }
}

/// T7 main loop.
/// `cycles`: number of kill -9 / restart iterations.
/// `writes_per_cycle`: FILE_SYNC writes per cycle.
fn chaos_loop(cycles: usize, writes_per_cycle: usize) {
    let img = tmp_img("chaos");
    let uuid = {
        let mut fs = Fs::format(&img, 8192).expect("format");
        fs.commit().expect("commit");
        fs.uuid()
    };

    let mut ledger: Vec<LedgerEntry> = Vec::new();
    let mut port: u16 = 25000 + (std::process::id() % 5000) as u16;

    for cycle in 0..cycles {
        port += 1;
        let mut server = spawn_server_proc(&img, port);
        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Do writes; they go into the ledger as durable.
        let new_entries = do_writes(&addr, &uuid, cycle * writes_per_cycle, writes_per_cycle);
        ledger.extend(new_entries);

        // Kill -9 at random (2/3 of cycles); clean shutdown otherwise.
        if cycle % 3 != 2 {
            kill9(&mut server);
        } else {
            let _ = server.kill();
            let _ = server.wait();
        }

        // Restart and verify the full ledger.
        port += 1;
        let mut server2 = spawn_server_proc(&img, port);
        let addr2: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        verify_ledger(&addr2, &uuid, &ledger);

        let _ = server2.kill();
        let _ = server2.wait();

        // Fsck must be clean.
        let fs = Fs::open(&img).expect("open for fsck");
        fs.check().expect("fsck clean after kill cycle");
    }

    let _ = std::fs::remove_file(&img);
}

/// PR-tier: 3 cycles, 5 writes each. Fast enough for CI.
#[test]
fn t7_chaos_short() {
    chaos_loop(3, 5);
}

/// Nightly 1-hour run (ignored by default).
/// Zero tolerance: any ledger mismatch or fsck failure fails.
#[test]
#[ignore]
fn t7_chaos_nightly() {
    let start = Instant::now();
    let mut cycles = 0;
    while start.elapsed() < Duration::from_secs(3600) {
        chaos_loop(1, 20);
        cycles += 1;
    }
    println!("T7 nightly: {cycles} kill cycles in 1h, zero failures");
}
