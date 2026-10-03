//! macOS xnu NFSv4.0 client session replay for .snapshots.
//!
//! Replays Heiko's exact 2026-10-03 terminal session against a mounted
//! cownfs share, encoded as the compounds macOS actually sends:
//!
//!   cd .snapshots            -> PUTROOTFH, LOOKUP, GETATTR (broad mask)
//!   ls                       -> ACCESS, READDIR (caches entry FHs)
//!   cd hourly-...            -> PUTFH(cached FH) directly, no LOOKUP
//!   cat foo                  -> PUTFH(cached FH), OPEN(read), READ, CLOSE
//!
//! macOS-specific behaviors encoded here:
//! - broad GETATTR mask (TYPE..FILEHANDLE) on every vnode operation
//! - ACCESS(0x3f) probes before directory reads
//! - filehandle caching: READDIR-returned FHs are PUTFH'd without LOOKUP
//! - file reads go through OPEN(read)+READ+CLOSE, never stateless PUTFH+READ
//! - SECINFO bundled into LOOKUP compounds
//!
//! This caught two real bugs: ESTALE on cd (FATTR4_FILEHANDLE was a live
//! handle) and EROFS on cat (OPEN rejected on snapshot handles).

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server_on, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

/// Attribute set macOS requests on vnode operations (broad, includes FILEHANDLE).
fn macos_mask() -> Vec<u32> {
    vec![
        FATTR4_TYPE,
        FATTR4_FILEID,
        FATTR4_SIZE,
        FATTR4_MODE,
        FATTR4_NUMLINKS,
        FATTR4_OWNER,
        FATTR4_OWNER_GROUP,
        FATTR4_SPACE_USED,
        FATTR4_TIME_ACCESS,
        FATTR4_TIME_MODIFY,
        FATTR4_TIME_METADATA,
        FATTR4_MOUNTED_ON_FILEID,
        FATTR4_FILEHANDLE,
    ]
}

fn setup_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-macos-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 512).unwrap();
    let ino = fs.create(ROOT_INO, b"foo", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, b"snapshot-data").unwrap();
    fs.commit().unwrap();
    fs.snapshot_create(b"hourly-20261003-062924").unwrap();
    fs.commit().unwrap();
    fs.write(ino, 0, b"live-data").unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

/// macOS `cd DIR`: PUTROOTFH, LOOKUP, SECINFO, GETATTR(broad mask).
fn macos_cd(c: &mut NfsClient, dir: &[u8]) {
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(dir);
    ops.secinfo(dir);
    ops.getattr(&macos_mask());
    let res = c.check_ok(b"macos-cd", ops);
    assert!(matches!(res[0], Reply::Ok));
    assert!(matches!(res[1], Reply::Ok));
    assert!(matches!(res[2], Reply::Secinfo(_)));
    assert!(matches!(res[3], Reply::Attrs(_)));
}

/// macOS `ls`: ACCESS(0x3f) probe then READDIR with broad mask.
/// Returns (name, filehandle-bytes) per entry: macOS caches these.
fn macos_ls(c: &mut NfsClient, fh: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut ops = Ops::new();
    match fh {
        Some(f) => ops.putfh_raw(f),
        None => ops.putrootfh(),
    }
    ops.access(0x3f);
    let res = c.check_ok(b"macos-access", ops);
    match &res[1] {
        Reply::Access { granted, .. } => assert_ne!(*granted & 0x01, 0, "read bit"),
        r => panic!("{r:?}"),
    }

    let mut ops = Ops::new();
    match fh {
        Some(f) => ops.putfh_raw(f),
        None => ops.putrootfh(),
    }
    ops.readdir(0, 8192, &macos_mask());
    let res = c.check_ok(b"macos-ls", ops);
    match &res[1] {
        Reply::Dir { entries, .. } => entries
            .iter()
            .map(|e| {
                (
                    e.name.clone(),
                    common::attr_raw(&e.attrs, FATTR4_FILEHANDLE).to_vec(),
                )
            })
            .collect(),
        r => panic!("{r:?}"),
    }
}

