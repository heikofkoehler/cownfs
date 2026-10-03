use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn setup() -> (std::path::PathBuf, Fs) {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-xattr-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 1024).unwrap();
    fs.commit().unwrap();
    (img, fs)
}

#[test]
fn set_get_list_remove() {
    let (_img, mut fs) = setup();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();

    // Not set initially.
    assert_eq!(fs.getxattr(ino, b"user.tag").unwrap(), None);
    assert!(fs.listxattrs(ino).unwrap().is_empty());

    // Set and get.
    fs.setxattr(ino, b"user.tag", b"hello").unwrap();
    assert_eq!(fs.getxattr(ino, b"user.tag").unwrap(), Some(b"hello".to_vec()));

    // List.
    fs.setxattr(ino, b"user.other", b"world").unwrap();
    let mut names = fs.listxattrs(ino).unwrap();
    names.sort();
    assert_eq!(names, vec![b"user.other".to_vec(), b"user.tag".to_vec()]);

    // Remove.
    assert!(fs.removexattr(ino, b"user.tag").unwrap());
    assert_eq!(fs.getxattr(ino, b"user.tag").unwrap(), None);
    assert!(!fs.removexattr(ino, b"user.tag").unwrap());

    fs.commit().unwrap();
}

#[test]
fn persists_across_reopen() {
    let (img, mut fs) = setup();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.setxattr(ino, b"user.key", b"value123").unwrap();
    fs.commit().unwrap();
    drop(fs);

    let fs = Fs::open(&img).unwrap();
    assert_eq!(
        fs.getxattr(ino, b"user.key").unwrap(),
        Some(b"value123".to_vec())
    );
}

#[test]
fn xattrs_dropped_on_unlink() {
    let (_img, mut fs) = setup();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.setxattr(ino, b"user.tag", b"x").unwrap();
    fs.unlink(ROOT_INO, b"f").unwrap();
    // Inode is gone; getxattr should fail.
    assert!(fs.getxattr(ino, b"user.tag").is_err());
    fs.commit().unwrap();
}

#[test]
fn hidden_from_readdir() {
    let (_img, mut fs) = setup();
    fs.setxattr(ROOT_INO, b"user.r", b"v").unwrap();
    let names: Vec<Vec<u8>> = fs.readdir(ROOT_INO).unwrap().into_iter().map(|(n, _, _)| n).collect();
    assert!(!names.iter().any(|n| n == b".xattrs"));
}
