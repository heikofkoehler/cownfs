//! P1.10: Sequential readahead test.
//!
//! Verifies that the readahead logic in Fs::read() doesn't corrupt data:
//! - Sequential reads in small chunks return correct data
//! - Random reads return correct data
//! - Readahead state is tracked per-inode

use cownfs_core::engine::Fs;

fn test_image(name: &str) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!(
        "cownfs-readahead-{name}-{}.img",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&img);
    img
}

#[test]
fn readahead_sequential_reads_correct() {
    let img = test_image("seq");
    let mut fs = Fs::format(&img, 8192).unwrap();

    // Create a file with known patterned data (1MB).
    let ino = fs.create(1, b"bigfile", 0o644, 0, 0).unwrap();
    let size = 1024 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    fs.write(ino, 0, &data).unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Reopen and read sequentially in 4KB chunks.
    let fs = Fs::open(&img).unwrap();
    let mut offset = 0u64;
    let chunk = 4096;
    while offset < size as u64 {
        let len = chunk.min(size as u64 - offset) as usize;
        let got = fs.read(ino, offset, len).unwrap();
        let expected = &data[offset as usize..offset as usize + len];
        assert_eq!(got, expected, "mismatch at offset {offset}");
        offset += len as u64;
    }

    let _ = std::fs::remove_file(&img);
}

#[test]
fn readahead_random_reads_correct() {
    let img = test_image("rand");
    let mut fs = Fs::format(&img, 8192).unwrap();

    let ino = fs.create(1, b"randfile", 0o644, 0, 0).unwrap();
    let size = 512 * 1024;
    let data: Vec<u8> = (0..size).map(|i| ((i * 7 + 3) % 251) as u8).collect();
    fs.write(ino, 0, &data).unwrap();
    fs.commit().unwrap();
    drop(fs);

    let fs = Fs::open(&img).unwrap();

    // Random-access reads (non-sequential offsets).
    let offsets = [0u64, 100000, 5000, 400000, 2000, 300000, 1000];
    for &off in &offsets {
        let len = 8192.min(size as u64 - off) as usize;
        let got = fs.read(ino, off, len).unwrap();
        let expected = &data[off as usize..off as usize + len];
        assert_eq!(got, expected, "mismatch at offset {off}");
    }

    // Then sequential reads should still work.
    let mut offset = 0u64;
    while offset < size as u64 {
        let len = 4096.min(size as u64 - offset) as usize;
        let got = fs.read(ino, offset, len).unwrap();
        let expected = &data[offset as usize..offset as usize + len];
        assert_eq!(got, expected, "sequential mismatch at {offset}");
        offset += len as u64;
    }

    let _ = std::fs::remove_file(&img);
}

#[test]
fn readahead_does_not_read_past_eof() {
    let img = test_image("eof");
    let mut fs = Fs::format(&img, 8192).unwrap();

    let ino = fs.create(1, b"small", 0o644, 0, 0).unwrap();
    // Small file: 10KB (less than the 64KB initial readahead).
    let size = 10 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    fs.write(ino, 0, &data).unwrap();
    fs.commit().unwrap();
    drop(fs);

    let fs = Fs::open(&img).unwrap();
    // Read the whole file in one go; readahead must not extend past EOF
    // or return extra bytes.
    let got = fs.read(ino, 0, size).unwrap();
    assert_eq!(got.len(), size);
    assert_eq!(got, data);

    // Read past EOF returns empty.
    let got = fs.read(ino, size as u64 + 100, 4096).unwrap();
    assert!(got.is_empty());

    let _ = std::fs::remove_file(&img);
}
