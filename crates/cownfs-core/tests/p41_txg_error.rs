//! B1: txg error propagation — waiters wake with error on sync failure.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-txgerr-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 1024).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn txg_wait_returns_error_on_sync_failure() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"data").unwrap();
    let txg = fs.commit_async().unwrap();

    // Simulate a sync failure by recording an error directly.
    fs.txg().set_error("simulated sync failure".to_string());

    // Wait should return Err promptly, not hang.
    let start = std::time::Instant::now();
    let result = fs.txg().wait(txg);
    let elapsed = start.elapsed();
    assert!(result.is_err(), "wait should return error");
    assert!(
        elapsed < Duration::from_secs(5),
        "wait hung (took {elapsed:?})"
    );
    let _ = std::fs::remove_file(&img);
}

#[test]
fn txg_wait_succeeds_after_error_cleared() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let txg = fs.commit_async().unwrap();

    // Error, then successful sync clears it.
    fs.txg().set_error("transient".to_string());
    assert!(fs.txg().wait(txg).is_err());

    // Successful sync_txg clears the error.
    fs.sync_txg().unwrap();
    // New txg should wait successfully.
    let txg2 = fs.commit_async().unwrap();
    fs.sync_txg().unwrap();
    assert!(fs.txg().wait(txg2).is_ok());
    let _ = std::fs::remove_file(&img);
}
