//! Step 5: admission control + overflow referral.
//!
//! --max-clients caps registered clients; new SETCLIENTID at the cap gets
//! NFS4ERR_DELAY. With --overflow-addr, LOOKUP on the root at the cap gets
//! NFS4ERR_MOVED and fs_locations advertises the overflow server.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_core::engine::Fs;
use cownfs_nfs::nfs4;
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

fn expect_op_err(res: &[Reply], idx: usize, status: u32, what: &str) {
    match &res[idx] {
        Reply::Err(s) => assert_eq!(*s, status, "{what}: wrong status"),
        r => panic!("{what}: expected Err({status}), got {r:?}"),
    }
}

fn expect_ok(res: &[Reply], idx: usize, what: &str) {
    match &res[idx] {
        Reply::Ok | Reply::Fh(_) | Reply::Attrs(_) => {}
        r => panic!("{what}: expected ok, got {r:?}"),
    }
}

#[test]
fn admission_control_and_overflow() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let img = dir.join(format!("cownfs-admission-{pid}.img"));
    let _ = std::fs::remove_file(&img);
    {
        let mut fs = Fs::format(&img, 8192).expect("format");
        fs.commit().expect("commit");
    }

    let port = free_port();
    let overflow_port = free_port();
    let overflow_addr = format!("127.0.0.1:{overflow_port}");

    let mut server: Child = Command::new(server_bin())
        .arg(&img)
        .arg(format!("127.0.0.1:{port}"))
        .arg("--max-clients")
        .arg("2")
        .arg("--overflow-addr")
        .arg(&overflow_addr)
        .arg("--grace-period-secs")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    wait_ready(port);

    // Two clients establish fine.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut c1 = NfsClient::connect(&addr);
    let _cid1 = establish_client(&mut c1, b"client-1");
    let mut c2 = NfsClient::connect(&addr);
    let _cid2 = establish_client(&mut c2, b"client-2");

    // Third client gets DELAY.
    let mut c3 = NfsClient::connect(&addr);
    let mut ops = Ops::new();
    ops.setclientid(&[3u8; 8], b"client-3");
    let replies = c3.call(b"t", ops).1;
    assert_eq!(replies.len(), 1);
    expect_op_err(&replies, 0, nfs4::NFS4ERR_DELAY, "setclientid at cap");

    // LOOKUP on root at the cap gets MOVED (fresh-mount redirect).
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"somefile");
    let replies = c3.call(b"t", ops).1;
    assert_eq!(replies.len(), 2);
    expect_ok(&replies, 0, "putrootfh");
    expect_op_err(&replies, 1, nfs4::NFS4ERR_MOVED, "lookup at cap");

    // fs_locations on root advertises the overflow server.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.getattr(&[nfs4::FATTR4_FS_LOCATIONS]);
    let replies = c3.call(b"t", ops).1;
    assert_eq!(replies.len(), 2);
    expect_ok(&replies, 0, "putrootfh");
    expect_ok(&replies, 1, "getattr fs_locations");

    let _ = server.kill();
    let _ = server.wait();
    let _ = std::fs::remove_file(&img);
}
