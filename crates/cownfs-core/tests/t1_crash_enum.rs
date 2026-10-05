//! T1: crash-state enumeration harness — full loop.
//!
//! Workload: N rounds of (create file, write patterned data, commit),
//! recording every device op via the `FileDevice` record hook. Each
//! committed round appends to a durability ledger tagged with the sync
//! index at commit time.
//!
//! Then: enumerate crash states (prefix through each sync + subsets of
//! the post-sync window, torn variants), materialize each onto a freshly
//! formatted image, and verify `Fs::open` -> `check()` -> oracle (every
//! ledger entry acknowledged at or before the crash sync must read back
//! intact).
//!
//! Tiers: PR (200 random states, small + large images), nightly-style
//! exhaustive for a small workload.

use cownfs_core::block::{replay_ops, BlockDevice, FileDevice, RecordedOp};
use cownfs_core::crash_enum::{self, Tier};
use cownfs_core::engine::{Fs, ROOT_INO};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static IMG_SEQ: AtomicU64 = AtomicU64::new(0);

struct LedgerEntry {
    name: Vec<u8>,
    data: Vec<u8>,
    /// Index into the recording's sync list at the moment `commit()`
    /// returned. The entry is durable in any crash state whose prefix
    /// includes this sync.
    sync_idx: usize,
}

fn tmp_img(tag: &str) -> PathBuf {
    let n = IMG_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("t1_{tag}_{n}.img"))
}

/// Run the workload; return (recording, ledger).
fn run_workload(path: &Path, blocks: u64, rounds: usize) -> (Vec<RecordedOp>, Vec<LedgerEntry>) {
    let mut fs = Fs::format(path, blocks).expect("format");
    fs.arm_recorder();
    let rec = fs.recorder_handle().expect("recorder armed");
    let mut ledger = Vec::new();
    for round in 0..rounds {
        let name = format!("t1_f{round:03}");
        let ino = fs
            .create(ROOT_INO, name.as_bytes(), 0o644, 0, 0)
            .expect("create");
        // 2 blocks of patterned data. Distinct file per round, so
        // unacknowledged later writes never alias acknowledged regions.
        let data = vec![round as u8; 8192];
        fs.write(ino, 0, &data).expect("write");
        fs.commit().expect("commit");
        let sync_idx = crash_enum::count_syncs(&rec.lock().unwrap()) - 1;
        ledger.push(LedgerEntry {
            name: name.into_bytes(),
            data,
            sync_idx,
        });
    }
    let recording = fs.take_recording();
    assert!(!recording.is_empty(), "recording must contain device ops");
    drop(fs);
    (recording, ledger)
}

/// Materialize one crash state and run the full verification loop.
fn verify_state(blocks: u64, state: &crash_enum::CrashState, ledger: &[LedgerEntry], tag: &str) {
    let img = tmp_img(tag);
    // Remove the image on drop, including on panic: a crash harness
    // fails by panicking, and must not leak images in /tmp.
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = RemoveOnDrop(img.clone());
    // Fresh format, then replay the crash state's ops.
    drop(Fs::format(&img, blocks).expect("format crash image"));
    {
        let mut dev = FileDevice::open(&img).expect("open dev");
        replay_ops(&state.ops, &mut dev).expect("replay");
        // FileDevice::drop closes; ensure durability of the materialized image.
        let _ = dev.sync();
    }
    let fs = Fs::open(&img).unwrap_or_else(|e| {
        panic!(
            "T1: Fs::open failed for crash state sync_idx={} torn={}: {e:?}",
            state.sync_idx, state.torn
        )
    });
    fs.check().unwrap_or_else(|e| {
        panic!(
            "T1: check() failed for crash state sync_idx={} torn={}: {e:?}",
            state.sync_idx, state.torn
        )
    });
    // Durability oracle: everything acknowledged at or before the crash
    // sync must be present and byte-identical.
    for e in ledger.iter().filter(|e| e.sync_idx <= state.sync_idx) {
        let (ino, _) = fs
            .lookup(ROOT_INO, &e.name)
            .expect("lookup")
            .unwrap_or_else(|| {
                panic!(
                    "T1: ledger file {:?} missing after crash sync_idx={} torn={}",
                    String::from_utf8_lossy(&e.name),
                    state.sync_idx,
                    state.torn
                )
            });
        let data = fs.read(ino, 0, e.data.len()).expect("read");
        assert_eq!(
            data,
            e.data,
            "T1: oracle mismatch for {:?} sync_idx={} torn={}",
            String::from_utf8_lossy(&e.name),
            state.sync_idx,
            state.torn
        );
    }
}

fn window_size(recording: &[RecordedOp], syncs: &[usize], si: usize) -> usize {
    let next = syncs.get(si + 1).copied().unwrap_or(recording.len());
    recording[syncs[si] + 1..next]
        .iter()
        .filter(|op| matches!(op, RecordedOp::Write { .. }))
        .count()
}

