//! Grace period + reclaim (RFC 7530 §8.4) with the real server binary.
//!
//! 1. Fresh boot enters grace: SETCLIENTID works, but a new OPEN gets
//!    NFS4ERR_GRACE.
//! 2. After kill -9 + restart (new grace period): the old clientid is
//!    STALE_CLIENTID; a re-established client gets GRACE for new opens,
//!    but CLAIM_PREVIOUS reclaim of its pre-restart open succeeds and the
//!    data is intact; LOCK with reclaim=true succeeds, reclaim=false
//!    gets GRACE.
//! 3. After grace expiry: CLAIM_PREVIOUS gets NFS4ERR_NO_GRACE and normal
//!    opens work again.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::{NFS4ERR_GRACE, NFS4ERR_NO_GRACE, NFS4ERR_STALE_CLIENTID, NFS4_OK};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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

fn tmp_img(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cownfs-grace-{}-{tag}.img", std::process::id()))
}

/// Format the image, returning its uuid (like t7_chaos does).
fn format_img(tag: &str) -> (std::path::PathBuf, [u8; 16]) {
    let img = tmp_img(tag);
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 8192).expect("format");
    fs.commit().expect("commit");
    let uuid = fs.uuid();
    drop(fs);
    (img, uuid)
}

fn start_server(img: &std::path::Path, port: u16, grace_secs: u64) -> Child {
    let child = Command::new(server_bin())
        .arg(img)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--grace-period-secs")
        .arg(grace_secs.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cownfs-server");
    let addr = format!("127.0.0.1:{port}");
    let start = std::time::Instant::now();
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
    let _ = child.kill();
    let _ = child.wait();
    // Let the port and the image lock release.
    std::thread::sleep(Duration::from_millis(300));
}

/// The per-op reply must be Err(status).
fn expect_op_err(res: &[Reply], idx: usize, status: u32, what: &str) {
    match &res[idx] {
        Reply::Err(s) => assert_eq!(*s, status, "{what}: wrong status"),
        r => panic!("{what}: expected Err({status}), got {r:?}"),
    }
}

#[test]
fn grace_on_boot_rejects_new_opens() {
    let (img, _uuid) = format_img("boot");
    let port = free_port();
    let mut srv = start_server(&img, port, 60);
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // SETCLIENTID is always allowed (clients need it to reclaim).
    let mut c = NfsClient::connect(&addr);
    let clientid = establish_client(&mut c, b"grace-boot");

    // But a fresh OPEN during grace gets NFS4ERR_GRACE.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(clientid, b"owner", 3, b"f.txt", 0o644);
    let (_, res) = c.call(b"grace-boot-open", ops);
    expect_op_err(&res, 1, NFS4ERR_GRACE, "new open during grace");

    kill9(&mut srv);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn reclaim_after_restart() {
    let (img, uuid) = format_img("reclaim");

    // Phase 1: normal operation (short grace, wait it out), create + write.
    let port1 = free_port();
    let mut srv = start_server(&img, port1, 1);
    std::thread::sleep(Duration::from_millis(1500));
    let addr1: std::net::SocketAddr = format!("127.0.0.1:{port1}").parse().unwrap();
    let ino = {
        let mut c = NfsClient::connect(&addr1);
        let clientid = establish_client(&mut c, b"grace-a");
        let ino = common::create_file(&mut c, &uuid, clientid, ROOT_INO, b"g.txt", 0o644);
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.write(0, 2, b"grace-data");
        c.check_ok(b"write", ops);
        ino
    };
    kill9(&mut srv);

    // Phase 2: restart with a long grace period.
    let port2 = free_port();
    let mut srv = start_server(&img, port2, 60);
    let addr2: std::net::SocketAddr = format!("127.0.0.1:{port2}").parse().unwrap();

    let mut c = NfsClient::connect(&addr2);
    let clientid_b = establish_client(&mut c, b"grace-c");

    // New (non-reclaim) open during grace -> GRACE.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_nocreate(clientid_b, b"owner", 3, b"g.txt");
    let (_, res) = c.call(b"new-open-during-grace", ops);
    expect_op_err(&res, 1, NFS4ERR_GRACE, "new open during grace");

    // Reclaim the pre-restart open -> OK.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.open_reclaim(clientid_b, b"owner", 3);
    let (_, res) = c.call(b"reclaim-open", ops);
    match &res[1] {
        Reply::Open { .. } => {}
        r => panic!("reclaim open failed: {r:?}"),
    }

    // Data survived the restart.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.read(0, 100);
    let res = c.check_ok(b"read-after-reclaim", ops);
    match &res[1] {
        Reply::Read { data, .. } => assert_eq!(data, b"grace-data"),
        r => panic!("read failed: {r:?}"),
    }

    // Reclaim a byte-range lock (pre-restart stateid unknown -> zeros).
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.lock_new_reclaim(clientid_b, 2, 0, 100, &[0u8; 16], b"lockowner", true);
    let (_, res) = c.call(b"reclaim-lock", ops);
    assert!(
        matches!(res[1], Reply::Lock { .. }),
        "reclaim lock failed: {:?}",
        res[1]
    );

    // New (non-reclaim) lock during grace -> GRACE.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.lock_new_reclaim(clientid_b, 2, 200, 100, &[0u8; 16], b"lockowner2", false);
    let (_, res) = c.call(b"new-lock-during-grace", ops);
    expect_op_err(&res, 1, NFS4ERR_GRACE, "new lock during grace");

    kill9(&mut srv);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn stale_clientid_after_restart() {
    let (img, uuid) = format_img("stale");
    let _ = uuid;

    // Phase 1: establish a client and an open.
    let port1 = free_port();
    let mut srv = start_server(&img, port1, 1);
    std::thread::sleep(Duration::from_millis(1500));
    let addr1: std::net::SocketAddr = format!("127.0.0.1:{port1}").parse().unwrap();
    let (old_clientid, _ino) = {
        let mut c = NfsClient::connect(&addr1);
        let clientid = establish_client(&mut c, b"grace-old");
        let ino = common::create_file(&mut c, &uuid, clientid, ROOT_INO, b"s.txt", 0o644);
        // Make the create durable so the post-restart open reaches the
        // clientid check instead of failing with NOENT.
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.write(0, 2, b"sync");
        c.check_ok(b"write", ops);
        (clientid, ino)
    };
    kill9(&mut srv);

    // Phase 2: the old clientid is unknown to the new server incarnation.
    let port2 = free_port();
    let mut srv = start_server(&img, port2, 60);
    let addr2: std::net::SocketAddr = format!("127.0.0.1:{port2}").parse().unwrap();
    let mut c = NfsClient::connect(&addr2);
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_nocreate(old_clientid, b"owner", 3, b"s.txt");
    let (_, res) = c.call(b"open-with-stale-clientid", ops);
    expect_op_err(&res, 1, NFS4ERR_STALE_CLIENTID, "stale clientid");

    kill9(&mut srv);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn no_grace_after_expiry() {
    let (img, uuid) = format_img("expiry");
    let port = free_port();
    let mut srv = start_server(&img, port, 1);
    // Wait out the 1s grace period.
    std::thread::sleep(Duration::from_millis(1500));
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let mut c = NfsClient::connect(&addr);
    let clientid = establish_client(&mut c, b"grace-exp");
    let ino = common::create_file(&mut c, &uuid, clientid, ROOT_INO, b"e.txt", 0o644);

    // Reclaim after grace expiry -> NO_GRACE.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ino);
    ops.open_reclaim(clientid, b"owner", 3);
    let (_, res) = c.call(b"reclaim-after-expiry", ops);
    expect_op_err(&res, 1, NFS4ERR_NO_GRACE, "reclaim after grace");

    // Normal open works again.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_nocreate(clientid, b"owner", 3, b"e.txt");
    let (st, _) = c.call(b"open-after-expiry", ops);
    assert_eq!(st, NFS4_OK, "normal open after grace expiry");

    kill9(&mut srv);
    let _ = std::fs::remove_file(&img);
}
