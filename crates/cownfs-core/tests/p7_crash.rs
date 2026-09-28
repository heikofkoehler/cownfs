//! P7 gate: deterministic crash injection across every commit boundary.
//!
//! For each FaultPoint, we:
//! 1. Create a filesystem, write data, commit cleanly (generation N).
//! 2. Write more data, arm the fault, attempt commit (aborts).
//! 3. Drop the Fs without retrying (simulating SIGKILL).
//! 4. Reopen: must see either generation N or N+1, never torn state.
//! 5. `check()` must pass on the reopened image.

use cownfs_core::engine::{FaultPoint, Fs, FsError, ROOT_INO};

fn write_file(fs: &mut Fs, parent: u64, name: &[u8], data: &[u8]) -> u64 {
    let ino = fs.create(parent, name, 0o644, 0, 0).unwrap();
    fs.write(ino, 0, data).unwrap();
    ino
}

fn read_file(fs: &Fs, ino: u64) -> Vec<u8> {
    let size = fs.getattr(ino).unwrap().size;
    fs.read(ino, 0, size as usize).unwrap()
}

#[test]
fn p7_crash_injection() {
    let points = [
        FaultPoint::AfterFlush,
        FaultPoint::AfterBitmap,
        FaultPoint::AfterSync,
    ];

    for point in points {
        let dir = std::env::temp_dir();
        let img = dir.join(format!(
            "cownfs-p7-crash-{:?}-{}.img",
            point,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&img);

        // Generation 1: clean commit.
        let mut fs = Fs::format(&img, 8192).unwrap();
        let root = ROOT_INO;
        let ino_a = write_file(&mut fs, root, b"stable.txt", b"stable-data-v1");
        fs.commit().unwrap();
        let gen1 = fs.generation();
        drop(fs);

        // Generation 2: write new data, arm fault, commit aborts.
        let mut fs = Fs::open(&img).unwrap();
        assert_eq!(fs.generation(), gen1);
        // Verify stable data is intact before the fault.
        assert_eq!(read_file(&fs, ino_a), b"stable-data-v1");
        // Write new file (uncommitted).
        let _ino_b = write_file(&mut fs, root, b"new.txt", b"new-data-v2");
        fs.set_fault_point(point);
        let res = fs.commit();
        assert!(
            matches!(res, Err(FsError::InjectedFault(p)) if p == point),
            "expected InjectedFault({point:?}), got {res:?}"
        );
        // Simulate SIGKILL: drop without retrying or cleaning up.
        drop(fs);

        // Reopen: must recover to a consistent generation.
        let fs = Fs::open(&img).unwrap();
        let gen_after = fs.generation();
        // Either the old generation (crash before flip) or the new one
        // (crash after flip but before we observed it — not possible here
        // since we abort before the flip, but the check is general).
        assert!(
            gen_after == gen1 || gen_after == gen1 + 1,
            "generation went backwards or jumped: {gen1} -> {gen_after}"
        );
        // The stable file must always be intact.
        assert_eq!(read_file(&fs, ino_a), b"stable-data-v1");
        // Full consistency check must pass.
        let _report = fs.check().expect("check failed after crash");
        println!("Crash at {point:?}: recovered to gen {gen_after}, check clean");

        // If we recovered to gen1, the new file must not be visible
        // (it was never committed). If gen1+1, it must be intact.
        if gen_after == gen1 {
            assert!(fs.lookup(root, b"new.txt").unwrap().is_none());
        } else {
            let (ino_b2, _) = fs.lookup(root, b"new.txt").unwrap().unwrap();
            assert_eq!(read_file(&fs, ino_b2), b"new-data-v2");
        }

        drop(fs);
        std::fs::remove_file(&img).unwrap();
    }
}
