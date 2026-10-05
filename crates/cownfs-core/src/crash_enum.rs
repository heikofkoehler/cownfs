//! T1: crash-state enumeration harness (CrashMonkey/ALICE style).
//!
//! A crash can land at any instant. The disk then holds some subset of the
//! writes the filesystem issued, in some order, possibly with the last
//! write torn. Everything before the last `sync` barrier is durable;
//! everything after is volatile.
//!
//! The harness:
//! 1. Records a workload's device ops (`FileDevice` record hook).
//! 2. Enumerates crash states: for each sync point, the full prefix
//!    through that sync plus any subset of the post-sync writes
//!    (bounded reorderings, optional torn last write).
//! 3. Materializes each state onto a freshly formatted image and runs
//!    `Fs::open` -> `check()` -> durability-oracle verification.
//!
//! Tiers: [`Tier::Random`] (PR: 200 random states) and [`Tier::Exhaustive`]
//! (nightly: all subsets for small post-sync windows).

use crate::block::RecordedOp;
use crate::{Block, BLOCK_SIZE};

/// A single crash state: the exact op list to replay onto a freshly
/// formatted image to materialize the post-crash disk contents.
#[derive(Debug, Clone)]
pub struct CrashState {
    /// Index (into the recording's sync list) of the last sync included
    /// in the prefix. All ledger entries acknowledged at or before this
    /// sync must be present after recovery.
    pub sync_idx: usize,
    /// Ops to replay: prefix through the sync + chosen post-sync subset.
    pub ops: Vec<RecordedOp>,
    /// True if the last write in `ops` is torn (partial).
    pub torn: bool,
}

/// Enumeration tier.
pub enum Tier {
    /// PR tier: `count` random crash states, deterministic from `seed`.
    Random { count: usize, seed: u64 },
    /// Nightly tier: exhaustive subsets for post-sync windows of at most
    /// `max_window` writes; larger windows fall back to random sampling
    /// (`sample` states each, deterministic from `seed`).
    Exhaustive {
        max_window: usize,
        sample: usize,
        seed: u64,
    },
}

/// Deterministic xorshift64* PRNG (no external deps; reproducible runs).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

/// Indices (into the op list) of every `Sync` op.
pub fn sync_indices(ops: &[RecordedOp]) -> Vec<usize> {
    ops.iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, RecordedOp::Sync))
        .map(|(i, _)| i)
        .collect()
}

/// Enumerate crash states from a recording.
pub fn enumerate(ops: &[RecordedOp], tier: Tier) -> Vec<CrashState> {
    let syncs = sync_indices(ops);
    if syncs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    match tier {
        Tier::Random { count, seed } => {
            let mut rng = Rng(seed);
            for _ in 0..count {
                out.push(random_state(ops, &syncs, &mut rng));
            }
        }
        Tier::Exhaustive {
            max_window,
            sample,
            seed,
        } => {
            let mut rng = Rng(seed);
            for (si, &spos) in syncs.iter().enumerate() {
                let next_sync = syncs.get(si + 1).copied().unwrap_or(ops.len());
                let window: Vec<&RecordedOp> = ops[spos + 1..next_sync]
                    .iter()
                    .filter(|op| matches!(op, RecordedOp::Write { .. }))
                    .collect();
                let prefix: Vec<RecordedOp> = ops[..=spos].to_vec();
                if window.len() <= max_window {
                    // All 2^|W| subsets, in recorded order, plus torn variants.
                    for mask in 0..(1u64 << window.len()) {
                        let mut chosen: Vec<RecordedOp> = window
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| mask & (1 << i) != 0)
                            .map(|(_, op)| (*op).clone())
                            .collect();
                        out.push(CrashState {
                            sync_idx: si,
                            ops: prefix.iter().cloned().chain(chosen.clone()).collect(),
                            torn: false,
                        });
                        if !chosen.is_empty() {
                            tear_last(&mut chosen, &mut rng);
                            out.push(CrashState {
                                sync_idx: si,
                                ops: prefix.iter().cloned().chain(chosen).collect(),
                                torn: true,
                            });
                        }
                    }
                } else {
                    // Large window: bounded random sampling.
                    for _ in 0..sample {
                        out.push(random_state_for_window(
                            &prefix, si, &window, &mut rng, true,
                        ));
                    }
                }
            }
        }
    }
    out
}

