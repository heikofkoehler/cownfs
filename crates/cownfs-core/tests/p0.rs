//! P0 tests: block device, bitmap, and ping-pong superblock.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use cownfs_core::bitmap::Bitmap;
use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::superblock;
use cownfs_core::BLOCK_SIZE;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_image(blocks: u64) -> (PathBuf, FileDevice) {
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "cownfs-p0-{}-{}.img",
        std::process::id(),
        n
    ));
    let dev = FileDevice::create(&path, blocks).expect("create image");
    (path, dev)
}

/// Overwrite `len` bytes at `offset` in the image file with garbage.
fn corrupt(path: &PathBuf, offset: u64, len: usize) {
    let mut f = OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&vec![0xA5; len]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn format_open_roundtrip() {
    let (_path, mut dev) = tmp_image(1024);
    let sb = { let n = dev.block_count(); superblock::format(&mut dev, n) }.unwrap();
    assert_eq!(sb.generation, 1);
    assert_eq!(sb.block_count, 1024);
    let (sb2, active) = superblock::open(&dev).unwrap();
    assert_eq!(sb2.generation, 1);
    assert_eq!(sb2.uuid, sb.uuid);
    assert_eq!(active, 0); // tie goes to slot 0
}

#[test]
fn bit_flip_falls_back_to_other_slot() {
    let (path, mut dev) = tmp_image(1024);
    { let n = dev.block_count(); superblock::format(&mut dev, n) }.unwrap();
    drop(dev);

    // Corrupt slot 0's magic; slot 1 must keep the fs mountable.
    corrupt(&path, 0, 8);
    let dev = FileDevice::open(&path).unwrap();
    let (sb, active) = superblock::open(&dev).expect("should fall back to slot 1");
    assert_eq!(active, 1);
    assert_eq!(sb.generation, 1);
}

#[test]
fn both_slots_corrupt_fails() {
    let (path, mut dev) = tmp_image(1024);
    { let n = dev.block_count(); superblock::format(&mut dev, n) }.unwrap();
    drop(dev);

    corrupt(&path, 0, 8);
    corrupt(&path, BLOCK_SIZE as u64, 8);
    let dev = FileDevice::open(&path).unwrap();
    assert!(superblock::open(&dev).is_err());
}

#[test]
fn commit_generation_ping_pongs() {
    let (_path, mut dev) = tmp_image(1024);
    { let n = dev.block_count(); superblock::format(&mut dev, n) }.unwrap();
    let (mut sb, mut active) = superblock::open(&dev).unwrap();
    assert_eq!((sb.generation, active), (1, 0));

    superblock::commit_generation(&mut dev, &mut sb, &mut active).unwrap();
    assert_eq!((sb.generation, active), (2, 1));

    // Re-open from disk: the new generation must win.
    let (sb2, active2) = superblock::open(&dev).unwrap();
    assert_eq!((sb2.generation, active2), (2, 1));

    superblock::commit_generation(&mut dev, &mut sb, &mut active).unwrap();
    let (sb3, active3) = superblock::open(&dev).unwrap();
    assert_eq!((sb3.generation, active3), (3, 0));
}

#[test]
fn block_read_write_roundtrip() {
    let (_path, mut dev) = tmp_image(64);
    let mut w = [0u8; BLOCK_SIZE];
    for (i, b) in w.iter_mut().enumerate() {
        *b = (i * 7 % 251) as u8;
    }
    dev.write_block(63, &w).unwrap();
    dev.sync().unwrap();
    let mut r = [0u8; BLOCK_SIZE];
    dev.read_block(63, &mut r).unwrap();
    assert_eq!(w, r);

    assert!(dev.read_block(64, &mut r).is_err());
}

#[test]
fn bitmap_alloc_and_exhaustion() {
    let mut map = Bitmap::new(100);
    // Reserve everything except blocks 7 and 42.
    for i in 0..100 {
        if i != 7 && i != 42 {
            map.set(i);
        }
    }
    let a = map.alloc().unwrap();
    let b = map.alloc().unwrap();
    assert!(a == 7 && b == 42 || a == 42 && b == 7);
    assert!(map.alloc().is_none());
    assert!(map.test(a) && map.test(b));

    // Serialization roundtrip preserves every bit.
    let bytes = map.to_bytes();
    let map2 = Bitmap::from_bytes(100, &bytes);
    for i in 0..100 {
        assert!(map2.test(i), "bit {i} lost in roundtrip");
    }
}

#[test]
fn format_rejects_tiny_images() {
    let (_path, mut dev) = tmp_image(8);
    assert!({ let n = dev.block_count(); superblock::format(&mut dev, n) }.is_err());
}
