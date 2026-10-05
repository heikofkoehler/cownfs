//! P0.5 / B5: End-to-end backup/restore test.
//!
//! Full backup + three incrementals, with content comparison after every
//! restore stage and an external `cownfs-fsck` run (checked exit status).

use cownfs_core::engine::{Fs, ROOT_INO};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn backup_bin() -> String {
    env!("CARGO_BIN_EXE_cownfs-backup").to_string()
}
fn fsck_bin() -> String {
    // cownfs-fsck lives in a separate package; locate it via the target dir.
    let test_exe = std::env::current_exe().expect("current test exe");
    // test_exe is target/debug/deps/p54_backup_e2e-<hash>; binary is at target/debug/cownfs-fsck
    let target_debug = test_exe
        .parent() // deps/
        .and_then(|p| p.parent()) // debug/
        .expect("target dir");
    let fsck = target_debug.join("cownfs-fsck");
    assert!(
        fsck.exists(),
        "cownfs-fsck binary not found at {}; run `cargo build -p cownfs-fsck` first",
        fsck.display()
    );
    fsck.to_string_lossy().to_string()
}

fn run_backup(args: &[&str]) {
    let out = Command::new(backup_bin())
        .args(args)
        .output()
        .expect("spawn cownfs-backup");
    assert!(
        out.status.success(),
        "cownfs-backup {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_fsck(image: &Path) {
    let out = Command::new(fsck_bin())
        .arg(image)
        .output()
        .expect("spawn cownfs-fsck");
    assert!(
        out.status.success(),
        "cownfs-fsck {} failed: {}",
        image.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Read all regular files in the root directory: name -> content.
fn dir_contents(img: &Path) -> HashMap<Vec<u8>, Vec<u8>> {
    let fs = Fs::open(img).expect("open for contents");
    let mut map = HashMap::new();
    for (name, ino, _typ) in fs.readdir(ROOT_INO).expect("readdir") {
        if name == b"." || name == b".." {
            continue;
        }
        let data = fs.read(ino, 0, 1 << 20).expect("read");
        map.insert(name, data);
    }
    map
}

fn write_file(fs: &mut Fs, name: &[u8], data: &[u8]) {
    // Remove if exists, then create fresh.
    let _ = fs.unlink(ROOT_INO, name);
    let ino = fs.create(ROOT_INO, name, 0o644, 0, 0).expect("create");
    fs.write(ino, 0, data).expect("write");
    fs.commit().expect("commit");
}

#[test]
fn backup_e2e_full_plus_three_incrementals() {
    // Ensure the fsck binary is fresh (separate package; cargo test -p
    // cownfs-nfs does not build it; a stale fsck fails on new formats).
    let build = std::process::Command::new("cargo")
        .args(["build", "-p", "cownfs-fsck"])
        .status()
        .expect("cargo build cownfs-fsck");
    assert!(build.success(), "cargo build -p cownfs-fsck failed");

    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let img1 = dir.join(format!("cownfs-bak-e2e-{pid}-src.img"));
    let img2 = dir.join(format!("cownfs-bak-e2e-{pid}-dst.img"));
    let full_bak = dir.join(format!("cownfs-bak-e2e-{pid}-full.bak"));
    let inc1_bak = dir.join(format!("cownfs-bak-e2e-{pid}-inc1.bak"));
    let inc2_bak = dir.join(format!("cownfs-bak-e2e-{pid}-inc2.bak"));
    let inc3_bak = dir.join(format!("cownfs-bak-e2e-{pid}-inc3.bak"));
    for p in [&img1, &img2, &full_bak, &inc1_bak, &inc2_bak, &inc3_bak] {
        let _ = std::fs::remove_file(p);
    }

    // --- Stage 0: create source image with files. ---
    {
        let mut fs = Fs::format(&img1, 4096).expect("format");
        fs.commit().expect("commit");
        write_file(&mut fs, b"alpha", b"alpha-v1-content");
        write_file(&mut fs, b"beta", b"beta-v1-content");
        fs.snapshot_create(b"s0").expect("snap s0");
        fs.commit().expect("commit");
    }
    let stage0 = dir_contents(&img1);
    assert_eq!(stage0.len(), 2);

    // --- Full backup. ---
    run_backup(&["create", img1.to_str().unwrap(), full_bak.to_str().unwrap()]);
    run_backup(&["verify", full_bak.to_str().unwrap()]);

    // --- Stage 1: modify (update alpha, add gamma, delete beta). ---
    {
        let mut fs = Fs::open(&img1).expect("open");
        write_file(&mut fs, b"alpha", b"alpha-v2-UPDATED-content-longer");
        write_file(&mut fs, b"gamma", b"gamma-v1-new-file");
        fs.unlink(ROOT_INO, b"beta").expect("unlink beta");
        fs.commit().expect("commit");
        fs.snapshot_create(b"s1").expect("snap s1");
        fs.commit().expect("commit");
    }
    let stage1 = dir_contents(&img1);
    assert_eq!(stage1.len(), 2); // alpha, gamma (beta deleted)
    assert!(stage1.contains_key(b"alpha".as_slice()));
    assert!(stage1.contains_key(b"gamma".as_slice()));

    // --- Incremental 1 (since s0). ---
    run_backup(&[
        "create-inc",
        img1.to_str().unwrap(),
        "s0",
        inc1_bak.to_str().unwrap(),
    ]);
    run_backup(&["verify", inc1_bak.to_str().unwrap()]);

    // --- Restore full to img2, then apply inc1. ---
    run_backup(&[
        "restore",
        full_bak.to_str().unwrap(),
        img2.to_str().unwrap(),
    ]);
    run_fsck(&img2);
    assert_eq!(dir_contents(&img2), stage0, "full restore content mismatch");

    run_backup(&[
        "restore-inc",
        img2.to_str().unwrap(),
        inc1_bak.to_str().unwrap(),
    ]);
    run_fsck(&img2);
    assert_eq!(
        dir_contents(&img2),
        stage1,
        "stage1 content mismatch after restore-inc 1"
    );

    // --- Stage 2: more modifications. ---
    {
        let mut fs = Fs::open(&img1).expect("open");
        write_file(&mut fs, b"gamma", b"gamma-v2-changed-again-with-more-data");
        write_file(&mut fs, b"delta", b"delta-v1");
        fs.commit().expect("commit");
        fs.snapshot_create(b"s2").expect("snap s2");
        fs.commit().expect("commit");
    }
    let stage2 = dir_contents(&img1);
    assert_eq!(stage2.len(), 3); // alpha, gamma, delta

    // --- Incremental 2 (since s1). ---
    run_backup(&[
        "create-inc",
        img1.to_str().unwrap(),
        "s1",
        inc2_bak.to_str().unwrap(),
    ]);
    run_backup(&["verify", inc2_bak.to_str().unwrap()]);

    run_backup(&[
        "restore-inc",
        img2.to_str().unwrap(),
        inc2_bak.to_str().unwrap(),
    ]);
    run_fsck(&img2);
    assert_eq!(
        dir_contents(&img2),
        stage2,
        "stage2 content mismatch after restore-inc 2"
    );

    // --- Stage 3: final modifications (including a deletion). ---
    {
        let mut fs = Fs::open(&img1).expect("open");
        fs.unlink(ROOT_INO, b"alpha").expect("unlink alpha");
        write_file(&mut fs, b"epsilon", &vec![0x7eu8; 20000]);
        fs.commit().expect("commit");
    }
    let stage3 = dir_contents(&img1);
    assert_eq!(stage3.len(), 3); // gamma, delta, epsilon (alpha deleted)
    assert!(!stage3.contains_key(b"alpha".as_slice()));

    // --- Incremental 3 (since s2). ---
    run_backup(&[
        "create-inc",
        img1.to_str().unwrap(),
        "s2",
        inc3_bak.to_str().unwrap(),
    ]);
    run_backup(&["verify", inc3_bak.to_str().unwrap()]);

    run_backup(&[
        "restore-inc",
        img2.to_str().unwrap(),
        inc3_bak.to_str().unwrap(),
    ]);
    run_fsck(&img2);
    assert_eq!(
        dir_contents(&img2),
        stage3,
        "stage3 content mismatch after restore-inc 3"
    );

    // --- Negative tests: exact-base enforcement. ---
    // 1. Wrong image (different UUID).
    let img3 = dir.join(format!("cownfs-bak-e2e-{pid}-other.img"));
    let _ = std::fs::remove_file(&img3);
    {
        let mut fs = Fs::format(&img3, 4096).expect("format");
        fs.commit().expect("commit");
        fs.snapshot_create(b"s0").expect("snap");
        fs.commit().expect("commit");
    }
    let out = Command::new(backup_bin())
        .args([
            "restore-inc",
            img3.to_str().unwrap(),
            inc1_bak.to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "restore-inc should refuse a different-UUID image"
    );

    // 2. Corrupt backup (flip a byte in the data section).
    let corrupt_bak = dir.join(format!("cownfs-bak-e2e-{pid}-corrupt.bak"));
    std::fs::copy(&inc1_bak, &corrupt_bak).expect("copy");
    {
        let mut data = std::fs::read(&corrupt_bak).expect("read");
        // Flip a byte well into the changed-block data region.
        let off = data.len() * 3 / 4;
        data[off] ^= 0xff;
        std::fs::write(&corrupt_bak, &data).expect("write");
    }
    let out = Command::new(backup_bin())
        .args([
            "restore-inc",
            img2.to_str().unwrap(),
            corrupt_bak.to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "restore-inc should refuse a corrupt backup"
    );

    // Cleanup.
    for p in [
        &img1,
        &img2,
        &img3,
        &full_bak,
        &inc1_bak,
        &inc2_bak,
        &inc3_bak,
        &corrupt_bak,
    ] {
        let _ = std::fs::remove_file(p);
    }
}
