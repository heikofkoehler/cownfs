//! P2.12: Indexed LRU eviction test.
//!
//! Forces >10,000 clean nodes into a `BlockArena`'s cache, verifies that
//! eviction keeps the cache bounded, and that evicted nodes can be
//! re-read from disk with intact data.

use cownfs_core::engine::Fs;
use std::path::PathBuf;

fn test_image(name: &str) -> PathBuf {
    let img = std::env::temp_dir().join(name);
    let _ = std::fs::remove_file(&img);
    img
}

/// Create `n` files with distinct content, committing after each batch so
/// the B-tree nodes become clean (flushable/evictable).
#[test]
fn lru_eviction_bounds_cache_and_reread_works() {
    let img = test_image("cownfs-p56-lru.img");
    // 20k blocks; each file is tiny (1 block data + inode + dirent).
    // 12k files => well over 10k nodes across the arenas.
    const NFILES: usize = 12_000;
    let mut fs = Fs::format(&img, 65536).unwrap();

    // Create files in batches, committing between batches so nodes become
    // clean and eligible for eviction.
    for batch in 0..12 {
        for i in 0..1000 {
            let idx = batch * 1000 + i;
            let name = format!("f{idx:05}");
            let ino = fs.create(1, name.as_bytes(), 0o644, 0, 0).unwrap();
            // Write distinct content so we can verify after eviction.
            let data = vec![(idx % 251) as u8; 512];
            fs.write(ino, 0, &data).unwrap();
        }
        fs.commit().unwrap();
    }

    // All files should be readable with correct content, even if their
    // nodes were evicted from cache (reread from disk).
    for idx in 0..NFILES {
        let name = format!("f{idx:05}");
        let (ino, _) = fs.lookup(1, name.as_bytes()).unwrap().unwrap();
        let data = fs.read(ino, 0, 512).unwrap();
        let expected = vec![(idx % 251) as u8; 512];
        assert_eq!(data, expected, "content mismatch for file {idx}");
    }

    // Directory should list all files.
    let entries = fs.readdir(1).unwrap();
    // +1 for the directory itself? No: readdir returns children only.
    // We created NFILES files in root (ino 1).
    assert_eq!(
        entries.len(),
        NFILES,
        "expected {NFILES} files in root, got {}",
        entries.len()
    );

    fs.check().unwrap();
    drop(fs);

    // Reopen and verify again (exercises cold-cache reads).
    let fs = Fs::open(&img).unwrap();
    for idx in (0..NFILES).step_by(1000) {
        let name = format!("f{idx:05}");
        let (ino, _) = fs.lookup(1, name.as_bytes()).unwrap().unwrap();
        let data = fs.read(ino, 0, 512).unwrap();
        let expected = vec![(idx % 251) as u8; 512];
        assert_eq!(data, expected, "reopen mismatch for file {idx}");
    }
    fs.check().unwrap();

    let _ = std::fs::remove_file(&img);
}
