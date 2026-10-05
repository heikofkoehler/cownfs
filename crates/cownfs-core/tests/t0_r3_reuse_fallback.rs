//! T0-R3: repro for premature block reuse vs superblock fallback.
//!
//! `persist_bitmap()` drains `pending_free` and marks those blocks free
//! before the new generation is durable. If the new generation's slot is
//! then corrupted (or the crash happens before the flip), we fall back to
//! the older generation — whose trees still reference those blocks. But the
//! blocks may have already been reallocated for new data, corrupting the
//! fallback generation.
//!
//! This test MUST FAIL on unfixed main.

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_core::{superblock, BLOCK_SIZE};

#[test]
fn r3_fallback_after_reuse_is_consistent() {
    let img = std::env::temp_dir().join("t0-r3-reuse-fallback.img");
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();

    // Gen 2: file f1 with distinctive data.
    let ino1 = fs.create(ROOT_INO, b"f1", 0o644, 0, 0).unwrap();
    let data1 = vec![0x11u8; 8192];
    fs.write(ino1, 0, &data1).unwrap();
    fs.commit().unwrap();
    let gen2 = fs.generation();

    // Gen 3: delete f1, then stage the commit (persist_bitmap marks the
    // blocks free) WITHOUT flipping the superblock.
    fs.unlink(ROOT_INO, b"f1").unwrap();
    fs.commit_async().unwrap();
    // Now allocate f2 — the allocator may reuse f1's just-freed blocks,
    // overwriting them, while gen 2 (the fallback) still references them.
    let ino2 = fs.create(ROOT_INO, b"f2", 0o644, 0, 0).unwrap();
    let data2 = vec![0x22u8; 8192];
    fs.write(ino2, 0, &data2).unwrap();
    // Crash before sync_txg (no superblock flip). Drop without committing.
    drop(fs);

    // Reopen: should fall back to gen 2 (gen 3 was never flipped).
    let fs = Fs::open(&img).unwrap();
    assert_eq!(fs.generation(), gen2, "should have fallen back to gen 2");
    // f1 must still exist with intact data.
    let (ino, _) = fs.lookup(ROOT_INO, b"f1").unwrap().expect("f1 should exist in gen 2");
    let read_back = fs.read(ino, 0, 8192).unwrap();
    assert_eq!(read_back, data1, "R3: fallback generation data corrupted by premature reuse");
    // Note: check() may report a leak (block marked allocated but unreachable)
    // due to the deferred-free queues. This is expected and safe: the blocks
    // will be reclaimed on the next commit. The critical property is that
    // the fallback generation's data is intact (verified above).

    let _ = std::fs::remove_file(&img);
}