/// One random crash state over the whole recording.
fn random_state(ops: &[RecordedOp], syncs: &[usize], rng: &mut Rng) -> CrashState {
    let si = rng.below(syncs.len());
    let spos = syncs[si];
    let next_sync = syncs.get(si + 1).copied().unwrap_or(ops.len());
    let window: Vec<&RecordedOp> = ops[spos + 1..next_sync]
        .iter()
        .filter(|op| matches!(op, RecordedOp::Write { .. }))
        .collect();
    let prefix: Vec<RecordedOp> = ops[..=spos].to_vec();
    let allow_shuffle = rng.below(2) == 0;
    random_state_for_window(&prefix, si, &window, rng, allow_shuffle)
}

/// Random subset (+ optional shuffle + optional tear) of one window.
fn random_state_for_window(
    prefix: &[RecordedOp],
    si: usize,
    window: &[&RecordedOp],
    rng: &mut Rng,
    allow_shuffle: bool,
) -> CrashState {
    let mut chosen: Vec<RecordedOp> = window
        .iter()
        .filter(|_| rng.below(2) == 0)
        .map(|op| (*op).clone())
        .collect();
    if allow_shuffle {
        rng.shuffle(&mut chosen);
    }
    let torn = !chosen.is_empty() && rng.below(2) == 0;
    if torn {
        tear_last(&mut chosen, rng);
    }
    CrashState {
        sync_idx: si,
        ops: prefix.iter().cloned().chain(chosen).collect(),
        torn,
    }
}

/// Simulate a torn write: the last write persisted only a prefix of its
/// bytes (power loss mid-DMA); the rest reads as zeros.
fn tear_last(chosen: &mut [RecordedOp], rng: &mut Rng) {
    if let Some(RecordedOp::Write { data, .. }) = chosen.last_mut() {
        let keep = 1 + rng.below(BLOCK_SIZE - 1);
        let mut torn_data: Block = [0u8; BLOCK_SIZE];
        torn_data[..keep].copy_from_slice(&data[..keep]);
        *data = torn_data;
    }
}

/// Count `Sync` ops in a (partial) recording.
pub fn count_syncs(ops: &[RecordedOp]) -> usize {
    ops.iter()
        .filter(|op| matches!(op, RecordedOp::Sync))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(block: u64) -> RecordedOp {
        RecordedOp::Write {
            block,
            data: [block as u8; BLOCK_SIZE],
        }
    }

    #[test]
    fn exhaustive_small_window_covers_all_subsets() {
        // sync, w1, w2, sync, w3
        let ops = vec![w(0), RecordedOp::Sync, w(1), w(2), RecordedOp::Sync, w(3)];
        let states = enumerate(
            &ops,
            Tier::Exhaustive {
                max_window: 8,
                sample: 4,
                seed: 1,
            },
        );
        // Window after sync 0: {w1, w2} -> 4 subsets (+ torn variants for
        // the 3 non-empty) = 7 states. Window after sync 1: {w3} ->
        // 2 subsets (+ 1 torn) = 3 states. Total 10.
        assert_eq!(states.len(), 10);
        // Every state starts with the prefix through its sync.
        for s in &states {
            assert!(matches!(s.ops[0], RecordedOp::Write { block: 0, .. }));
        }
    }

    #[test]
    fn random_tier_is_deterministic() {
        let ops = vec![w(0), RecordedOp::Sync, w(1), RecordedOp::Sync];
        let a = enumerate(
            &ops,
            Tier::Random {
                count: 50,
                seed: 42,
            },
        );
        let b = enumerate(
            &ops,
            Tier::Random {
                count: 50,
                seed: 42,
            },
        );
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.ops.len(), y.ops.len());
            assert_eq!(x.torn, y.torn);
        }
    }

    #[test]
    fn torn_write_truncates_last() {
        let mut chosen = vec![w(9)];
        let mut rng = Rng(7);
        tear_last(&mut chosen, &mut rng);
        match &chosen[0] {
            RecordedOp::Write { data, .. } => {
                // Some prefix kept, rest zeros.
                let kept = data.iter().take_while(|&&b| b == 9).count();
                assert!(kept >= 1 && kept < BLOCK_SIZE);
                assert!(data[kept..].iter().all(|&b| b == 0));
            }
            _ => panic!("expected write"),
        }
    }
}