#[test]
fn macos_snapshot_session() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"macos");

    // cd .snapshots
    macos_cd(&mut c, b".snapshots");

    // ls .snapshots (via root FH + lookup to get .snapshots FH first).
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"getfh-snapshots", ops);
    let snapshots_fh = match &res[2] {
        Reply::Attrs(a) => common::attr_raw(a, FATTR4_FILEHANDLE).to_vec(),
        r => panic!("{r:?}"),
    };
    let entries = macos_ls(&mut c, Some(&snapshots_fh));
    let hourly_fh = entries
        .iter()
        .find(|(n, _)| n == b"hourly-20261003-062924")
        .map(|(_, f)| f.clone())
        .expect("hourly snapshot in listing");

    // cd hourly-... : macOS PUTFHs the cached direntry FH directly.
    let mut ops = Ops::new();
    ops.putfh_raw(&hourly_fh);
    ops.getattr(&macos_mask());
    let res = c.check_ok(b"macos-cd-hourly", ops);
    match &res[1] {
        Reply::Attrs(a) => assert_eq!(common::attr_u32(a, FATTR4_TYPE), NF4DIR),
        r => panic!("{r:?}"),
    }

    // ls inside the snapshot.
    let entries = macos_ls(&mut c, Some(&hourly_fh));
    let foo_fh = entries
        .iter()
        .find(|(n, _)| n == b"foo")
        .map(|(_, f)| f.clone())
        .expect("foo in snapshot listing");

    // cat foo: macOS does PUTFH(dir), OPEN(name, read), then READ, CLOSE.
    let mut ops = Ops::new();
    ops.putfh_raw(&hourly_fh);
    ops.open_nocreate(clientid, b"macos-owner", 1, b"foo");
    let res = c.check_ok(b"macos-cat-open", ops);
    let stateid = match &res[1] {
        Reply::Open { stateid } => *stateid,
        r => panic!("{r:?}"),
    };
    // Sanity: the OPEN left cfh on the snapshot file; its FILEHANDLE
    // must match the cached READDIR handle (same object).
    let mut ops = Ops::new();
    ops.putfh_raw(&hourly_fh);
    ops.lookup(b"foo");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"macos-cat-fh", ops);
    match &res[2] {
        Reply::Attrs(a) => {
            let fh2 = common::attr_raw(a, FATTR4_FILEHANDLE).to_vec();
            assert_eq!(fh2, foo_fh, "OPEN and READDIR FHs must match");
        }
        r => panic!("{r:?}"),
    }

    // READ the content via the snapshot file handle.
    let mut ops = Ops::new();
    ops.putfh_raw(&foo_fh);
    ops.read(0, 100);
    let res = c.check_ok(b"macos-cat-read", ops);
    match &res[1] {
        Reply::Read { data, .. } => assert_eq!(data, b"snapshot-data"),
        r => panic!("{r:?}"),
    }

    // CLOSE.
    let mut ops = Ops::new();
    ops.putfh_raw(&foo_fh);
    ops.close(1, &stateid);
    c.check_ok(b"macos-cat-close", ops);

    let _ = std::fs::remove_file(&img);
}

/// macOS `cp` from a snapshot must fail cleanly (ROFS), not STALE or IO.
#[test]
fn macos_write_to_snapshot_fails_rofs() {
    let img = setup_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"macos2");

    // Get the snapshot dir FH.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".snapshots");
    ops.lookup(b"hourly-20261003-062924");
    ops.getattr(&[FATTR4_FILEHANDLE]);
    let res = c.check_ok(b"getfh", ops);
    let snap_fh = match &res[3] {
        Reply::Attrs(a) => common::attr_raw(a, FATTR4_FILEHANDLE).to_vec(),
        r => panic!("{r:?}"),
    };

    // macOS copyfile: OPEN(CREATE) inside the snapshot.
    let mut ops = Ops::new();
    ops.putfh_raw(&snap_fh);
    ops.open_create(clientid, b"macos-owner", 3, b"copy", 0o644);
    let (status, replies) = c.call(b"macos-copy", ops);
    assert_eq!(status, NFS4ERR_ROFS, "replies: {replies:?}");

    let _ = std::fs::remove_file(&img);
}
