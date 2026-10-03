//! Reliability: lease races, checksum on snapshots, backup corruption.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::{Arc, Mutex};
use std::thread;

#[test]
fn lease_concurrent_acquire() {
    // Two threads racing to acquire: exactly one wins.
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-lease-race-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 256).unwrap();

    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = vec![];
    for i in 0..4 {
        let img = img.clone();
        let results = results.clone();
        handles.push(thread::spawn(move || {
            let mut fs = Fs::open(&img).unwrap();
            let won = fs.lease_acquire(&format!("node{i}"), 60).unwrap();
            results.lock().unwrap().push((i, won));
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let results = results.lock().unwrap();
    let winners: Vec<_> = results.iter().filter(|(_, w)| *w).collect();
    assert_eq!(winners.len(), 1, "exactly one winner, got {results:?}");

    std::fs::remove_file(&img).ok();
}

#[test]
fn checksum_verified_on_snapshot_read() {
    // Checksums are verified even when reading from snapshots.
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-cksum-snap-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 256).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    let data = vec![0xCDu8; 4096];
    fs.write(ino, 0, &data).unwrap();
    let snap = fs.snapshot_create(b"snap1").unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Corrupt the data block.
    let mut img_data = std::fs::read(&img).unwrap();
    for chunk in img_data.chunks_exact_mut(4096) {
        if chunk.iter().all(|&b| b == 0xCD) {
            chunk[0] ^= 0xFF;
            break;
        }
    }
    std::fs::write(&img, &img_data).unwrap();

    // Snapshot read should detect corruption.
    let fs = Fs::open(&img).unwrap();
    let result = fs.snapshot_read(snap, ino, 0, data.len());
    assert!(result.is_err(), "snapshot read should detect corruption");

    std::fs::remove_file(&img).ok();
}

#[test]
fn backup_verify_detects_corruption() {
    // A corrupted backup file fails verification.
    use std::process::Command;

    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let img = dir.join(format!("bkup-corr-{pid}.img"));
    let bak = dir.join(format!("bkup-corr-{pid}.bak"));
    for p in [&img, &bak] {
        let _ = std::fs::remove_file(p);
    }

    let mut fs = Fs::format(&img, 256).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, &[0xEEu8; 100]).unwrap();
    fs.commit().unwrap();
    drop(fs);

    let mut bin = std::env::current_exe().unwrap();
    bin.pop();
    bin.pop();
    bin.push("cownfs-backup");

    let out = Command::new(&bin)
        .args(["create", img.to_str().unwrap(), bak.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Corrupt the backup.
    let mut bak_data = std::fs::read(&bak).unwrap();
    let mid = bak_data.len() / 2;
    bak_data[mid] ^= 0xFF;
    std::fs::write(&bak, &bak_data).unwrap();

    // Verify should fail.
    let out = Command::new(&bin)
        .args(["verify", bak.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "verify should fail on corrupted backup"
    );

    for p in [&img, &bak] {
        let _ = std::fs::remove_file(p);
    }
}
