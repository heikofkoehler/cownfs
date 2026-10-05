//! P0.1: Bitmap CRC32C sidecars + generation fallback.
//!
//! Tests for the per-bitmap-block CRC32C sidecars and the newest-to-oldest
//! superblock fallback on `BitmapCorrupt`.

use cownfs_core::engine::{Fs, FsError};
use std::path::PathBuf;

/// Helper to create a test image path.
fn test_img(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("cownfs-p53-{name}-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Corrupt a single bit in the bitmap area of the newest slot's active bitmap.
fn corrupt_bitmap_bit(img: &PathBuf, bit_offset: u64) {
    use cownfs_core::block::{BlockDevice, FileDevice};
    use cownfs_core::superblock;

    let dev = FileDevice::open(img).unwrap();
    let (sb, _) = superblock::open(&dev).unwrap();
    let bitmap_start = sb.bitmap_start + sb.bitmap_base_area * sb.bitmap_blocks;
    let block_idx = bit_offset / 4096 / 8;
    let byte_in_block = (bit_offset / 8) % 4096;
    let bit_in_byte = bit_offset % 8;

    let mut blk = [0u8; 4096];
    dev.read_block(bitmap_start + block_idx, &mut blk).unwrap();
    blk[byte_in_block as usize] ^= 1 << bit_in_byte;
    dev.write_block(bitmap_start + block_idx, &blk).unwrap();
    dev.sync().unwrap();
}

/// Test 1: Corrupt the newest bitmap, verify fallback to older generation.
#[test]
fn bitmap_corrupt_falls_back_to_older_generation() {
    let img = test_img("fallback");
    {
        let mut fs = Fs::format(&img, 4096).unwrap();
        // Do a commit to create a second generation.
        fs.commit().unwrap();
        // Write some data to ensure the bitmap changes.
        let ino = fs.create(1, b"testfile", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, b"hello world").unwrap();
        fs.commit().unwrap();
    }
    // Corrupt a bit in the newest bitmap.
    corrupt_bitmap_bit(&img, 100);

    // Open should fall back to the older generation.
    let fs = Fs::open(&img).expect("open should fall back to older generation");
    // Verify the filesystem is consistent.
    fs.check()
        .expect("check should pass on fallback generation");
    let _ = std::fs::remove_file(&img);
}

/// Test 2: Corrupt both bitmap areas, verify open fails with BitmapCorrupt.
#[test]
fn bitmap_corrupt_both_areas_fails() {
    let img = test_img("both-corrupt");
    {
        let mut fs = Fs::format(&img, 4096).unwrap();
        fs.commit().unwrap();
    }
    // Corrupt bits in both bitmap areas.
    // Area 0 starts at block 2, area 1 at block 2 + bblocks.
    use cownfs_core::block::{BlockDevice, FileDevice};
    use cownfs_core::superblock;

    let dev = FileDevice::open(&img).unwrap();
    let (sb, _) = superblock::open(&dev).unwrap();
    let bblocks = sb.bitmap_blocks;
    for area in 0..2 {
        let bitmap_start = sb.bitmap_start + area * bblocks;
        let mut blk = [0u8; 4096];
        dev.read_block(bitmap_start, &mut blk).unwrap();
        blk[0] ^= 0xFF; // Flip all bits in first byte
        dev.write_block(bitmap_start, &blk).unwrap();
    }
    dev.sync().unwrap();
    drop(dev);

    // Open should fail with BitmapCorrupt (both slots' bitmaps are bad).
    match Fs::open(&img) {
        Err(FsError::BitmapCorrupt) => {} // expected
        Err(e) => panic!("expected BitmapCorrupt, got: {e:?}"),
        Ok(_) => panic!("expected BitmapCorrupt, got Ok"),
    }
    let _ = std::fs::remove_file(&img);
}

/// Test 3: Fresh format reopens immediately without another checkpoint.
#[test]
fn fresh_format_reopens_immediately() {
    let img = test_img("fresh");
    {
        let _fs = Fs::format(&img, 4096).unwrap();
        // No commit, drop immediately.
    }
    // Should open without needing another commit.
    let fs = Fs::open(&img).expect("fresh format should reopen immediately");
    fs.check().expect("check should pass on fresh format");
    let _ = std::fs::remove_file(&img);
}

/// Test 4: Legacy v3 image (without CRC magic) opens and allows writes.
/// Creates a v3-style image by formatting then zeroing the sidecar magic.
#[test]
fn legacy_v3_image_opens_and_writes() {
    let img = test_img("legacy");
    {
        let _fs = Fs::format(&img, 4096).unwrap();
    }
    // Zero the sidecar magic to simulate a v3 image.
    use cownfs_core::block::{BlockDevice, FileDevice};
    use cownfs_core::superblock;

    let dev = FileDevice::open(&img).unwrap();
    // Read superblock to find bitmap layout.
    let (sb, _) = superblock::open(&dev).unwrap();
    let bblocks = sb.bitmap_blocks;
    let cb = superblock::bitmap_crc_blocks(bblocks);
    let sidecar_start = sb.bitmap_start + 2 * bblocks;
    // Zero both sidecar areas.
    let zero = [0u8; 4096];
    for i in 0..(2 * cb) {
        dev.write_block(sidecar_start + i, &zero).unwrap();
    }
    dev.sync().unwrap();
    drop(dev);

    // Open should succeed (legacy mode, no CRC verification).
    let mut fs = Fs::open(&img).expect("legacy v3 should open");
    // Should allow writes.
    let ino = fs.create(1, b"legacyfile", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"legacy data").unwrap();
    fs.commit().unwrap();
    // Verify the data.
    let data = fs.read(ino, 0, 11).unwrap();
    assert_eq!(data, b"legacy data");
    let _ = std::fs::remove_file(&img);
}

/// Test 5: Sidecars spanning multiple blocks (large image).
/// Uses a large image where bblocks > 1, ensuring the sidecar spans multiple blocks.
/// Verifies that CRCs are written and verified correctly for multi-block bitmaps.
#[test]
fn sidecar_spans_multiple_blocks() {
    // 100K blocks: bitmap bits = 100K, bytes = 12500, blocks = 4 (12500/4096=3.05).
    let img = test_img("multiblock");
    let blocks = 100_000u64;
    {
        let mut fs = Fs::format(&img, blocks).unwrap();
        fs.commit().unwrap();
        // Write data to dirty the bitmap.
        let ino = fs.create(1, b"bigfile", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, b"x".repeat(10000).as_slice()).unwrap();
        fs.commit().unwrap();
    }
    // Verify it opens and the bitmap is consistent.
    // This exercises the multi-block sidecar read path in verify_bitmap_crcs.
    let fs = Fs::open(&img).expect("multi-block sidecar should open");
    fs.check().expect("check should pass");
    let _ = std::fs::remove_file(&img);
}
