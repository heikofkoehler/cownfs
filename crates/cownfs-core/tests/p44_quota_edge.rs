//! D1: quota edge cases — rename-over, setattr uid change, rmdir.

use cownfs_core::engine::{Fs, ROOT_INO, SetAttrs};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn setup() -> (std::path::PathBuf, Fs) {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-quotaedge-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    (img, fs)
}

#[test]
fn quota_released_on_rename_over() {
    let (_img, mut fs) = setup();
    fs.set_quota(1000, 10);
    // Create two files as uid 1000.
    let a = fs.create(ROOT_INO, b"a", 0o644, 1000, 1000).unwrap();
    fs.write(a, 0, &vec![0u8; 8192]).unwrap(); // 2 data blocks + 1 inode = 3
    let b = fs.create(ROOT_INO, b"b", 0o644, 1000, 1000).unwrap();
    fs.write(b, 0, &vec![0u8; 8192]).unwrap(); // +3 = 6
    let before = fs.quota_usage(1000);
    // Rename a over b: b is unlinked, its quota should be released.
    fs.rename(ROOT_INO, b"a", ROOT_INO, b"b").unwrap();
    let after = fs.quota_usage(1000);
    // b's blocks (3) should be freed; a's blocks remain.
    assert!(after < before, "quota not released on rename-over: {before} -> {after}");
    assert_eq!(after, 3, "expected 3 blocks (just a)");
}

#[test]
fn quota_transfers_on_uid_change() {
    let (_img, mut fs) = setup();
    fs.set_quota(1000, 10);
    fs.set_quota(2000, 10);
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, &vec![0u8; 8192]).unwrap(); // 3 blocks for uid 1000
    assert_eq!(fs.quota_usage(1000), 3);
    assert_eq!(fs.quota_usage(2000), 0);
    // Change owner to uid 2000.
    fs.setattr(
        ino,
        &SetAttrs {
            uid: Some(2000),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.quota_usage(1000), 0, "old uid should be released");
    assert_eq!(fs.quota_usage(2000), 3, "new uid should be charged");
}

#[test]
fn quota_released_on_rmdir() {
    let (_img, mut fs) = setup();
    fs.set_quota(1000, 10);
    let dir = fs.mkdir(ROOT_INO, b"d", 0o755, 1000, 1000).unwrap();
    let f = fs.create(dir, b"f", 0o644, 1000, 1000).unwrap();
    fs.write(f, 0, &vec![0u8; 8192]).unwrap();
    let before = fs.quota_usage(1000);
    assert!(before > 0);
    fs.unlink(dir, b"f").unwrap();
    fs.rmdir(ROOT_INO, b"d").unwrap();
    let after = fs.quota_usage(1000);
    // Only root remains (uid 0), so uid 1000 should be 0.
    assert_eq!(after, 0, "quota not fully released: {after}");
}
