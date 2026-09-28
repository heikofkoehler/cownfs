//! P1 gate: randomized model test of the CoW B-tree against `BTreeMap`.
//!
//! Exercises insert/remove/get plus snapshot creation and drops under a
//! deterministic PRNG, then checks every live tree for exact equality with
//! its model and for node leaks (allocated == reachable once snapshots are
//! gone).

use std::collections::BTreeMap;

use cownfs_core::btree::BTree;

/// xorshift64* — deterministic, no dependency.
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

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn check_tree<const T: usize>(t: &BTree<u64, u64, T>, m: &BTreeMap<u64, u64>) {
    let got: Vec<(u64, u64)> = t.to_sorted_vec();
    let want: Vec<(u64, u64)> = m.iter().map(|(&k, &v)| (k, v)).collect();
    assert_eq!(got, want, "tree contents diverged from model");
    assert_eq!(t.len(), m.len(), "len diverged from model");
}

fn run_model<const T: usize>(seed: u64, ops: usize) {
    let mut rng = Rng(seed);
    let mut trees: Vec<(BTree<u64, u64, T>, BTreeMap<u64, u64>)> =
        vec![(BTree::new(), BTreeMap::new())];

    for step in 0..ops {
        let ti = rng.below(trees.len() as u64) as usize;
        match rng.below(100) {
            0..=44 => {
                let k = rng.below(2000);
                let v = rng.next();
                let old_t = trees[ti].0.insert(k, v);
                let old_m = trees[ti].1.insert(k, v);
                assert_eq!(old_t, old_m, "insert old-value mismatch at step {step}");
            }
            45..=69 => {
                let k = rng.below(2000);
                let old_t = trees[ti].0.remove(&k);
                let old_m = trees[ti].1.remove(&k);
                assert_eq!(old_t, old_m, "remove old-value mismatch at step {step}");
            }
            70..=79 => {
                let k = rng.below(2000);
                assert_eq!(
                    trees[ti].0.get(&k),
                    trees[ti].1.get(&k).copied(),
                    "get mismatch at step {step}"
                );
            }
            80..=89 => {
                // Snapshot: shares structure with the source tree.
                let t2 = trees[ti].0.snapshot();
                let m2 = trees[ti].1.clone();
                trees.push((t2, m2));
            }
            _ => {
                if trees.len() > 1 {
                    let di = rng.below(trees.len() as u64) as usize;
                    trees.swap_remove(di);
                }
            }
        }
        if step % 500 == 0 {
            for (t, m) in &trees {
                check_tree(t, m);
            }
        }
    }

    for (t, m) in &trees {
        check_tree(t, m);
    }

    // Leak check: drop every snapshot; every allocated node must be
    // reachable from the one surviving root.
    let (mut survivor, _) = trees.pop().unwrap();
    drop(trees);
    // Touch the survivor so the compiler can't drop it early.
    survivor.insert(u64::MAX, 1);
    survivor.remove(&u64::MAX);
    let (live, reachable) = survivor.debug_stats();
    assert_eq!(live, reachable, "leaked nodes: {live} allocated, {reachable} reachable");
}

#[test]
fn model_t4() {
    for seed in [1, 2, 3, 42] {
        run_model::<4>(seed, 30_000);
    }
}

#[test]
fn model_t2_stress_splits() {
    // Minimum degree 2: nodes hold at most 3 keys, splitting constantly.
    run_model::<2>(7, 15_000);
}

#[test]
fn ascending_inserts_descending_removes() {
    let mut t = BTree::<u64, u64, 4>::new();
    for k in 0..5000 {
        t.insert(k, k * 3);
    }
    assert_eq!(t.len(), 5000);
    for k in 0..5000 {
        assert_eq!(t.get(&k), Some(k * 3));
    }
    for k in (0..5000).rev() {
        assert_eq!(t.remove(&k), Some(k * 3));
    }
    assert!(t.is_empty());
    assert_eq!(t.to_sorted_vec(), vec![]);
    let (live, reachable) = t.debug_stats();
    assert_eq!((live, reachable), (1, 1), "only the empty root should remain");
}

#[test]
fn cow_snapshot_isolation() {
    let mut base = BTree::<u64, u64, 4>::new();
    for k in 0..100 {
        base.insert(k, k);
    }
    let mut snap = base.snapshot();
    // Mutate only the snapshot: inserts, overwrites, deletes.
    for k in 100..200 {
        snap.insert(k, k * 2);
    }
    for k in (0..50).step_by(2) {
        snap.remove(&k);
    }
    snap.insert(7, 777);
    // Base is untouched.
    assert_eq!(base.len(), 100);
    for k in 0..100 {
        assert_eq!(base.get(&k), Some(k));
    }
    assert_eq!(base.get(&150), None);
    // Snapshot has its own contents.
    assert_eq!(snap.len(), 100 + 100 - 25);
    assert_eq!(snap.get(&7), Some(777));
    assert_eq!(snap.get(&8), None); // removed
    assert_eq!(snap.get(&150), Some(300));
    drop(snap);
    let (live, reachable) = base.debug_stats();
    assert_eq!(live, reachable, "snapshot drop leaked nodes");
}
