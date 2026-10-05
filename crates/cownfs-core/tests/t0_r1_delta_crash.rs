//! T0-R1: repro for the delta-bitmap crash-consistency bug.
//!
//! The delta bitmap is written to area (1 - base_area) — the same area the
//! currently-committed superblock's delta lives in. If the process crashes
//! between persist_bitmap_delta and the superblock flip, apply_delta sees a
//! gen mismatch and silently ignores the delta, loading a stale base bitmap.
//! Blocks allocated since the checkpoint then look free.
//!
//! This test MUST FAIL on unfixed main (check() reports unreachable blocks).
//! It uses a 1GB image so bitmap deltas actually fit (on small images every
//! commit is forced to be a full checkpoint, hiding the bug).

use cownfs_core::engine::{FaultPoint, Fs, ROOT_INO};

/// 1GB image → bitmap_blocks=8, so deltas fit (delta_blocks <= bitmap_blocks).
const BLOCKS: u64 = 262144;

#[test]
fn r1_delta_overwrite_crash_is_consistent() {
    let img = std::env::temp_dir().join("t0-r1-delta-crash.img");
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, BLOCKS).unwrap();
    fs.commit().unwrap();

    // Gen 2: file with data, committed. Writes delta D_2.
    let ino = fs.create(ROOT_INO, b"f1", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![0xabu8; 8192]).unwrap();
    fs.commit().unwrap();

    // Gen 3 (faulted): D_3 overwrites D_2 in the delta area, then "crash"
    // before the superblock flip. On-disk SB still says delta_gen=2.
    let ino2 = fs.create(ROOT_INO, b"f2", 0o644, 0, 0).unwrap();
    fs.write(ino2, 0, &vec![0xcdu8; 8192]).unwrap();
    fs.set_fault_point(FaultPoint::AfterBitmap);
    let _ = fs.commit(); // Err(InjectedFault), expected.
    drop(fs); // simulated kill -9

    // Recovery must be consistent: the committed gen-2 trees reference
    // blocks that the loaded bitmap must mark allocated.
    let fs = Fs::open(&img).unwrap();
    fs.check()
        .expect("R1: bitmap must be consistent after crash");
    // Gen-2 data must still read back.
    let (ino, _) = fs.lookup(ROOT_INO, b"f1").unwrap().unwrap();
    let data = fs.read(ino, 0, 8192).unwrap();
    assert!(data.iter().all(|&b| b == 0xab), "R1: data corrupted");

    let _ = std::fs::remove_file(&img);
}
