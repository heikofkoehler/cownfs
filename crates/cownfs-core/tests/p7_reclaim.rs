//! P7 gate: fsck --reclaim reclaims unreachable allocated blocks.

use cownfs_core::engine::{Fs, ROOT_INO};

#[test]
fn p7_reclaim() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-p7-reclaim-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 8192).unwrap();
    // Create a file so we have some allocated blocks.
    let ino = fs.create(ROOT_INO, b"file.txt", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"hello world").unwrap();
    fs.commit().unwrap();

    // Clean filesystem: reclaim should find nothing.
    let n = fs.reclaim_unreachable().unwrap();
    assert_eq!(n, 0, "clean fs should have nothing to reclaim");

    // Simulate a lost free: forcibly set bits for blocks that are
    // definitely not reachable (high block numbers).
    // First, find a free block by checking what's allocated.
    let report = fs.check().unwrap();
    let total = report.allocated_blocks;
    // Use blocks beyond the current allocation (but within the image).
    // The image has 8192 blocks; we'll use high numbers that are free.
    fs.debug_set_bitmap_bit(8000);
    fs.debug_set_bitmap_bit(8001);
    fs.debug_set_bitmap_bit(8002);
    fs.commit().unwrap();

    // Now check() should fail (unreachable allocated blocks).
    assert!(fs.check().is_err(), "check should fail with leaked blocks");

    // Reclaim should free the 3 blocks.
    let n = fs.reclaim_unreachable().unwrap();
    assert_eq!(n, 3, "should reclaim 3 blocks, got {n}");
    fs.commit().unwrap();

    // Now check() should pass.
    fs.check().expect("check should pass after reclaim");
    let report2 = fs.check().unwrap();
    assert_eq!(
        report2.allocated_blocks, total,
        "allocated count should be back to {total}"
    );

    println!("Reclaim test: freed 3 blocks, check clean");
    drop(fs);
    std::fs::remove_file(&img).unwrap();
}
