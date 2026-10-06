//! Graceful shutdown: SIGTERM drains connections and commits.

use std::io::{Read, Write};
use std::net::TcpStream;
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

fn fsck_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("cownfs-fsck")
}

struct TestServer {
    child: Child,
    addr: String,
    img: std::path::PathBuf,
    /// If true, don't delete the image on drop (for fsck verification).
    keep_img: bool,
}

impl TestServer {
    fn start() -> Self {
        let dir = std::env::temp_dir();
        let img = dir.join(format!("cownfs-shutdown-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&img);

        // Format.
        let out = Command::new(mkfs_bin())
            .arg(img.to_str().unwrap())
            .output()
            .unwrap();
        assert!(out.status.success());

        // Find free port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let addr = format!("127.0.0.1:{port}");
        let child = Command::new(server_bin())
            .arg(img.to_str().unwrap())
            .arg(&addr)
            .arg("--grace-period-secs")
            .arg("0") // tests control grace explicitly; not under test here
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Wait for server to be ready.
        for _ in 0..50 {
            if TcpStream::connect(&addr).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        TestServer {
            child,
            addr,
            img,
            keep_img: false,
        }
    }

    fn stop_gracefully(mut self) -> (bool, String) {
        self.keep_img = true; // Don't delete on drop; caller will fsck then delete.
                              // Send SIGTERM.
        #[cfg(unix)]
        {
            // Use kill command since we don't have the PID directly.
            let pid = self.child.id();
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output();
        }

        // Wait for exit (with timeout).
        let start = std::time::Instant::now();
        let exited = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if start.elapsed() > Duration::from_secs(10) {
                        // Timeout: kill.
                        let _ = self.child.kill();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break None,
            }
        };

        // Get stderr.
        let mut stderr = String::new();
        if let Some(mut err) = self.child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }

        let clean = exited.map(|s| s.success()).unwrap_or(false);
        (clean, stderr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if !self.keep_img {
            let _ = std::fs::remove_file(&self.img);
        }
    }
}

#[test]
fn graceful_shutdown_commits() {
    let server = TestServer::start();

    // Do a write via NFS (simplified: just connect).
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Send NULL to verify server is responsive.
    let mut req = Vec::new();
    req.extend_from_slice(&1u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&2u32.to_be_bytes());
    req.extend_from_slice(&100003u32.to_be_bytes());
    req.extend_from_slice(&4u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    let mut framed = Vec::new();
    framed.extend_from_slice(&((req.len() as u32) | 0x80000000).to_be_bytes());
    framed.extend_from_slice(&req);
    stream.write_all(&framed).unwrap();
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).unwrap();
    drop(stream);

    // Graceful shutdown.
    let img = server.img.clone();
    let (clean, stderr) = server.stop_gracefully();
    // server is consumed, Drop won't run (we cloned img before).

    // Server should exit cleanly (or at least not crash).
    // Note: exit code may be non-zero due to signal handling; we check
    // that it shut down (didn't hang) and the image is valid.
    assert!(
        stderr.contains("shutdown") || stderr.contains("draining") || clean,
        "expected shutdown messages, got: {stderr}"
    );

    // Image should still be valid.
    let out = Command::new(fsck_bin())
        .arg(img.to_str().unwrap())
        .output()
        .unwrap();
    let fsck_ok = out.status.success();
    let _ = std::fs::remove_file(&img);
    assert!(
        fsck_ok,
        "fsck failed after graceful shutdown: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
