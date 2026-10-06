//! Step 6: persistent state WAL.
//!
//! With --state-wal, mutations are fsynced to a WAL. On restart, the
//! server replays the WAL instead of forcing clients through reclaim:
//! the client's pre-restart clientid and open stateid remain valid.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn server_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("cownfs-server")
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn wait_ready(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if std::net::TcpStream::connect(&addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not start on {addr}");
}

fn kill9(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn wal_replay_restores_state() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let img = dir.join(format!("cownfs-wal-{pid}.img"));
    let wal = dir.join(format!("cownfs-wal-{pid}.wal"));
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&wal);

    let uuid = {
        let mut fs = Fs::format(&img, 8192).expect("format");
        fs.commit().expect("commit");
        fs.uuid()
    };

    let port = free_port();

    // Start server with WAL.
    let mut server: Child = Command::new(server_bin())
        .arg(&img)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--state-wal")
        .arg(&wal)
        .arg("--grace-period-secs")
        .arg("0")
        .arg("--server-id")
        .arg("77")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    wait_ready(port);

    // Establish client, create and open a file.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (clientid, stateid, ino) = {
        let mut c = NfsClient::connect(&addr);
        let clientid = establish_client(&mut c, b"wal-client");
        let ino = common::create_file(&mut c, &uuid, clientid, ROOT_INO, b"w.txt", 0o644);
        let mut ops = Ops::new();
        ops.putrootfh();
        ops.open_nocreate(clientid, b"owner", 3, b"w.txt");
        let res = c.check_ok(b"open", ops);
        let stateid = match &res[1] {
            Reply::Open { stateid } => *stateid,
            r => panic!("open failed: {r:?}"),
        };
        (clientid, stateid, ino)
    };

    // Kill -9 (no clean shutdown; WAL must have it all).
    kill9(&mut server);
    assert!(wal.exists(), "WAL file should exist");

    // Restart with the same WAL.
    let mut server2: Child = Command::new(server_bin())
        .arg(&img)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--state-wal")
        .arg(&wal)
        .arg("--grace-period-secs")
        .arg("0")
        .arg("--server-id")
        .arg("77")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server2");
    wait_ready(port);

    // The pre-restart clientid should map to the same client (SETCLIENTID
    // with same name/verifier returns the same id, no reclaim needed).
    // More importantly: the old open stateid should still be valid —
    // use it for a CLOSE. If the WAL replay failed, this would be
    // STALE_STATEID.
    {
        let mut c = NfsClient::connect(&addr);
        // Re-establish with same verifier/name -> same clientid.
        let cid2 = establish_client(&mut c, b"wal-client");
        assert_eq!(cid2, clientid, "clientid should survive WAL replay");
        // Use the old stateid: close the open.
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.close(1, &stateid);
        let res = c.call(b"close", ops).1;
        match &res[1] {
            Reply::Ok => {}
            r => panic!("close with replayed stateid failed: {r:?}"),
        }
    }

    let _ = server2.kill();
    let _ = server2.wait();
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&wal);
}
