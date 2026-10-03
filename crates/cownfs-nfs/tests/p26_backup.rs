//! Backup/restore roundtrip.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::path::PathBuf;
use std::process::Command;

fn backup_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // deps -> debug
    p.pop();
    p.join("cownfs-backup")
}

#[test]
fn backup_restore_roundtrip() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let img = dir.join(format!("bkup-rt-{pid}.img"));
    let bak = dir.join(format!("bkup-rt-{pid}.bak"));
    let rst = dir.join(format!("bkup-rt-{pid}-restored.img"));
    for p in [&img, &bak, &rst] {
        let _ = std::fs::remove_file(p);
    }

    // Create image with data.
    let mut fs = Fs::format(&img, 1024).unwrap();
    let ino = fs.create(ROOT_INO, b"data", 0o644, 1000, 1000).unwrap();
    let data = vec![0x42u8; 10000];
    fs.write(ino, 0, &data).unwrap();
    fs.commit().unwrap();
    drop(fs);

    let bin = backup_bin();

    // Create backup.
    let out = Command::new(&bin)
        .args(["create", img.to_str().unwrap(), bak.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "create: {}", String::from_utf8_lossy(&out.stderr));

    // Verify.
    let out = Command::new(&bin)
        .args(["verify", bak.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "verify: {}", String::from_utf8_lossy(&out.stderr));

    // Restore.
    let out = Command::new(&bin)
        .args(["restore", bak.to_str().unwrap(), rst.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "restore: {}", String::from_utf8_lossy(&out.stderr));

    // Read back from restored image.
    let fs = Fs::open(&rst).unwrap();
    let out = fs.read(ino, 0, data.len()).unwrap();
    assert_eq!(out, data);

    for p in [&img, &bak, &rst] {
        let _ = std::fs::remove_file(p);
    }
}
