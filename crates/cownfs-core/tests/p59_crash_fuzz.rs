//! P59: systematic crash-consistency fuzzer.
//!
//! Device for finding data correctness issues:
//! - Randomized workload: creates, writes, deletes, truncates across many files
//! - Fault injected at a RANDOM point in the commit path (AfterFlush,
//!   AfterBitmap, AfterSync) — simulates crash at the worst moment
//! - After "crash", verify:
//!   1. Fs::open succeeds (no panic, no corruption)
//!   2. fs.check() passes (on-disk consistency)
//!   3. Every file reads back without checksum errors
//!
//! Each iteration uses a deterministic PRNG seed so failures are reproducible.
//! Run with many seeds to cover the state space.

use cownfs_core::engine::{FaultPoint, Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

/// Simple xorshift64 PRNG for deterministic test workloads.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-crashfuzz-{n}.img"));
    let _ = std::fs::remove_file(&img);
    img
}

/// Run one fuzz iteration with the given seed.
/// Returns the seed on failure (for reproduction).
fn fuzz_one(seed: u64) {
    let img = test_image();
    let mut rng = Rng(seed);

    // Format.
    let mut fs = Fs::format(&img, 16384).unwrap();
    fs.commit().unwrap();

    // Track live files: (name, ino, expected content length)
    let mut files: Vec<(Vec<u8>, u64)> = Vec::new();

    // Phase 1: build up state with committed operations.
    for i in 0..20u64 {
        let name = format!("f{i:03}");
        let ino = fs.create(ROOT_INO, name.as_bytes(), 0o644, 0, 0).unwrap();
        // Write random-sized data.
        let len = (rng.below(8) + 1) * 1024; // 1K..8K
        let data: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        fs.write(ino, 0, &data).unwrap();
        files.push((name.into_bytes(), ino));
        if i % 5 == 4 {
            fs.commit().unwrap();
        }
    }
    fs.commit().unwrap();

    // Phase 2: random uncommitted operations.
    for _ in 0..30 {
        match rng.below(4) {
            0 => {
                // Create
                let name = format!("n{:03}_{}", files.len(), rng.below(1000));
                if let Ok(ino) = fs.create(ROOT_INO, name.as_bytes(), 0o644, 0, 0) {
                    let len = (rng.below(4) + 1) * 512;
                    let data: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
                    let _ = fs.write(ino, 0, &data);
                    files.push((name.into_bytes(), ino));
                }
            }
            1 => {
                // Overwrite random file
                if !files.is_empty() {
                    let idx = rng.below(files.len() as u64) as usize;
                    let ino = files[idx].1;
                    let off = rng.below(4096);
                    let len = (rng.below(4) + 1) * 256;
                    let data: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
                    let _ = fs.write(ino, off, &data);
                }
            }
            2 => {
                // Delete random file
                if !files.is_empty() {
                    let idx = rng.below(files.len() as u64) as usize;
                    let (name, _) = files.remove(idx);
                    let _ = fs.unlink(ROOT_INO, &name);
                }
            }
            _ => {
                // Truncate random file
                if !files.is_empty() {
                    let idx = rng.below(files.len() as u64) as usize;
                    let ino = files[idx].1;
                    let new_len = rng.below(8192);
                    let _ = fs.truncate(ino, new_len);
                }
            }
        }
    }

    // Phase 3: inject fault at random commit point (simulated crash).
    let fault = match rng.below(3) {
        0 => FaultPoint::AfterFlush,
        1 => FaultPoint::AfterBitmap,
        _ => FaultPoint::AfterSync,
    };
    fs.set_fault_point(fault);
    let _ = fs.commit(); // Expected to fail with InjectedFault.
    drop(fs); // Simulate kill -9: no cleanup.

    // Phase 4: recovery verification.
    // 1. Open must succeed (fallback to older generation if needed).
    let fs = Fs::open(&img).unwrap_or_else(|e| {
        panic!("seed {seed}: Fs::open failed after fault {fault:?}: {e:?}");
    });
    // 2. fsck must pass.
    fs.check().unwrap_or_else(|e| {
        panic!("seed {seed}: fs.check() failed after fault {fault:?}: {e:?}");
    });
    // 3. Every surviving file must read without checksum errors.
    // (We don't check exact content — uncommitted writes may be lost —
    // but reads must not return Corrupt errors.)
    for (name, ino) in &files {
        if let Some((found_ino, _ftype)) = fs.lookup(ROOT_INO, name).unwrap() {
            // Read in chunks; any Corrupt error is a bug.
            let mut off = 0u64;
            loop {
                match fs.read(found_ino, off, 4096) {
                    Ok(data) => {
                        if data.is_empty() {
                            break;
                        }
                        off += data.len() as u64;
                    }
                    Err(e) => {
                        panic!("seed {seed}: read of {name:?} failed after fault {fault:?}: {e:?}");
                    }
                }
            }
            // Silence unused variable warning for ino
            let _ = ino;
        }
    }

    let _ = std::fs::remove_file(&img);
}

#[test]
fn crash_fuzz_many_seeds() {
    // 200 seeds × 3 fault points of coverage. Each seed is deterministic.
    // Increase this number to hunt for rarer bugs.
    for seed in 1..=200u64 {
        fuzz_one(seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(0x12345));
    }
}
