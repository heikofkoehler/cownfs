//! Lease renewal: background thread keeps the lease alive.

use cownfs_core::engine::Fs;
use std::process::{Command, Stdio};
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

#[test]
fn lease_renewal_keeps_lease_alive() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-lease-renew-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    // Format.
    let out = Command::new(mkfs_bin())
        .arg(img.to_str().unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());

    // Start server with short TTL (5s) and node-id.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(server_bin())
        .args([
            "--node-id",
            "test-node",
            "--lease-ttl",
            "5",
            img.to_str().unwrap(),
            &addr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for server to start and acquire lease.
    std::thread::sleep(Duration::from_secs(2));

    // Check lease is held.
    let fs = Fs::open(&img).unwrap();
    assert!(
        fs.lease_check("test-node").unwrap(),
        "lease should be held after startup"
    );
    drop(fs);

    // Wait past the original TTL (5s). If renewal works, lease is still held.
    // Renewal happens every ttl/3 ≈ 1.6s.
    std::thread::sleep(Duration::from_secs(7));

    let fs = Fs::open(&img).unwrap();
    assert!(
        fs.lease_check("test-node").unwrap(),
        "lease should still be held after renewal (background thread works)"
    );
    drop(fs);

    // Kill server.
    let _ = child.kill();
    let _ = child.wait();

    // Lease should expire soon (5s TTL). Wait and verify another node can acquire.
    std::thread::sleep(Duration::from_secs(6));
    let mut fs = Fs::open(&img).unwrap();
    assert!(
        fs.lease_acquire("other-node", 60).unwrap(),
        "other node should acquire after expiry"
    );

    let _ = std::fs::remove_file(&img);
}
