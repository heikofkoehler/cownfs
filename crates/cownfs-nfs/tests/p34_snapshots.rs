//! .snapshots virtual directory over NFS.
//!
//! LOOKUP ".snapshots" in any directory -> virtual dir listing snapshots.
//! Each snapshot appears as a subdirectory; files inside are read-only
//! views as of the snapshot. Mutating ops fail with NFS4ERR_ROFS.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_server_on, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

/// Format an image, write version1, snapshot it, then overwrite with version2.
fn setup_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-snap-it-{}-{n}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 512).unwrap();
    let ino = fs.create(ROOT_INO, b"data.txt", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, b"version1").unwrap();
    fs.commit().unwrap();
    let snap_id = fs.snapshot_create(b"snap1").unwrap();
    assert_eq!(snap_id, 1);
    fs.commit().unwrap();
    // Modify after the snapshot.
    fs.write(ino, 0, b"version2").unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn snapshots_dir_lists_snapshots() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.readdir(0, 8192, &[FATTR4_TYPE]);
    let res = c.check_ok(b"snap-list", ops);
    match &res[2] {
        Reply::Dir(entries) => {
            let names: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
            assert!(names.contains(&b"snap1".to_vec()), "names: {names:?}");
        }
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

#[test]
fn snapshot_file_reads_old_version() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    // Into the snapshot: old content.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.lookup(b"data.txt");
    ops.read(0, 100);
    let res = c.check_ok(b"snap-read", ops);
    match &res[4] {
        Reply::Read { data, .. } => assert_eq!(data, b"version1"),
        r => panic!("{r:?}"),
    }

    // Live file still has the new content.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"data.txt");
    ops.read(0, 100);
    let res = c.check_ok(b"live-read", ops);
    match &res[2] {
        Reply::Read { data, .. } => assert_eq!(data, b"version2"),
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

#[test]
fn snapshot_readdir_lists_snapshot_root() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.readdir(0, 8192, &[FATTR4_TYPE]);
    let res = c.check_ok(b"snap-readdir", ops);
    match &res[3] {
        Reply::Dir(entries) => {
            let names: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
            assert!(names.contains(&b"data.txt".to_vec()), "names: {names:?}");
        }
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

#[test]
fn snapshot_writes_are_rofs() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    // Navigate into the snapshot, then try to write.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.lookup(b"data.txt");
    ops.write(0, FILE_SYNC4, b"hack");
    let (status, replies) = c.call(b"snap-write", ops);
    assert_eq!(status, NFS4ERR_ROFS, "replies: {replies:?}");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn snapshots_dir_getattr_is_dir() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.getattr(&[FATTR4_TYPE]);
    let res = c.check_ok(b"snap-getattr", ops);
    match &res[2] {
        Reply::Attrs(a) => {
            let t = common::attr_u32(a, FATTR4_TYPE);
            assert_eq!(t, NF4DIR, "type: {t}");
        }
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}
