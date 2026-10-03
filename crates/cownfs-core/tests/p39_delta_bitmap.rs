//! A1: delta bitmap tests — only dirty words are written per txg.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image(blocks: u64) -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-delta-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, blocks).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn delta_bitmap_survives_reopen() {
    let img = test_image(4096);
    let mut fs = Fs::open(&img).unwrap();
    // Several commits with allocations (each should write a delta, not full).
    for i in 0..10 {
        let ino = fs
            .create(ROOT_INO, format!("f{i}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        fs.write(ino, 0, &vec![i as u8; 8192]).unwrap();
        fs.commit().unwrap();
    }
    let free_before = fs.free_block_count();
    drop(fs);

    // Reopen: bitmap must be reconstructed via delta application.
    let fs = Fs::open(&img).unwrap();
    assert_eq!(fs.free_block_count(), free_before);
    // Data must be intact.
    for i in 0..10 {
        let (ino, _) = fs
            .lookup(ROOT_INO, format!("f{i}").as_bytes())
            .unwrap()
            .unwrap();
        let data = fs.read(ino, 0, 8192).unwrap();
        assert!(data.iter().all(|&b| b == i as u8), "data mismatch f{i}");
    }
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}

#[test]
fn delta_bitmap_checkpoint() {
    let img = test_image(2048);
    let mut fs = Fs::open(&img).unwrap();
    // Force many commits to trigger the checkpoint (every 100).
    for i in 0..105 {
        let ino = fs
            .create(ROOT_INO, format!("c{i}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        fs.write(ino, 0, b"x").unwrap();
        fs.commit().unwrap();
    }
    let free_before = fs.free_block_count();
    drop(fs);

    let fs = Fs::open(&img).unwrap();
    assert_eq!(fs.free_block_count(), free_before);
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}
