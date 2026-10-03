//! P0.1/B2: full bitmap block CRCs + older-generation fallback.
//!
//! - Corrupt a base bitmap block → `Fs::open` must detect the CRC
//!   mismatch and fall back to the older superblock generation.
//! - The fallback generation must be consistent (`Fs::check()` passes).

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_core::{superblock, BLOCK_SIZE};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-bmapcrc-{n}.img"));
    let _ = std::fs::remove_file(&img);
    img
}

#[test]
fn corrupt_base_bitmap_falls_back_to_older_generation() {
    let img = test_image();
    // Small image: 4096 blocks → 64 bitmap words; dirtying >6 words
    // (>10%) forces a checkpoint.
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap(); // gen 2, checkpoint (full bitmap + CRCs)
    let gen_after_first = fs.generation();

    // Force a second checkpoint with a different base area: allocate
    // enough blocks to dirty >10% of bitmap words (64 words → need >6;
    // 700 contiguous blocks ≈ 11 words).
    let ino = fs.create(ROOT_INO, b"big", 0o644, 0, 0).unwrap();
    let chunk = vec![0xabu8; BLOCK_SIZE];
    for i in 0..700 {
        fs.write(ino, (i * BLOCK_SIZE) as u64, &chunk).unwrap();
    }
    fs.commit().unwrap(); // gen 3, checkpoint → base area flips
    let gen_newest = fs.generation();
    assert!(gen_newest > gen_after_first);

    // Confirm the two newest generations use different base areas.
    let mut dev = FileDevice::open(&img).unwrap();
    let (sb, _) = superblock::open(&dev).unwrap();
    assert_eq!(sb.generation, gen_newest);
    assert_eq!(
        sb.bitmap_full_gen, gen_newest,
        "second commit should have checkpointed"
    );
    let newest_base = sb.bitmap_base_area;
    // Find the older slot's base area by flipping: after a checkpoint the
    // base area always alternates, so the previous generation used the
    // other area.
    let prev_base = 1 - newest_base;
    let base_start = sb.bitmap_start + newest_base * sb.bitmap_blocks;
    assert!(sb.bitmap_blocks > 0);

    // Corrupt the first block of the newest base bitmap area.
    let mut blk = [0u8; BLOCK_SIZE];
    dev.read_block(base_start, &mut blk).unwrap();
    blk[100] ^= 0xff;
    dev.write_block(base_start, &blk).unwrap();
    dev.sync().unwrap();
    drop(dev);
    drop(fs);

    // Reopen: CRC mismatch on the newest generation's base bitmap →
    // must fall back to the older generation (which uses `prev_base`,
    // untouched).
    let fs = Fs::open(&img).unwrap();
    assert_eq!(
        fs.generation(),
        gen_after_first,
        "should have fallen back to the older generation"
    );
    // The fallback generation must be fully consistent.
    fs.check().unwrap();
    // The file from the corrupt generation is gone (older state), but
    // the root directory from the older generation is intact.
    assert!(fs.lookup(ROOT_INO, b"big").unwrap().is_none());

    let _ = std::fs::remove_file(&img);
    let _ = prev_base; // documented above
}

#[test]
fn corrupt_both_base_areas_fails_open() {
    let img = test_image();
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    let ino = fs.create(ROOT_INO, b"big", 0o644, 0, 0).unwrap();
    let chunk = vec![0xabu8; BLOCK_SIZE];
    for i in 0..700 {
        fs.write(ino, (i * BLOCK_SIZE) as u64, &chunk).unwrap();
    }
    fs.commit().unwrap();
    drop(fs);

    let mut dev = FileDevice::open(&img).unwrap();
    let (sb, _) = superblock::open(&dev).unwrap();
    // Corrupt the first block of BOTH bitmap areas.
    for area in 0..2 {
        let base_start = sb.bitmap_start + area * sb.bitmap_blocks;
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(base_start, &mut blk).unwrap();
        blk[100] ^= 0xff;
        dev.write_block(base_start, &blk).unwrap();
    }
    dev.sync().unwrap();
    drop(dev);

    // Both generations' bitmaps are corrupt → open must fail loudly,
    // not mount a corrupt bitmap.
    match Fs::open(&img) {
        Err(cownfs_core::engine::FsError::BitmapCorrupt) => {}
        Err(e) => panic!("expected BitmapCorrupt, got {e}"),
        Ok(_) => panic!("expected BitmapCorrupt, open succeeded"),
    }
    let _ = std::fs::remove_file(&img);
}
