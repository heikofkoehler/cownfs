//! A2: allocation cursor + contiguous runs.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image(blocks: u64) -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-alloc-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, blocks).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn sequential_write_is_contiguous() {
    let img = test_image(8192);
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"seq", 0o644, 0, 0).unwrap();
    // 1 MiB sequential write = 256 blocks.
    fs.write(ino, 0, &vec![0xabu8; 1024 * 1024]).unwrap();
    fs.commit().unwrap();

    // Count extents: want few (ideally 1 contiguous run, but CoW may split).
    // We check that blocks are mostly contiguous by reading extent block ids.
    let extents = fs.debug_extent_blocks(ino).unwrap();
    let mut runs = 1;
    for w in extents.windows(2) {
        if w[1] != w[0] + 1 {
            runs += 1;
        }
    }
    println!("256 blocks in {runs} runs");
    // Allow some fragmentation, but should be far fewer than 256.
    assert!(runs < 32, "too fragmented: {runs} runs for 256 blocks");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn alloc_cursor_wraps() {
    let img = test_image(1024);
    let mut fs = Fs::open(&img).unwrap();
    // Allocate many blocks, ensure we don't run out prematurely and
    // the cursor wraps correctly.
    let mut inos = Vec::new();
    for i in 0..50 {
        let ino = fs
            .create(ROOT_INO, format!("f{i}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        fs.write(ino, 0, &vec![i as u8; 4096]).unwrap();
        inos.push(ino);
    }
    fs.commit().unwrap();
    // All should be readable.
    for (i, ino) in inos.iter().enumerate() {
        let data = fs.read(*ino, 0, 4096).unwrap();
        assert!(data.iter().all(|&b| b == i as u8));
    }
    let _ = std::fs::remove_file(&img);
}