#[test]
fn t1_pr_tier_200_random_states_small_image() {
    let img = tmp_img("workload");
    let (recording, ledger) = run_workload(&img, 8192, 6);
    let states = crash_enum::enumerate(
        &recording,
        Tier::Random {
            count: 200,
            seed: 0xC0FFEE,
        },
    );
    assert_eq!(states.len(), 200);
    for (i, s) in states.iter().enumerate() {
        verify_state(8192, s, &ledger, &format!("pr{i}"));
    }
    std::fs::remove_file(&img).unwrap();
}

#[test]
fn t1_pr_tier_200_random_states_large_image() {
    // 1M blocks: full-checkpoint bitmap writes are ~31 blocks per commit,
    // so post-sync windows are much larger than on the small image.
    let img = tmp_img("workload_big");
    let (recording, ledger) = run_workload(&img, 1_048_576, 4);
    let syncs = crash_enum::sync_indices(&recording);
    // Each commit does two syncs (data barrier, then slot barrier), so
    // the wide windows (full flush writes) sit at odd sync indices.
    let max_w = (0..syncs.len())
        .map(|si| window_size(&recording, &syncs, si))
        .max()
        .unwrap_or(0);
    assert!(
        max_w > 20,
        "large image should produce wide post-sync windows, got max {max_w}"
    );
    let states = crash_enum::enumerate(
        &recording,
        Tier::Random {
            count: 200,
            seed: 0xB16B1,
        },
    );
    assert_eq!(states.len(), 200);
    for (i, s) in states.iter().enumerate() {
        verify_state(1_048_576, s, &ledger, &format!("big{i}"));
    }
    std::fs::remove_file(&img).unwrap();
}

#[test]
fn t1_exhaustive_small_workload() {
    // Nightly-style: exhaustive subsets for small post-sync windows.
    let img = tmp_img("workload_exh");
    let (recording, ledger) = run_workload(&img, 4096, 3);
    let syncs = crash_enum::sync_indices(&recording);
    // Pin the test to the exhaustive path: every window must fit.
    const MAX_WINDOW: usize = 16;
    for si in 0..syncs.len() {
        let w = window_size(&recording, &syncs, si);
        assert!(
            w <= MAX_WINDOW,
            "window {si} has {w} writes > {MAX_WINDOW}: not exhaustive"
        );
    }
    let states = crash_enum::enumerate(
        &recording,
        Tier::Exhaustive {
            max_window: MAX_WINDOW,
            sample: 32,
            seed: 7,
        },
    );
    assert!(
        states.len() >= 30,
        "expected a real exhaustive enumeration, got {} states",
        states.len()
    );
    for (i, s) in states.iter().enumerate() {
        // Exhaustive can produce thousands of states on wider windows;
        // cap the verify loop for test time, but require full coverage
        // of the first sync's window (the most bug-prone: early commits).
        if i >= 2000 {
            break;
        }
        verify_state(4096, s, &ledger, &format!("exh{i}"));
    }
    std::fs::remove_file(&img).unwrap();
}

#[test]
fn t1_harness_selftest_oracle_is_live() {
    // Negative control: deliberately corrupt a ledger file's data block
    // in an otherwise fully-durable crash state. Verification MUST fail.
    // Guards against the harness silently becoming vacuous (e.g., ledger
    // filtering that excludes everything, or a check() that can't fail).
    let img = tmp_img("selftest_wl");
    let rounds = 2usize;
    let (recording, ledger) = run_workload(&img, 8192, rounds);
    let syncs = crash_enum::sync_indices(&recording);
    let last = syncs.len() - 1;
    let mut ops: Vec<RecordedOp> = recording[..=syncs[last]].to_vec();
    // The last round wrote 2 blocks filled with byte `rounds-1`. Corrupt
    // one of those data blocks: check() stays clean (block still
    // allocated and reachable) but the read checksum must fail, so only
    // a live oracle catches it.
    let pat = (rounds - 1) as u8;
    let mut corrupted = false;
    for op in ops.iter_mut() {
        if let RecordedOp::Write { data, .. } = op {
            if data.iter().all(|&b| b == pat) {
                data[100] ^= 0xFF;
                corrupted = true;
                break;
            }
        }
    }
    assert!(corrupted, "could not find last round's data block");
    let state = crash_enum::CrashState {
        sync_idx: last,
        ops,
        torn: false,
    };
    let result = std::panic::catch_unwind(|| {
        verify_state(8192, &state, &ledger, "selftest");
    });
    assert!(
        result.is_err(),
        "T1 self-test: oracle did not catch deliberate data corruption"
    );
    std::fs::remove_file(&img).unwrap();
}
