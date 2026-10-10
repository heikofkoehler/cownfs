//! #6 regression: the deferred-free queue must never corrupt the bitmap,
//! no matter the image size or queue length.
//!
//! Two failure modes of the old persist:
//! - `no_padding_no_clobber`: when `bitmap_bytes` is a multiple of
//!   BLOCK_SIZE (image size a multiple of 128 MiB) there is no padding
//!   after the bitmap data; the old code wrote the queue at offset 0 of
//!   the last bitmap page anyway, clobbering live bitmap words (and the
//!   queue was unreadable on open, leaking the freed blocks).
//! - `overflow_stays_consistent`: when the queue exceeds the real padding,
//!   the old code persisted a truncated queue while the area kept every
//!   bit set; on reopen the dropped entries were "allocated but
//!   unreachable" and fsck failed.
use cownfs_core::engine::{Fs, ROOT_INO};

fn unique_img(tag: &str) -> std::path::PathBuf {
    let img = std::env::temp_dir().join(format!(
        "cownfs-r3-{tag}-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&img);
    img
}

/// 128 MiB image => 32768 blocks => bitmap_bytes = 4096 = exactly one
/// block: zero padding after the bitmap data.
#[test]
fn no_padding_no_clobber() {
    let img = unique_img("nopad");
    let mut fs = Fs::format(&img, 32768).expect("format");
    fs.commit().expect("commit");
    // Generates deferred frees (btree CoW on every insert).
    for i in 0..20 {
        let name = format!("f{i:03}");
        let ino = fs
            .create(ROOT_INO, name.as_bytes(), 0o644, 0, 0)
            .expect("create");
        fs.write(ino, 0, &[i as u8; 1024]).expect("write");
    }
    fs.commit().expect("commit");
    fs.check().expect("live fsck clean");
    let free_before = fs.free_block_count();
    drop(fs);

    let fs = Fs::open(&img).expect("open");
    fs.check().expect("fsck clean after reopen");
    // The deferred queue was applied on open; free count must not drift.
    let free_after = fs.free_block_count();
    assert!(
        free_after >= free_before,
        "free count drifted: before={free_before} after={free_after}"
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

/// 32896 blocks => bitmap_bytes = 4112 => 4080 bytes of real padding =
/// room for 509 queue entries. Overwriting 800 files frees 800 data
/// blocks in one generation, overflowing it.
#[test]
fn overflow_stays_consistent() {
    let img = unique_img("overflow");
    let mut fs = Fs::format(&img, 32896).expect("format");
    fs.commit().expect("commit");
    let mut inos = Vec::new();
    for i in 0..800 {
        let name = format!("g{i:04}");
        let ino = fs
            .create(ROOT_INO, name.as_bytes(), 0o644, 0, 0)
            .expect("create");
        fs.write(ino, 0, &[0xAAu8; 4096]).expect("write");
        inos.push(ino);
    }
    fs.commit().expect("commit");
    // Overwrite every file: the old data blocks were committed, so each
    // overwrite CoW-frees one block into the deferred queue (> 509).
    for (i, ino) in inos.iter().enumerate() {
        fs.write(*ino, 0, &vec![(i & 0xFF) as u8; 4096])
            .expect("overwrite");
    }
    fs.commit().expect("commit");
    fs.check().expect("live fsck clean");
    drop(fs);

    let fs = Fs::open(&img).expect("open");
    fs.check().expect("fsck clean after reopen");
    drop(fs);
    let _ = std::fs::remove_file(&img);
}
