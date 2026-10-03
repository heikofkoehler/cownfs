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

/// Regression test for macOS `cd` into a snapshot: READDIR returns
/// FATTR4_FILEHANDLE per entry; the client PUTFHs that handle directly.
/// Before the fix, snapshot entries carried live filehandles with bogus
/// inodes and PUTFH failed with NFS4ERR_STALE.
#[test]
fn snapshot_readdir_filehandles_roundtrip() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    // READDIR .snapshots, asking for FILEHANDLE per entry.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.readdir(0, 8192, &[FATTR4_TYPE, FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"snap-readdir-fh", ops);
    let fhs: Vec<Vec<u8>> = match &res[2] {
        Reply::Dir(entries) => {
            assert!(!entries.is_empty());
            entries
                .iter()
                .map(|e| common::attr_raw(&e.attrs, FATTR4_FILEHANDLE).to_vec())
                .collect()
        }
        r => panic!("{r:?}"),
    };

    // Each entry filehandle must PUTFH cleanly and be a directory
    // (this is what `cd` does).
    for fh in &fhs {
        let mut ops = Ops::new();
        ops.putfh_raw(fh);
        ops.getattr(&[FATTR4_TYPE]);
        let res = c.check_ok(b"snap-cd", ops);
        match &res[1] {
            Reply::Attrs(a) => {
                assert_eq!(common::attr_u32(a, FATTR4_TYPE), NF4DIR);
            }
            r => panic!("fh {fh:?}: {r:?}"),
        }
    }
    let _ = std::fs::remove_file(&img);
}

/// GETATTR FILEHANDLE on a looked-up snapshot must return a handle that
/// PUTFHs back to the same snapshot (not a live handle).
#[test]
fn snapshot_getattr_filehandle_roundtrip() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"snap-fh-getattr", ops);
    let fh = match &res[3] {
        Reply::Attrs(a) => common::attr_raw(a, FATTR4_FILEHANDLE).to_vec(),
        r => panic!("{r:?}"),
    };

    // The handle must be a snapshot handle (SNAP magic), not a live one.
    assert_eq!(&fh[4..8], b"SNAP", "expected snapshot filehandle");

    // PUTFH it back and read the snapshot's root: must list data.txt.
    let mut ops = Ops::new();
    ops.putfh_raw(&fh);
    ops.readdir(0, 8192, &[FATTR4_TYPE]);
    let res = c.check_ok(b"snap-fh-readdir", ops);
    match &res[1] {
        Reply::Dir(entries) => {
            let names: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
            assert!(names.contains(&b"data.txt".to_vec()), "names: {names:?}");
        }
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

/// Same round-trip for a file inside a snapshot: the handle must read
/// the snapshot's version of the data.
#[test]
fn snapshot_file_filehandle_roundtrip() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.lookup(b"data.txt");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"snapfile-fh-getattr", ops);
    let fh = match &res[4] {
        Reply::Attrs(a) => common::attr_raw(a, FATTR4_FILEHANDLE).to_vec(),
        r => panic!("{r:?}"),
    };
    assert_eq!(&fh[4..8], b"SNAP", "expected snapshot filehandle");

    // PUTFH the file handle directly and read: old content.
    let mut ops = Ops::new();
    ops.putfh_raw(&fh);
    ops.read(0, 100);
    let res = c.check_ok(b"snapfile-fh-read", ops);
    match &res[1] {
        Reply::Read { data, .. } => assert_eq!(data, b"version1"),
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

/// The .snapshots dir's own FILEHANDLE must round-trip too.
#[test]
fn snapshots_dir_filehandle_roundtrip() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"snapdir-fh-getattr", ops);
    let fh = match &res[2] {
        Reply::Attrs(a) => common::attr_raw(a, FATTR4_FILEHANDLE).to_vec(),
        r => panic!("{r:?}"),
    };

    let mut ops = Ops::new();
    ops.putfh_raw(&fh);
    ops.readdir(0, 8192, &[FATTR4_TYPE]);
    let res = c.check_ok(b"snapdir-fh-readdir", ops);
    match &res[1] {
        Reply::Dir(entries) => {
            let names: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
            assert!(names.contains(&b"snap1".to_vec()), "names: {names:?}");
        }
        r => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(&img);
}

/// macOS `cat` pattern: OPEN (read-only) a file inside a snapshot must
/// succeed; READ returns the snapshot's version. Before the fix, any OPEN
/// on a snapshot handle failed with NFS4ERR_ROFS.
#[test]
fn snapshot_open_read_succeeds() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let id = common::establish_client(&mut c, b"snap-open");

    // LOOKUP into the snapshot, then OPEN the file read-only.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.open_nocreate(id, b"owner1", 1, b"data.txt"); // flags=1: read access
    let res = c.check_ok(b"snap-open", ops);
    let stateid = match &res[3] {
        Reply::Open { stateid } => *stateid,
        r => panic!("{r:?}"),
    };

    // READ via the opened (snapshot) FH: old content.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.lookup(b"data.txt");
    ops.read(0, 100);
    let res = c.check_ok(b"snap-open-read", ops);
    match &res[4] {
        Reply::Read { data, .. } => assert_eq!(data, b"version1"),
        r => panic!("{r:?}"),
    }

    // CLOSE the open.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.lookup(b"data.txt");
    ops.close(1, &stateid);
    c.check_ok(b"snap-close", ops);
    let _ = std::fs::remove_file(&img);
}

/// OPEN with CREATE inside a snapshot must fail with ROFS.
#[test]
fn snapshot_open_create_fails_rofs() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let id = common::establish_client(&mut c, b"snap-open2");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.open_create(id, b"owner1", 1, b"newfile", 0o644);
    let (status, replies) = c.call(b"snap-open-create", ops);
    assert_eq!(status, NFS4ERR_ROFS, "replies: {replies:?}");
    let _ = std::fs::remove_file(&img);
}

/// OPEN requesting WRITE access inside a snapshot must fail with ROFS.
#[test]
fn snapshot_open_write_fails_rofs() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let id = common::establish_client(&mut c, b"snap-open3");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"snap1");
    ops.open_nocreate(id, b"owner1", 2, b"data.txt"); // flags=2: write access
    let (status, replies) = c.call(b"snap-open-write", ops);
    assert_eq!(status, NFS4ERR_ROFS, "replies: {replies:?}");
    let _ = std::fs::remove_file(&img);
}
