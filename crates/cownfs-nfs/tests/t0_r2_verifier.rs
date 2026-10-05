//! T0-R2: repro for the NFS write verifier bug.
//!
//! The server returned `[0u8; 8]` for WRITE/COMMIT verifiers. Clients compare
//! the WRITE verifier with the COMMIT verifier to detect server restart
//! (which loses UNSTABLE writes). With a constant zero verifier, clients
//! cannot detect restarts.
//!
//! This test MUST FAIL on unfixed main (verifiers are equal).

use cownfs_core::engine::Fs;
use cownfs_nfs::server::Shared;

#[test]
fn r2_boot_verifier_changes_on_restart() {
    let img = std::env::temp_dir().join("t0-r2-verifier.img");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 256).unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Simulate two server boots (restarts).
    let fs1 = Fs::open(&img).unwrap();
    let shared1 = Shared::new(fs1);
    let v1 = shared1.boot_verifier;

    let fs2 = Fs::open(&img).unwrap();
    let shared2 = Shared::new(fs2);
    let v2 = shared2.boot_verifier;

    // Verifiers must differ (random per boot). With the bug, both are zeros.
    assert_ne!(v1, v2, "R2: boot verifier must change on restart");
    // And must not be all zeros (the buggy value).
    assert_ne!(v1, [0u8; 8], "R2: boot verifier must not be zeros");
    assert_ne!(v2, [0u8; 8], "R2: boot verifier must not be zeros");

    let _ = std::fs::remove_file(&img);
}
