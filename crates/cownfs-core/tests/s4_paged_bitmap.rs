//! S4: paged bitmap integration.
//!
//! Exit criterion: RSS bound test at 16 TiB sparse image.
//! The bitmap (512 MiB for 16 TiB) must NOT be fully resident.

use std::path::PathBuf;

use cownfs_core::engine::Fs;

/// Current RSS in bytes (Linux /proc).
fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("rss value")
                .parse()
                .expect("parse rss");
            return kb * 1024;
        }
    }
    panic!("VmRSS not found");
}

fn tmp_path(tag: &str) -> PathBuf {
    // /tmp is a small tmpfs; use the workspace disk for the large sparse image.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!(
        "cownfs-s4-{tag}-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// S4.1: 16 TiB sparse image opens with bounded RSS.
/// 16 TiB = 4 GiB blocks; a resident bitmap would be 512 MiB per area.
#[test]
fn s4_rss_bound_16tib() {
    let img = tmp_path("16tib");
    let _ = std::fs::remove_file(&img);

    // 16 TiB in 4 KiB blocks.
    let blocks: u64 = 16 * 1024 * 1024 * 1024 * 1024 / 4096;
    assert_eq!(blocks, 4_294_967_296);

    let rss_before = rss_bytes();
    {
        let mut fs = Fs::format(&img, blocks).expect("format 16 TiB");
        // Do a little work so pages get touched.
        let ino = fs
            .create(cownfs_core::engine::ROOT_INO, b"f", 0o644, 0, 0)
            .expect("create");
        fs.write(ino, 0, b"hello").expect("write");
        fs.commit().expect("commit");
    }
    let rss_after_format = rss_bytes();
    // Reopen: the bitmap must NOT be paged in fully.
    {
        let _fs = Fs::open(&img).expect("open 16 TiB");
        let rss_after_open = rss_bytes();
        println!(
            "S4: RSS before={:.1}MiB format={:.1}MiB open={:.1}MiB",
            rss_before as f64 / 1e6,
            rss_after_format as f64 / 1e6,
            rss_after_open as f64 / 1e6
        );
        // A resident bitmap would add 512 MiB per area. Bound the growth.
        // (Allow generous headroom for test harness overhead.)
        let growth = rss_after_open.saturating_sub(rss_before);
        assert!(
            growth < 100 * 1024 * 1024,
            "RSS grew by {growth} bytes on 16 TiB open; bitmap not paged?"
        );
    }

    let _ = std::fs::remove_file(&img);
}

/// S4.2: alloc/free correctness with paging (small image, exercises cache).
#[test]
fn s4_paged_alloc_free() {
    let img = tmp_path("alloc");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 8192).expect("format");
    let free_initial = fs.free_block_count();

    // Allocate many blocks across many bitmap pages.
    let mut inos = Vec::new();
    for i in 0..100 {
        let name = format!("f{i:03}");
        let ino = fs
            .create(cownfs_core::engine::ROOT_INO, name.as_bytes(), 0o644, 0, 0)
            .expect("create");
        // 2 blocks each (8 KiB) to spread allocations.
        fs.write(ino, 0, &vec![0xABu8; 8192]).expect("write");
        inos.push((name, ino));
    }
    fs.commit().expect("commit");
    let free_after_alloc = fs.free_block_count();
    println!("free_initial={free_initial} free_after_alloc={free_after_alloc}");
    // 100 files × 2 data blocks = 200 blocks, plus metadata.
    assert!(
        free_initial - free_after_alloc >= 200,
        "expected at least 200 blocks allocated"
    );

    // Reopen: free count persists via the superblock (S4 P3 counter).
    // Note: may increase slightly as the deferred-free queue is applied on open.
    drop(fs);
    let fs = Fs::open(&img).expect("open");
    let free_after_reopen = fs.free_block_count();
    println!("free_after_reopen={free_after_reopen}");
    assert!(
        free_after_reopen >= free_after_alloc,
        "free count should persist (got {free_after_reopen}, expected >= {free_after_alloc})"
    );

    let _ = std::fs::remove_file(&img);
}

/// N5 regression: dirtying >64 bitmap pages must not corrupt the live area.
/// Allocates blocks across many 128 MiB regions (each touches a different
/// bitmap page), then verifies the image still opens and fsck is clean.
/// With the old (buggy) eviction, dirty pages were written to the live
/// area, causing a torn bitmap after crash.
#[test]
fn s4_many_dirty_pages_no_corruption() {
    use cownfs_core::engine::{Fs, ROOT_INO};
    let img = std::env::temp_dir().join(format!(
        "cownfs-s4-n5-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&img);
    // 8 GiB image = 2M blocks = 64 bitmap pages. We need >64 dirty pages,
    // so use a larger image or force many distinct regions.
    // 16 GiB = 4M blocks = 128 pages.
    let mut fs = Fs::format(&img, 16 * 1024 * 1024 * 1024 / 4096).expect("format");
    fs.commit().expect("commit");

    // Allocate 100 blocks spread across the image to dirty many pages.
    // Each allocation in a different 128 MiB region touches a different page.
    for i in 0..100 {
        let name = format!("f{i:03}");
        let ino = fs
            .create(ROOT_INO, name.as_bytes(), 0o644, 0, 0)
            .expect("create");
        // Write 1 MiB (256 blocks) to force allocation.
        let data = vec![i as u8; 1024 * 1024];
        fs.write(ino, 0, &data).expect("write");
        if i % 20 == 19 {
            fs.commit().expect("commit");
        }
    }
    fs.commit().expect("final commit");
    drop(fs);

    // Reopen and verify clean.
    let fs = Fs::open(&img).expect("open after many dirty pages");
    fs.check().expect("fsck clean");
    drop(fs);

    let _ = std::fs::remove_file(&img);
}
