//! T4: loom concurrency models for TxgCoord.
//!
//! Models the wait/notify/error protocol of `TxgCoord` (engine.rs).
//! Run with: `RUSTFLAGS="--cfg loom" cargo test --test t4_loom --release`

#![cfg(loom)]

use loom::sync::{Condvar, Mutex};
use loom::thread;
use std::sync::Arc;

/// Mirror of TxgCoord's inner state.
struct TxgInner {
    current: u64,
    synced: u64,
    dirty: bool,
    error: Option<String>,
}

struct TxgModel {
    state: Mutex<TxgInner>,
    cv: Condvar,
}

impl TxgModel {
    fn new() -> Self {
        Self {
            state: Mutex::new(TxgInner {
                current: 1,
                synced: 0,
                dirty: false,
                error: None,
            }),
            cv: Condvar::new(),
        }
    }

    fn wait(&self, id: u64) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        while s.synced < id {
            if let Some(e) = s.error.clone() {
                return Err(e);
            }
            s = self.cv.wait(s).unwrap();
        }
        Ok(())
    }

    fn set_error(&self, e: String) {
        let mut s = self.state.lock().unwrap();
        s.error = Some(e);
        self.cv.notify_all();
    }

    fn sync_done(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        s.synced = id;
        s.error = None;
        self.cv.notify_all();
    }
}

/// T4.1: waiter wakes when sync completes.
#[test]
fn loom_txg_wait_wakes_on_sync() {
    loom::model(|| {
        let txg = Arc::new(TxgModel::new());
        let t1 = {
            let txg = Arc::clone(&txg);
            thread::spawn(move || txg.wait(1).unwrap())
        };
        let t2 = {
            let txg = Arc::clone(&txg);
            thread::spawn(move || txg.sync_done(1))
        };
        t1.join().unwrap();
        t2.join().unwrap();
    });
}

/// T4.2: waiter gets error if sync fails.
#[test]
fn loom_txg_wait_gets_error() {
    loom::model(|| {
        let txg = Arc::new(TxgModel::new());
        let t1 = {
            let txg = Arc::clone(&txg);
            thread::spawn(move || {
                let r = txg.wait(1);
                assert!(r.is_err());
            })
        };
        let t2 = {
            let txg = Arc::clone(&txg);
            thread::spawn(move || txg.set_error("io".to_string()))
        };
        t1.join().unwrap();
        t2.join().unwrap();
    });
}

/// T4.3: multiple waiters all wake.
#[test]
fn loom_txg_multiple_waiters() {
    loom::model(|| {
        let txg = Arc::new(TxgModel::new());
        let mut handles = vec![];
        for _ in 0..3 {
            let txg = Arc::clone(&txg);
            handles.push(thread::spawn(move || txg.wait(1).unwrap()));
        }
        let txg2 = Arc::clone(&txg);
        let syncer = thread::spawn(move || txg2.sync_done(1));
        for h in handles {
            h.join().unwrap();
        }
        syncer.join().unwrap();
    });
}

/// E2: P1 commit-split model.
///
/// Mirrors the TxgCoord E2 protocol:
/// - `commit_async` captures `syncing = current`, increments `current`.
/// - `finish_sync(txg)` sets `synced = txg` (the specific txg), clears `syncing`.
/// - Waiters for txg <= synced wake; waiters for newer txgs do not.
///
/// This model verifies that a WRITE joining the next txg (after the flush
/// point) is NOT acknowledged as durable when the previous txg syncs.

/// Mirror of TxgCoord with E2 syncing field.
struct TxgE2Model {
    state: Mutex<TxgE2Inner>,
    cv: Condvar,
}

struct TxgE2Inner {
    current: u64,
    synced: u64,
    syncing: Option<u64>,
    dirty: bool,
}

impl TxgE2Model {
    fn new() -> Self {
        Self {
            state: Mutex::new(TxgE2Inner {
                current: 1,
                synced: 0,
                syncing: None,
                dirty: false,
            }),
            cv: Condvar::new(),
        }
    }

    /// E2 commit_async: close txg at flush point.
    /// Returns the syncing txg.
    fn commit_async(&self) -> u64 {
        let mut s = self.state.lock().unwrap();
        let syncing = s.current;
        s.current += 1;
        s.syncing = Some(syncing);
        s.dirty = true;
        syncing
    }

    /// E2 finish_sync: publish the specific txg.
    fn finish_sync(&self, txg: u64) {
        let mut s = self.state.lock().unwrap();
        // Must be the txg that was syncing.
        assert_eq!(s.syncing, Some(txg));
        s.synced = txg;
        s.syncing = None;
        s.dirty = false;
        self.cv.notify_all();
    }

    /// Wait for txg to be durable.
    fn wait(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        while s.synced < id {
            s = self.cv.wait(s).unwrap();
        }
    }

    fn current(&self) -> u64 {
        self.state.lock().unwrap().current
    }

    fn synced(&self) -> u64 {
        self.state.lock().unwrap().synced
    }
}

/// T4.4: E2 split — finish_sync publishes the specific txg, not current.
/// Verifies that after commit_async (txg 1) and a concurrent write (txg 2),
/// finish_sync(1) sets synced=1 (not 2), so the txg-2 waiter does not wake early.
#[test]
fn loom_e2_split_no_early_ack() {
    loom::model(|| {
        let txg = Arc::new(TxgE2Model::new());

        // Main: commit_async closes txg 1.
        let syncing = txg.commit_async();
        assert_eq!(syncing, 1);
        // A concurrent write would now get txg 2 (current advanced).
        let write_txg = txg.current();
        assert_eq!(write_txg, 2);

        // Thread D: finish_sync(1).
        let txg_d = Arc::clone(&txg);
        let d = thread::spawn(move || txg_d.finish_sync(1));
        d.join().unwrap();

        // Synced must be exactly 1, not 2. The txg-2 write is not durable.
        assert_eq!(txg.synced(), 1);
    });
}

/// T4.5: E2 sequential commits — second commit gets next txg.
#[test]
fn loom_e2_sequential_commits() {
    loom::model(|| {
        let txg = Arc::new(TxgE2Model::new());

        let t1 = txg.commit_async();
        assert_eq!(t1, 1);
        assert_eq!(txg.current(), 2);

        txg.finish_sync(t1);
        assert_eq!(txg.synced(), 1);

        let t2 = txg.commit_async();
        assert_eq!(t2, 2);
        assert_eq!(txg.current(), 3);

        txg.finish_sync(t2);
        assert_eq!(txg.synced(), 2);
    });
}
