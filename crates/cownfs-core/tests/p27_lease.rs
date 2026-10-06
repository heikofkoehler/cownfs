//! Leader lease: fencing prevents split-brain.

use cownfs_core::engine::Fs;

#[test]
fn lease_acquire_and_fence() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-lease-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 256).unwrap();

    // Node1 acquires.
    assert!(fs.lease_acquire("node1", 60).unwrap());
    assert!(fs.lease_check("node1").unwrap());

    // Node2 cannot acquire while node1 holds it.
    assert!(!fs.lease_acquire("node2", 60).unwrap());
    assert!(!fs.lease_check("node2").unwrap());

    // Node1 renews.
    assert!(fs.lease_renew("node1", 60).unwrap());

    // Node1 releases.
    fs.lease_release("node1").unwrap();
    assert!(!fs.lease_check("node1").unwrap());

    // Now node2 can acquire.
    assert!(fs.lease_acquire("node2", 60).unwrap());
    assert!(fs.lease_check("node2").unwrap());

    drop(fs);
    std::fs::remove_file(&img).ok();
}

#[test]
fn lease_expires() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-lease-exp-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 256).unwrap();

    // Acquire with 1-second TTL.
    assert!(fs.lease_acquire("node1", 1).unwrap());
    assert!(fs.lease_check("node1").unwrap());

    // Wait for expiry + grace period (LEASE_GRACE_SECS=10; the old holder
    // may still have in-flight writes).
    std::thread::sleep(std::time::Duration::from_secs(12));
    assert!(!fs.lease_check("node1").unwrap());

    // Node2 can now take over.
    assert!(fs.lease_acquire("node2", 60).unwrap());

    drop(fs);
    std::fs::remove_file(&img).ok();
}

/// P0: exclusive-open lock — two writers cannot open the same image.
#[test]
fn p0_exclusive_open_lock() {
    let img = std::env::temp_dir().join(format!(
        "cownfs-excl-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&img);
    let lock_path = {
        let mut p = img.as_os_str().to_owned();
        p.push(".lock");
        std::path::PathBuf::from(p)
    };
    let _ = std::fs::remove_file(&lock_path);

    let fs1 = cownfs_core::engine::Fs::format(&img, 1024).expect("first format");

    // Second writer open must fail.
    match cownfs_core::engine::Fs::open(&img) {
        Ok(_) => panic!("second writer open should fail"),
        Err(cownfs_core::engine::FsError::Locked(_)) => {} // expected
        Err(e) => panic!("wrong error: {e:?}"),
    }

    drop(fs1);
    // After first writer drops, second writer can open.
    let _fs2 = cownfs_core::engine::Fs::open(&img).expect("open after drop");

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&lock_path);
}
