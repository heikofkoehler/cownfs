//! D5: chaos — drop without commit (simulated kill -9), verify recovery.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-chaos-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 8192).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn kill9_during_writes_recovers_to_last_commit() {
    let img = test_image();
    // Phase 1: committed state.
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"committed-data").unwrap();
    fs.commit().unwrap();
    // Phase 2: uncommitted writes (simulating work in progress).
    fs.write(ino, 0, b"uncommitted-XXXX").unwrap();
    let ino2 = fs.create(ROOT_INO, b"g", 0o644, 0, 0).unwrap();
    fs.write(ino2, 0, b"never-committed").unwrap();
    // Simulate kill -9: drop without commit.
    drop(fs);

    // Reopen: must see the last committed state.
    let fs = Fs::open(&img).unwrap();
    fs.check().unwrap();
    // Original file should have committed data.
    let data = fs.read(ino, 0, 14).unwrap();
    assert_eq!(&data, b"committed-data", "should see last committed data");
    // Uncommitted file should not exist.
    assert!(
        fs.lookup(ROOT_INO, b"g").unwrap().is_none(),
        "uncommitted file should not exist"
    );
    let _ = std::fs::remove_file(&img);
}

#[test]
fn kill9_during_commit_recovers_cleanly() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![0xabu8; 16384]).unwrap();
    // Start commit but simulate crash via fault point.
    fs.set_fault_point(cownfs_core::engine::FaultPoint::AfterBitmap);
    let _ = fs.commit(); // Will fail with InjectedFault.
    drop(fs);

    // Reopen: must be clean (either old or new gen, but consistent).
    let fs = Fs::open(&img).unwrap();
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}
