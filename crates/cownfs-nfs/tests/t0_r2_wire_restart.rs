//! T0-R2 (wire): UNSTABLE writes are lost on restart; DATA_SYNC4 survives.
//!
//! This is the true wire-sequence test for R2. The boot verifier mechanism
//! (tested in t0_r2_verifier.rs) lets clients DETECT the restart; this test
//! verifies the underlying semantics:
//! 1. UNSTABLE WRITE without COMMIT is lost if the server restarts.
//! 2. DATA_SYNC4 WRITE is durable across a server restart.
//!
//! Uses the real cownfs-server binary (kill + restart).

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_nfs::nfs4::{DATA_SYNC4, UNSTABLE4};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn server_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("cownfs-server")
}

fn mkfs_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("cownfs-mkfs")
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn start_server(img: &std::path::Path, port: u16) -> Child {
    Command::new(server_bin())
        .arg(img)
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cownfs-server")
}

fn wait_ready(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not start on {addr}");
}

fn kill_server(mut child: Child) {
    // SIGKILL to simulate crash (UNSTABLE data must be lost).
    let _ = Command::new("kill")
        .args(["-9", &child.id().to_string()])
        .status();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(200));
}

fn setup_image(img: &std::path::Path) {
    let build = Command::new("cargo")
        .args(["build", "-p", "cownfs-mkfs"])
        .status()
        .expect("cargo build");
    assert!(build.success());
    let out = Command::new(mkfs_bin())
        .arg(img.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "mkfs failed");
}

/// Create a file via OPEN(CREATE), return (uuid, inode).
/// The creation is COMMITted so the file itself is durable.
fn create_via_open(c: &mut NfsClient, name: &[u8]) -> ([u8; 16], u64) {
    let clientid = establish_client(c, b"r2-wire");
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(clientid, b"owner", 3, name, 0o644);
    ops.getfh();
    let res = c.check_ok(b"create", ops);
    let (uuid, ino) = match &res[2] {
        Reply::Fh(fh) => (fh.fs_uuid, fh.inode),
        r => panic!("expected fh, got {r:?}"),
    };
    // Commit the creation so the file itself is durable.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.commit();
    c.check_ok(b"commit-create", ops);
    (uuid, ino)
}

fn read_file(c: &mut NfsClient, uuid: &[u8; 16], ino: u64) -> Vec<u8> {
    let mut ops = Ops::new();
    ops.putfh(uuid, ino);
    ops.read(0, 100);
    let res = c.check_ok(b"read", ops);
    match &res[1] {
        Reply::Read { data, .. } => data.clone(),
        r => panic!("expected Read, got {r:?}"),
    }
}

#[test]
fn r2_unstable_lost_on_restart() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("t0-r2-wire-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    setup_image(&img);

    let port = free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // --- Boot 1: UNSTABLE write, no commit. ---
    let srv = start_server(&img, port);
    wait_ready(port);

    let mut c = NfsClient::connect(&addr);
    let (uuid, ino) = create_via_open(&mut c, b"f.txt");

    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.write(0, UNSTABLE4, b"unstable data");
    c.check_ok(b"unstable-write", ops);
    drop(c);

    kill_server(srv);

    // --- Boot 2: data must be gone. ---
    let port2 = free_port();
    let addr2: std::net::SocketAddr = format!("127.0.0.1:{port2}").parse().unwrap();
    let srv = start_server(&img, port2);
    wait_ready(port2);

    let mut c2 = NfsClient::connect(&addr2);
    // Lookup the file to get its fh.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"f.txt");
    ops.getfh();
    let res = c2.check_ok(b"lookup", ops);
    let (uuid2, ino2) = match &res[2] {
        Reply::Fh(fh) => (fh.fs_uuid, fh.inode),
        r => panic!("expected fh, got {r:?}"),
    };

    let data = read_file(&mut c2, &uuid2, ino2);
    assert!(
        data.is_empty(),
        "R2: UNSTABLE write without COMMIT must be lost on restart, got {data:?}"
    );

    kill_server(srv);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn r2_datasync_survives_restart() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("t0-r2-datasync-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    setup_image(&img);

    let port = free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // --- Boot 1: DATA_SYNC4 write. ---
    let srv = start_server(&img, port);
    wait_ready(port);

    let mut c = NfsClient::connect(&addr);
    let (uuid, ino) = create_via_open(&mut c, b"g.txt");

    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.write(0, DATA_SYNC4, b"durable data");
    c.check_ok(b"datasync-write", ops);
    drop(c);

    kill_server(srv);

    // --- Boot 2: data must persist. ---
    let port2 = free_port();
    let addr2: std::net::SocketAddr = format!("127.0.0.1:{port2}").parse().unwrap();
    let srv = start_server(&img, port2);
    wait_ready(port2);

    let mut c2 = NfsClient::connect(&addr2);
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"g.txt");
    ops.getfh();
    let res = c2.check_ok(b"lookup", ops);
    let (uuid2, ino2) = match &res[2] {
        Reply::Fh(fh) => (fh.fs_uuid, fh.inode),
        r => panic!("expected fh, got {r:?}"),
    };

    let data = read_file(&mut c2, &uuid2, ino2);
    assert_eq!(
        data, b"durable data",
        "R2: DATA_SYNC4 write must survive restart"
    );

    kill_server(srv);
    let _ = std::fs::remove_file(&img);
}
