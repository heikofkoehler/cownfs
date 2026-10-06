//! Targeted test for the "stale NodeId generation" bug.
//!
//! Reproduces the issue from stress_mixed_high_contention without the NFS
//! layer: concurrent threads hammer the Fs with create/write/truncate/unlink,
//! churning B-tree nodes through free/reallocate cycles.
//!
//! The bug: a B-tree holds a NodeId (block, gen) after the block is freed
//! and reallocated with a new generation, causing "stale NodeId generation"
//! Corrupt errors.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::{Arc, Barrier};

fn test_fs(blocks: u64) -> (Fs, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "cownfs-stale-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let fs = Fs::format(&path, blocks).unwrap();
    (fs, path)
}

#[test]
fn stale_gen_concurrent_truncate() {
    let (fs, path) = test_fs(32768);
    let fs = Arc::new(std::sync::RwLock::new(fs));
    let barrier = Arc::new(Barrier::new(16));

    let mut handles = vec![];
    for i in 0..16 {
        let fs = Arc::clone(&fs);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for j in 0..20 {
                let name = format!("f-{i}-{j}");
                // Create, write, truncate, unlink — churns extent B-tree.
                let ino = {
                    let mut fs = fs.write().unwrap();
                    let ino = fs.create(ROOT_INO, name.as_bytes(), 0o644, 0, 0).unwrap();
                    fs.write(ino, 0, &vec![0xABu8; 1024]).unwrap();
                    ino
                };
                {
                    let mut fs = fs.write().unwrap();
                    // Truncate via setattr (like the NFS test).
                    fs.setattr(
                        ino,
                        &cownfs_core::engine::SetAttrs {
                            size: Some(512),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                {
                    let mut fs = fs.write().unwrap();
                    fs.unlink(ROOT_INO, name.as_bytes()).unwrap();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    std::fs::remove_file(&path).ok();
}
