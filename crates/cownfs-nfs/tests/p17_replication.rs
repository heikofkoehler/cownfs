//! Phase 1c: end-to-end replication test.
//!
//! Primary -> replica via cownfs-replicate send/receive. Verifies that
//! incremental replication advances the replica to the primary's state.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use cownfs_core::engine::Fs;

fn test_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cownfs-repl-test-{}-{}", name, std::process::id()));
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

fn start_receiver(image: &PathBuf, port: u16) -> Child {
    let child = Command::new(bin())
        .args([
            "receive",
            image.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
        ])
        .spawn()
        .expect("spawn receive");
    // Give the receiver time to bind. macOS is slower than Linux;
    // 200ms raced on macOS (connection refused). 1s is safe.
    std::thread::sleep(Duration::from_millis(1000));
    child
}

fn run_sender(primary: &PathBuf, port: u16, state: &PathBuf) {
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
    assert!(
        out.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn wait_for_exit(mut child: Child) {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert!(status.success(), "receiver exited with {status}");
                return;
            }
            None => {
                if start.elapsed() > Duration::from_secs(10) {
                    child.kill().ok();
                    panic!("receiver did not exit in time");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[test]
fn replicate_incremental() {
    let dir = test_dir("incr");
    let primary = dir.join("primary.img");
    let replica = dir.join("replica.img");
    let state = dir.join("repl.state");

    // Build primary with some data.
    let mut fs = Fs::format(&primary, 512).unwrap();
    let ino1 = fs.create(1, b"a.txt", 0o644, 1000, 1000).unwrap();
    fs.write(ino1, 0, b"hello replica").unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Replica starts as a copy.
    std::fs::copy(&primary, &replica).unwrap();

    // Round 1: full send (no state file yet).
    let port = free_port();
    let rx = start_receiver(&replica, port);

    run_sender(&primary, port, &state);
    wait_for_exit(rx);

    // Verify replica has the data.
    let fs2 = Fs::open(&replica).unwrap();
    let (ino, _) = fs2.lookup(1, b"a.txt").unwrap().unwrap();
    assert_eq!(fs2.read(ino, 0, 99).unwrap(), b"hello replica");
    drop(fs2);

    // More changes on primary.
    let mut fs = Fs::open(&primary).unwrap();
    let ino2 = fs.create(1, b"b.txt", 0o644, 1000, 1000).unwrap();
    fs.write(ino2, 0, b"second file with more content").unwrap();
    fs.write(ino1, 0, b"modified content").unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Round 2: incremental send.
    let port = free_port();
    let rx = start_receiver(&replica, port);

    run_sender(&primary, port, &state);
    wait_for_exit(rx);

    // Verify replica caught up.
    let fs2 = Fs::open(&replica).unwrap();
    let (ino, _) = fs2.lookup(1, b"a.txt").unwrap().unwrap();
    assert_eq!(fs2.read(ino, 0, 99).unwrap(), b"modified content");
    let (ino, _) = fs2.lookup(1, b"b.txt").unwrap().unwrap();
    assert_eq!(
        fs2.read(ino, 0, 99).unwrap(),
        b"second file with more content"
    );
    // Generations must match.
    let fsp = Fs::open(&primary).unwrap();
    assert_eq!(fs2.generation(), fsp.generation());
    drop(fs2);
    drop(fsp);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replicate_empty_diff_is_noop() {
    let dir = test_dir("noop");
    let primary = dir.join("primary.img");
    let replica = dir.join("replica.img");
    let state = dir.join("repl.state");

    let mut fs = Fs::format(&primary, 256).unwrap();
    fs.commit().unwrap();
    drop(fs);
    std::fs::copy(&primary, &replica).unwrap();

    // Full send.
    let port = free_port();
    let rx = start_receiver(&replica, port);

    run_sender(&primary, port, &state);
    wait_for_exit(rx);

    // Incremental with no changes — should send ~0 data blocks.
    let port = free_port();
    let rx = start_receiver(&replica, port);

    run_sender(&primary, port, &state);
    wait_for_exit(rx);

    // Replica still opens cleanly at the same generation.
    let fsp = Fs::open(&primary).unwrap();
    let fsr = Fs::open(&replica).unwrap();
    assert_eq!(fsp.generation(), fsr.generation());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replicate_drive_writes_status() {
    let dir = test_dir("drive");
    let primary = dir.join("primary.img");
    let replica = dir.join("replica.img");
    let state = dir.join("repl.state");

    let mut fs = Fs::format(&primary, 256).unwrap();
    fs.commit().unwrap();
    drop(fs);
    std::fs::copy(&primary, &replica).unwrap();

    // Receiver only accepts one connection; drive with interval 1 and let
    // it fail after the first successful replication.
    let port = free_port();
    let rx = start_receiver(&replica, port);

    let mut drive = Command::new(bin())
        .args([
            "drive",
            primary.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
            "--state",
            state.to_str().unwrap(),
            "--interval",
            "1",
        ])
        .spawn()
        .expect("spawn drive");
    // First replication succeeds, receiver exits; subsequent attempts fail.
    wait_for_exit(rx);
    std::thread::sleep(Duration::from_secs(3));
    drive.kill().ok();
    drive.wait().ok();

    // Status file must exist with valid JSON showing the failure.
    let status_path = {
        let mut p = state.clone();
        p.set_extension("status");
        p
    };
    let data = std::fs::read_to_string(&status_path).expect("status file exists");
    assert!(
        data.contains("\"consecutive_failures\""),
        "has failure count"
    );
    assert!(data.contains("\"lag_seconds\""), "has lag metric");
    assert!(data.contains("\"generations_behind\""), "has behind metric");
    // At least one success happened before the receiver went away.
    assert!(
        data.contains("\"last_success_unix\": 0") == false,
        "should have one success: {data}"
    );

    // The `status` subcommand reads it back.
    let out = Command::new(bin())
        .args([
            "status",
            primary.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ])
        .output()
        .expect("run status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("consecutive_failures"));

    std::fs::remove_dir_all(&dir).ok();
}
