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
