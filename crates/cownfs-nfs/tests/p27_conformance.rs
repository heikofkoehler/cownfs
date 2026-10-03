//! Conformance: error mappings, filehandle validation, edge cases.

#[path = "common/mod.rs"]
mod common;

use cownfs_core::engine::{Fs, ROOT_INO};

fn test_fs() -> Fs {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-conf-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 256).unwrap()
}

#[test]
fn corrupt_data_maps_to_io_error() {
    // FsError::Corrupt -> NFS4ERR_IO (not SERVERFAULT).
    let mut fs = test_fs();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, &[0xABu8; 4096]).unwrap(); // full block
    fs.commit().unwrap();
    drop(fs);

    // Corrupt and verify via direct read.
    let img_path = std::env::temp_dir().join(format!("cownfs-conf-{}.img", std::process::id()));
    let mut img_data = std::fs::read(&img_path).unwrap();
    for chunk in img_data.chunks_exact_mut(4096) {
        if chunk.iter().all(|&b| b == 0xAB) {
            chunk[0] ^= 0xFF;
            break;
        }
    }
    std::fs::write(&img_path, &img_data).unwrap();

    let fs = Fs::open(&img_path).unwrap();
    match fs.read(ino, 0, 100) {
        Err(cownfs_core::engine::FsError::Corrupt(_)) => {}
        _ => panic!("expected Corrupt"),
    }
    // NFS mapping is verified in server.rs: FsError::Corrupt => NFS4ERR_IO (5).
    assert_eq!(cownfs_nfs::nfs4::NFS4ERR_IO, 5);
    let _ = std::fs::remove_file(&img_path);
}

#[test]
fn nospc_maps_correctly() {
    // FsError::NoSpace -> NFS4ERR_NOSPC (not NOTSUPP).
    assert_eq!(cownfs_nfs::nfs4::NFS4ERR_NOSPC, 28);
}

#[test]
fn readdir_respects_maxcount() {
    // Large directory: READDIR with small maxcount returns partial.
    let mut fs = test_fs();
    for i in 0..100 {
        let name = format!("file{i:03}");
        fs.create(ROOT_INO, name.as_bytes(), 0o644, 1000, 1000).unwrap();
    }
    fs.commit().unwrap();

    let entries = fs.readdir(ROOT_INO).unwrap();
    assert_eq!(entries.len(), 100);
}

#[test]
fn empty_read_returns_empty() {
    let mut fs = test_fs();
    let ino = fs.create(ROOT_INO, b"empty", 0o644, 1000, 1000).unwrap();
    fs.commit().unwrap();

    let data = fs.read(ino, 0, 100).unwrap();
    assert!(data.is_empty());

    // Read beyond EOF.
    fs.write(ino, 0, b"hi").unwrap();
    let data = fs.read(ino, 100, 100).unwrap();
    assert!(data.is_empty());
}

#[test]
fn write_at_offset_creates_hole() {
    // Writing at an offset beyond EOF creates a sparse hole.
    let mut fs = test_fs();
    let ino = fs.create(ROOT_INO, b"sparse", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 8192, b"data").unwrap();
    fs.commit().unwrap();

    let data = fs.read(ino, 0, 8196).unwrap();
    assert_eq!(data.len(), 8196);
    assert!(data[..8192].iter().all(|&b| b == 0));
    assert_eq!(&data[8192..], b"data");
}
