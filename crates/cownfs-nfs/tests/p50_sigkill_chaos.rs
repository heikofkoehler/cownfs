//! D5: real SIGKILL chaos — spawn cownfs-server as a subprocess, drive NFS
//! traffic concurrently, kill -9 it mid-flight, restart, verify recovery.

mod common;

use common::{create_file, establish_client, NfsClient, Ops, Reply};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn server_bin() -> String {
    env!("CARGO_BIN_EXE_cownfs-server").to_string()
}

fn mkfs_bin() -> String {
    // cownfs-mkfs is in a separate package; locate via target dir.
    let test_exe = std::env::current_exe().expect("current test exe");
    let target_debug = test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target dir");
    target_debug
        .join("cownfs-mkfs")
        .to_string_lossy()
        .to_string()
}

fn fsck_bin() -> String {
    // cownfs-fsck is in a separate package; locate via target dir.
    let test_exe = std::env::current_exe().expect("current test exe");
    let target_debug = test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target dir");
    target_debug
        .join("cownfs-fsck")
        .to_string_lossy()
        .to_string()
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = l.local_addr().expect("local addr").port();
    drop(l);
    p
}

fn spawn(img: &std::path::Path, port: u16, log_suffix: &str) -> Child {
    let log = std::env::temp_dir().join(format!("p50-chaos-{port}-{log_suffix}.log"));
    let logf = std::fs::File::create(&log).expect("create server log");
    Command::new(server_bin())
        .arg(img)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--txg-interval-ms")
        .arg("10000") // 10s: avoid background sync racing with traffic
        .stdout(std::process::Stdio::null())
        .stderr(logf)
        .spawn()
        .expect("spawn cownfs-server")
}

fn wait_ready(addr: &SocketAddr, child: &mut Child, port: u16, log_suffix: &str) {
    let start = Instant::now();
    loop {
        if std::net::TcpStream::connect_timeout(addr, Duration::from_millis(100)).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let log = std::env::temp_dir().join(format!("p50-chaos-{port}-{log_suffix}.log"));
            let err = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("server exited with {status}: {err}");
        }
        if start.elapsed() > Duration::from_secs(10) {
            let log = std::env::temp_dir().join(format!("p50-chaos-{port}-{log_suffix}.log"));
            let err = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("server did not become ready at {addr}: {err}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigkill(child: &mut Child) {
    let pid = child.id();
    let st = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run kill -9");
    assert!(st.success(), "kill -9 failed");
    let _ = child.wait();
}

fn get_uuid(c: &mut NfsClient) -> [u8; 16] {
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.getfh();
    let res = c.check_ok(b"getfh", ops);
    match &res[1] {
        Reply::Fh(fh) => fh.fs_uuid,
        r => panic!("expected fh, got {r:?}"),
    }
}

#[test]
fn sigkill_mid_traffic_recovers() {
    // Simple unique name (distinct from p50_minimal test).
    let img = std::env::temp_dir().join(format!("p50-chaos-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    // Format via mkfs binary (not in-process) to ensure the image is
    // exactly what the server binary expects.
    let st = Command::new(mkfs_bin())
        .arg("--size")
        .arg("16M")
        .arg(&img)
        .status()
        .expect("run cownfs-mkfs");
    assert!(st.success(), "mkfs failed");
    // Give the OS a moment to flush; also explicitly sync the file.
    std::thread::sleep(Duration::from_millis(100));

    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // Spawn server.
    let mut child = spawn(&img, port, "s1");
    wait_ready(&addr, &mut child, port, "s1");

    // Write baseline data (committed).
    let uuid;
    {
        let mut c = NfsClient::connect(&addr);
        uuid = get_uuid(&mut c);
        let cid = establish_client(&mut c, b"sigkill1");
        let ino = create_file(&mut c, &uuid, cid, 1, b"baseline", 0o644);
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.write(0, 2, b"committed-baseline-data");
        c.check_ok(b"write sync", ops);
    }

    // Drive traffic concurrently in background thread.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let addr_clone = addr;
    let uuid_clone = uuid;
    let traffic = std::thread::spawn(move || {
        let mut c = NfsClient::connect(&addr_clone);
        let cid = establish_client(&mut c, b"sigkill2");
        let ino = create_file(&mut c, &uuid_clone, cid, 1, b"victim", 0o644);
        let mut i = 0u64;
        while !stop_clone.load(Ordering::Relaxed) {
            let mut w = Ops::new();
            w.putfh(&uuid_clone, ino);
            // UNSTABLE writes: may be lost on crash, that's fine.
            w.write((i % 100) * 64, 1, &[(i % 256) as u8; 64]);
            let _ = c.call(b"unstable", w);
            i += 1;
            if i > 10000 {
                break;
            }
        }
    });

    // Let traffic run briefly, then SIGKILL mid-flight.
    std::thread::sleep(Duration::from_millis(500));
    sigkill(&mut child);
    stop.store(true, Ordering::Relaxed);
    let _ = traffic.join();

    // Restart server on same image.
    let mut child2 = spawn(&img, port, "s2");
    wait_ready(&addr, &mut child2, port, "s2");

    // Verify baseline data survived (it was committed before the kill).
    {
        let mut c = NfsClient::connect(&addr);
        let uuid2 = get_uuid(&mut c);
        assert_eq!(uuid, uuid2, "uuid must survive restart");
        let mut ops = Ops::new();
        ops.putfh(&uuid2, 1);
        ops.lookup(b"baseline");
        ops.getfh();
        let res = c.check_ok(b"lookup baseline", ops);
        let bino = match &res[2] {
            Reply::Fh(fh) => fh.inode,
            r => panic!("expected fh, got {r:?}"),
        };
        let mut ops = Ops::new();
        ops.putfh(&uuid2, bino);
        ops.read(0, 64);
        let res = c.check_ok(b"read baseline", ops);
        match &res[1] {
            Reply::Read { data, .. } => assert_eq!(data, b"committed-baseline-data"),
            r => panic!("expected data, got {r:?}"),
        }
    }
    sigkill(&mut child2);

    // External fsck with checked status (not library Fs::check).
    let out = Command::new(fsck_bin())
        .arg(&img)
        .output()
        .expect("run cownfs-fsck");
    assert!(
        out.status.success(),
        "fsck failed after SIGKILL: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_file(&img);
}
