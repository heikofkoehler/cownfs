//! Data block checksums: parent-stored CRC32C in extents,
//! verified on read.

use cownfs_core::engine::{Fs, ROOT_INO};

#[test]
fn data_checksum_verified_on_read() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-cksum-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 256).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    let data = b"hello checksum world";
    fs.write(ino, 0, data).unwrap();
    fs.commit().unwrap();

    // Read back: should succeed and verify.
    let out = fs.read(ino, 0, data.len()).unwrap();
    assert_eq!(out, data);

    drop(fs);
    std::fs::remove_file(&img).ok();
}

#[test]
fn data_checksum_detects_corruption() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-cksum-corrupt-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 256).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    let data = vec![0xABu8; 8192]; // 2 blocks
    fs.write(ino, 0, &data).unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Corrupt a data block directly in the image.
    // Find a data block by scanning for the pattern.
    let mut img_data = std::fs::read(&img).unwrap();
    let mut corrupted = false;
    for chunk in img_data.chunks_exact_mut(4096) {
        if chunk.iter().all(|&b| b == 0xAB) {
            chunk[0] ^= 0xFF; // flip bits
            corrupted = true;
            break;
        }
    }
    assert!(corrupted, "should find the data block");
    std::fs::write(&img, &img_data).unwrap();

    // Reopen and read: should detect the corruption.
    let fs = Fs::open(&img).unwrap();
    let result = fs.read(ino, 0, data.len());
    match result {
        Err(cownfs_core::engine::FsError::Corrupt(_)) => {} // expected
        Ok(_) => panic!("should have detected corruption"),
        Err(e) => panic!("wrong error: {e}"),
    }

    std::fs::remove_file(&img).ok();
}
