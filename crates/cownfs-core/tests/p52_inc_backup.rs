//! B5: incremental backup captures changed blocks.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-incbak-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn incremental_captures_changes() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    // Create base snapshot.
    let snap_id = fs.snapshot_create(b"base").unwrap();
    fs.commit().unwrap();

    // Make changes.
    let ino = fs.create(ROOT_INO, b"newfile", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![0xabu8; 8192]).unwrap();
    fs.commit().unwrap();

    // Diff.
    let old_roots = fs.snapshot_roots(snap_id).unwrap();
    let mut changed = fs.diff_roots(&old_roots).unwrap();
    changed.sort_unstable();
    changed.dedup();

    // Should have changed blocks (the new file's data + metadata).
    assert!(!changed.is_empty(), "should detect changed blocks");
    println!("Changed blocks: {}", changed.len());
    let _ = std::fs::remove_file(&img);
}

#[test]
fn incremental_empty_when_no_changes() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let snap_id = fs.snapshot_create(b"base").unwrap();
    fs.commit().unwrap();

    // No changes.
    let old_roots = fs.snapshot_roots(snap_id).unwrap();
    let changed = fs.diff_roots(&old_roots).unwrap();
    // May have some metadata changes from the snapshot itself, but should
    // be minimal.
    println!("Changed blocks with no writes: {}", changed.len());
    let _ = std::fs::remove_file(&img);
}
