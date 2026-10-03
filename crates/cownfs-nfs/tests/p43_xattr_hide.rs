//! D1: .xattrs is hidden from NFS clients.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server_with_quotas, NfsClient, Ops};
use cownfs_core::engine::Fs;
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-hide-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 1024).unwrap();
    // Create a file and set an xattr (creates the .xattrs file).
    let ino = fs.create(1, b"f", 0o644, 0, 0).unwrap();
    fs.setxattr(ino, b"user.tag", b"value").unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn xattr_file_hidden_from_nfs() {
    let img = test_image();
    let srv = spawn_server_with_quotas(&img, &[]);
    let mut c = NfsClient::connect(&srv.addr);
    let _id = establish_client(&mut c, b"hidden");

    // LOOKUP .xattrs should return NOENT.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b".xattrs");
    let (status, _) = c.call(b"lookup-xattrs", ops);
    assert_eq!(status, NFS4ERR_NOENT, ".xattrs should be hidden");

    // READDIR should not list .xattrs.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.readdir(0, 8192, &[]);
    let res = c.check_ok(b"readdir", ops);
    // Verify no .xattrs in the entries (check via debug format).
    let debug = format!("{res:?}");
    assert!(
        !debug.contains(".xattrs"),
        ".xattrs should not appear in readdir"
    );

    let _ = std::fs::remove_file(&img);
}
