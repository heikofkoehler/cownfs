//! Benchmark for readahead strace comparison.
//! Reads a 10MB file sequentially in 4KB chunks via Fs::read.

use cownfs_core::engine::Fs;
use std::path::PathBuf;

fn main() {
    let img = PathBuf::from("/tmp/cownfs-readahead-bench.img");
    let _ = std::fs::remove_file(&img);

    // Create a 10MB file with patterned data.
    let mut fs = Fs::format(&img, 8192).unwrap();
    let ino = fs.create(1, b"bench10m", 0o644, 0, 0).unwrap();
    let size: usize = 10 * 1024 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    // Write in 1MB chunks to avoid huge single write.
    for (i, chunk) in data.chunks(1024 * 1024).enumerate() {
        fs.write(ino, (i * 1024 * 1024) as u64, chunk).unwrap();
    }
    fs.commit().unwrap();
    drop(fs);

    // Reopen and read sequentially in 4KB chunks.
    let fs = Fs::open(&img).unwrap();
    let mut offset = 0u64;
    let chunk_size = 4096;
    let mut total = 0usize;
    let mut checksum: u64 = 0;
    while offset < size as u64 {
        let len = chunk_size.min(size as u64 - offset) as usize;
        let got = fs.read(ino, offset, len).unwrap();
        assert_eq!(got.len(), len);
        // Simple checksum to prevent optimization.
        for &b in &got {
            checksum = checksum.wrapping_add(b as u64);
        }
        total += got.len();
        offset += len as u64;
    }
    assert_eq!(total, size);
    println!("Read {total} bytes, checksum {checksum}");

    let _ = std::fs::remove_file(&img);
}
