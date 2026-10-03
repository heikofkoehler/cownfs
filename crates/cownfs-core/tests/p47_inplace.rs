//! A3: in-place overwrite when block unshared.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-inplace-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn overwrite_reuses_block_without_snapshot() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![1u8; 8192]).unwrap();
    // No commit yet — blocks are in txg_allocated.
    let blocks_before = fs.debug_extent_blocks(ino).unwrap();
    let free_before = fs.free_block_count();

    // Overwrite the same range in the same txg: should reuse blocks.
    fs.write(ino, 0, &vec![2u8; 8192]).unwrap();
    let blocks_after = fs.debug_extent_blocks(ino).unwrap();
    let free_after = fs.free_block_count();

    assert_eq!(
        blocks_before, blocks_after,
        "blocks should be reused for in-place overwrite in same txg"
    );
    assert_eq!(
        free_before, free_after,
        "no net allocation for in-place overwrite"
    );
    fs.commit().unwrap();
    // Data should be updated.
    let data = fs.read(ino, 0, 8192).unwrap();
    assert!(data.iter().all(|&b| b == 2));
    let _ = std::fs::remove_file(&img);
}

#[test]
fn overwrite_cows_with_snapshot() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![1u8; 8192]).unwrap();
    fs.commit().unwrap();
    let blocks_before = fs.debug_extent_blocks(ino).unwrap();

    // Snapshot pins the blocks.
    let snap_id = fs.snapshot_create(b"snap").unwrap();
    fs.commit().unwrap();

    // Overwrite: must CoW (allocate new blocks).
    fs.write(ino, 0, &vec![2u8; 8192]).unwrap();
    fs.commit().unwrap();
    let blocks_after = fs.debug_extent_blocks(ino).unwrap();

    assert_ne!(
        blocks_before, blocks_after,
        "blocks should differ (CoW) when snapshot pins"
    );
    // Snapshot should still see old data.
    let snap_data = fs.snapshot_read(snap_id, ino, 0, 8192).unwrap();
    assert!(snap_data.iter().all(|&b| b == 1), "snapshot data corrupted");
    // Live should see new data.
    let live_data = fs.read(ino, 0, 8192).unwrap();
    assert!(live_data.iter().all(|&b| b == 2));
    let _ = std::fs::remove_file(&img);
}
