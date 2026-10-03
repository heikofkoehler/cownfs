//! Quota wire tests: per-UID block limits return NFS4ERR_DQUOT.

#[path = "common/mod.rs"]
mod common;

use common::{create_file, establish_client, spawn_server_with_quotas, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-quota-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 1024).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn quota_exceeded_returns_dquot() {
    let img = test_image();
    // uid 0 (test client + root) limited to 10 blocks. Root uses 1.
    let srv = spawn_server_with_quotas(&img, &[(0, 10)]);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"quota");

    // Create (1 block for the inode). Usage: 1 (root) + 1 = 2.
    let ino = create_file(&mut c, &srv.uuid, id, ROOT_INO, b"qf", 0o644);

    // Write 4 blocks (16KB) -> usage 6. OK.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(0, FILE_SYNC4, &vec![1u8; 16384]);
    c.check_ok(b"write-1", ops);

    // Write 4 more blocks -> usage 10, at the limit. OK.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(16384, FILE_SYNC4, &vec![2u8; 16384]);
    c.check_ok(b"write-2", ops);

    // One more block exceeds -> NFS4ERR_DQUOT (69).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(32768, FILE_SYNC4, &vec![3u8; 4096]);
    let (status, _) = c.call(b"write-over", ops);
    assert_eq!(status, NFS4ERR_DQUOT);

    let _ = std::fs::remove_file(&img);
}

#[test]
fn quota_create_blocked_when_full() {
    let img = test_image();
    // uid 0 limited to 2 blocks: root (1) + one create (1) = 2, at limit.
    // Second create fails.
    let srv = spawn_server_with_quotas(&img, &[(0, 2)]);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"quota2");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(id, b"q", 3, b"first", 0o644);
    c.check_ok(b"create-first", ops);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(id, b"q2", 3, b"second", 0o644);
    let (status, _) = c.call(b"create-second", ops);
    assert_eq!(status, NFS4ERR_DQUOT);

    let _ = std::fs::remove_file(&img);
}
