//! T2: Model-based test for Fs operations.
//!
//! Maintains an in-memory HashMap model (filename -> content) alongside
//! the Fs. Performs random create/write/delete/commit/reopen operations,
//! then verifies the Fs state matches the model.
//!
//! This is a simplified T2 (no proptest/shrinking yet); it uses a
//! deterministic xorshift PRNG for reproducibility.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::collections::HashMap;

struct Prng {
    state: u64,
}

impl Prng {
    fn new(seed: u64) -> Self {
        Prng { state: seed }
    }
    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn t2_model_fs_vs_hashmap() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-t2-model-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 1024).unwrap();
    let mut model: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut rng = Prng::new(0xDEADBEEF);

    // Track inodes for model entries.
    let mut inodes: HashMap<Vec<u8>, u64> = HashMap::new();

    for iter in 0..200 {
        let op = rng.below(5);
        match op {
            // 0: create file
            0 => {
                let name = format!("f{}", rng.below(20)).into_bytes();
                if !model.contains_key(&name) {
                    let ino = fs.create(ROOT_INO, &name, 0o644, 0, 0).unwrap();
                    model.insert(name.clone(), Vec::new());
                    inodes.insert(name, ino);
                }
            }
            // 1: write to random file
            1 => {
                if !model.is_empty() {
                    let idx = rng.below(model.len() as u64) as usize;
                    let name = model.keys().nth(idx).unwrap().clone();
                    let data = vec![(rng.below(256) as u8); 100];
                    let ino = inodes[&name];
                    fs.write(ino, 0, &data).unwrap();
                    model.insert(name, data);
                }
            }
            // 2: delete random file
            2 => {
                if !model.is_empty() {
                    let idx = rng.below(model.len() as u64) as usize;
                    let name = model.keys().nth(idx).unwrap().clone();
                    fs.unlink(ROOT_INO, &name).unwrap();
                    model.remove(&name);
                    inodes.remove(&name);
                }
            }
            // 3: commit
            3 => {
                fs.commit().unwrap();
            }
            // 4: reopen (drop and reopen Fs) — only if last op was commit
            // (uncommitted data is lost on reopen, which would desync the model).
            4 => {
                // Commit first to ensure model and Fs are in sync.
                fs.commit().unwrap();
                drop(fs);
                fs = Fs::open(&img).unwrap();
                // Rebuild inode map by readdir.
                inodes.clear();
                for (name, ino, _) in fs.readdir(ROOT_INO).unwrap() {
                    if model.contains_key(&name) {
                        inodes.insert(name, ino);
                    }
                }
                // Verify model matches Fs after reopen.
                for (name, expected) in &model {
                    // Skip if not in inodes (shouldn't happen after commit).
                    if let Some(&ino) = inodes.get(name) {
                        let actual = fs.read(ino, 0, 10000).unwrap();
                        assert_eq!(
                            &actual, expected,
                            "T2: model mismatch after reopen for {name:?}"
                        );
                    }
                }
            }
            _ => unreachable!(),
        }

        // Periodic verification: every 50 iters, check model vs Fs.
        if iter % 50 == 49 {
            for (name, expected) in &model {
                let ino = inodes[name];
                let actual = fs.read(ino, 0, 10000).unwrap();
                assert_eq!(
                    &actual, expected,
                    "T2: model mismatch for {name:?} at iter {iter}"
                );
            }
            // Check that Fs doesn't have extra files.
            let entries = fs.readdir(ROOT_INO).unwrap();
            assert_eq!(
                entries.len(),
                model.len(),
                "T2: file count mismatch at iter {iter}"
            );
        }
    }

    // Final verification.
    fs.commit().unwrap();
    for (name, expected) in &model {
        let ino = inodes[name];
        let actual = fs.read(ino, 0, 10000).unwrap();
        assert_eq!(&actual, expected, "T2: final mismatch for {name:?}");
    }

    drop(fs);
    let _ = std::fs::remove_file(&img);
}
