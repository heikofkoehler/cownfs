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

    // Wait for expiry.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!fs.lease_check("node1").unwrap());

    // Node2 can now take over.
    assert!(fs.lease_acquire("node2", 60).unwrap());

    drop(fs);
    std::fs::remove_file(&img).ok();
}
