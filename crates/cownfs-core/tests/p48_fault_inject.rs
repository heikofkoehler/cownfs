//! B3/D3: fault-injection harness — torn writes, bit flips, reordering.

use cownfs_core::block::FaultInjector;
use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-fault-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn torn_write_detected_by_checksum() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![0xabu8; 8192]).unwrap();
    fs.commit().unwrap();

    // Enable torn writes (only first 100 bytes of each block).
    fs.set_device_faults(FaultInjector::new().with_torn_writes(100));
    // Overwrite: the torn write should corrupt the block.
    fs.write(ino, 0, &vec![0xcdu8; 8192]).unwrap();
    fs.clear_device_faults();
    fs.commit().unwrap();
    drop(fs);

    // Reopen and read: checksum MUST fail (torn block detected).
    // The torn write persists only 100 of 4096 bytes; the checksum was
    // computed over the full in-memory buffer, so verification fails
    // deterministically.
    let fs = Fs::open(&img).unwrap();
    match fs.read(ino, 0, 8192) {
        Err(cownfs_core::engine::FsError::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt from torn write, got {e}"),
        Ok(_) => panic!("expected Corrupt from torn write, read succeeded"),
    }
    // The filesystem itself must still be consistent.
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}

#[test]
fn bit_flip_detected() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, &vec![0xabu8; 8192]).unwrap();
    fs.commit().unwrap();

    // Enable bit flips with probability 1.0: every written block gets a
    // deterministic single-bit flip (seeded by block number), while the
    // extent checksum is computed from the pre-flip buffer. Corruption is
    // guaranteed, not probabilistic.
    fs.set_device_faults(FaultInjector::new().with_bit_flips(1.0));
    fs.write(ino, 0, &vec![0xcdu8; 8192]).unwrap();
    fs.clear_device_faults();
    fs.commit().unwrap();
    drop(fs);

    // Reopen and read: checksum MUST fail deterministically.
    let fs = Fs::open(&img).unwrap();
    match fs.read(ino, 0, 8192) {
        Err(cownfs_core::engine::FsError::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt from bit flip, got {e}"),
        Ok(_) => panic!("expected Corrupt from bit flip, read succeeded"),
    }
    // Metadata is intact; only the data block checksum fails.
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}

#[test]
fn reordered_writes_stay_consistent() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    // Enable write reordering.
    fs.set_device_faults(FaultInjector::new().with_reordering());
    // Do several commits with reordered writes.
    for i in 0..5 {
        let ino = fs
            .create(ROOT_INO, format!("f{i}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        fs.write(ino, 0, &vec![i as u8; 4096]).unwrap();
        fs.commit().unwrap();
    }
    fs.clear_device_faults();
    drop(fs);

    // Reopen: filesystem must be consistent (ping-pong + checksums
    // protect against reordering).
    let fs = Fs::open(&img).unwrap();
    fs.check().unwrap();
    // Data should be intact.
    for i in 0..5 {
        let (ino, _) = fs
            .lookup(ROOT_INO, format!("f{i}").as_bytes())
            .unwrap()
            .unwrap();
        let data = fs.read(ino, 0, 4096).unwrap();
        assert!(data.iter().all(|&b| b == i as u8));
    }
    let _ = std::fs::remove_file(&img);
}
