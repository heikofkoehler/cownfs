//! R7: feature-flag format versioning.
//!
//! Exit criteria:
//! 1. Open an image with an unknown `incompat` flag → clean refusal.
//! 2. Unknown `ro_compat` flag → open succeeds but the FS is read-only
//!    (mutations refused).
//! 3. Legacy (pre-R7, 256-byte header) images still open.
//! 4. Unknown `compat` flags are ignored.

use std::path::PathBuf;

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::engine::{Fs, FsError, ROOT_INO};
use cownfs_core::superblock;

fn tmp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "cownfs-r7-{tag}-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Set feature flags on both superblock slots of `img`.
fn set_flags(img: &std::path::Path, compat: u64, ro_compat: u64, incompat: u64) {
    let dev = FileDevice::open(img).expect("open dev");
    for slot in 0..superblock::SLOT_BLOCKS.len() {
        let mut sb = superblock::read_slot(&dev, slot).expect("read slot");
        sb.compat = compat;
        sb.ro_compat = ro_compat;
        sb.incompat = incompat;
        superblock::write_slot(&dev, slot, &sb).expect("write slot");
    }
    dev.sync().expect("sync");
}

/// R7.1: unknown incompat flag → clean refusal to open.
#[test]
fn r7_unknown_incompat_refused() {
    let img = tmp_path("incompat");
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 2048).expect("format");

    // Sanity: opens fine before flagging.
    drop(Fs::open(&img).expect("open"));

    set_flags(&img, 0, 0, 0x1); // unknown incompat bit 0
    match Fs::open(&img) {
        Err(FsError::IncompatibleFeature(_)) => {}
        Err(e) => panic!("expected IncompatibleFeature, got {e}"),
        Ok(_) => panic!("expected IncompatibleFeature, opened successfully"),
    }

    let _ = std::fs::remove_file(&img);
}

/// R7.2: unknown ro_compat flag → read-only open; mutations refused.
#[test]
fn r7_unknown_ro_compat_read_only() {
    let img = tmp_path("rocompat");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).expect("format");
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.write(ino, 0, b"data").expect("write");
    fs.commit().expect("commit");
    drop(fs);

    set_flags(&img, 0, 0x4, 0); // unknown ro_compat bit 2

    // Opens fine...
    let mut fs = Fs::open(&img).expect("open with unknown ro_compat");
    // ...reads work...
    let (found, _) = fs.lookup(ROOT_INO, b"f").expect("lookup").expect("found");
    assert_eq!(fs.read(found, 0, 4).expect("read"), b"data");
    // ...but mutations are refused.
    assert!(matches!(
        fs.create(ROOT_INO, b"g", 0o644, 0, 0),
        Err(FsError::ReadOnly)
    ));
    assert!(matches!(fs.write(found, 0, b"x"), Err(FsError::ReadOnly)));
    assert!(matches!(fs.commit(), Err(FsError::ReadOnly)));
    assert!(matches!(fs.snapshot_create(b"s"), Err(FsError::ReadOnly)));
    assert!(matches!(fs.lease_acquire("n", 60), Err(FsError::ReadOnly)));

    let _ = std::fs::remove_file(&img);
}

/// R7.3: unknown compat flags are ignored (open RW, mutations work).
#[test]
fn r7_unknown_compat_ignored() {
    let img = tmp_path("compat");
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 2048).expect("format");

    set_flags(&img, 0x8000_0000_0000_0000, 0, 0); // unknown compat bit 63

    let mut fs = Fs::open(&img).expect("open with unknown compat");
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.write(ino, 0, b"ok").expect("write");
    fs.commit().expect("commit");

    let _ = std::fs::remove_file(&img);
}

/// R7.4: known flags (all zero) behave exactly as before.
#[test]
fn r7_zero_flags_normal() {
    let img = tmp_path("zero");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).expect("format");
    assert_eq!(fs.generation(), 1);
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.commit().expect("commit");
    drop(fs);

    // Reopen: flags are zero, RW.
    let mut fs = Fs::open(&img).expect("open");
    fs.write(ino, 0, b"w").expect("write");
    fs.commit().expect("commit");

    let _ = std::fs::remove_file(&img);
}

/// R7.5: flags survive a commit round-trip (written into every superblock).
#[test]
fn r7_flags_persist_across_commit() {
    let img = tmp_path("persist");
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 2048).expect("format");

    // Set a compat flag (ignored) and commit; the flag must still be there.
    set_flags(&img, 0x2, 0, 0);
    let mut fs = Fs::open(&img).expect("open");
    fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.commit().expect("commit");
    drop(fs);

    let dev = FileDevice::open(&img).expect("open dev");
    let (sb, _) = superblock::open(&dev).expect("superblock open");
    assert_eq!(sb.compat, 0x2, "compat flags must survive commits");
    assert_eq!(sb.ro_compat, 0);
    assert_eq!(sb.incompat, 0);

    let _ = std::fs::remove_file(&img);
}

/// R7.6: legacy 256-byte headers (pre-R7 images) still decode, flags=0.
#[test]
fn r7_legacy_header_still_opens() {
    use cownfs_core::checksum::checksum;

    let img = tmp_path("legacy");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).expect("format");
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.write(ino, 0, b"legacy").expect("write");
    fs.commit().expect("commit");
    drop(fs);

    // Rewrite both slots with legacy 256-byte headers: for each slot,
    // copy the first 256 bytes, zero the flag area, recompute the
    // 256-byte checksum. Each slot keeps its own generation.
    let dev = FileDevice::open(&img).expect("open dev");
    for slot_block in [0u64, 1u64] {
        let mut blk = [0u8; 4096];
        dev.read_block(slot_block, &mut blk).expect("read slot");
        let mut legacy = [0u8; 4096];
        legacy[..256].copy_from_slice(&blk[..256]);
        // Recompute checksum over 256 bytes with the checksum field zeroed.
        legacy[64..72].fill(0);
        let sum = checksum(&legacy[..256]);
        legacy[64..72].copy_from_slice(&sum.to_le_bytes());
        dev.write_block(slot_block, &legacy)
            .expect("write legacy slot");
    }
    dev.sync().expect("sync");
    drop(dev);

    // Opens via the legacy decode path; flags read as zero.
    let fs = Fs::open(&img).expect("open legacy image");
    let (found, _) = fs.lookup(ROOT_INO, b"f").expect("lookup").expect("found");
    assert_eq!(fs.read(found, 0, 6).expect("read"), b"legacy");

    // And it's RW (no unknown flags).
    let mut fs = fs;
    fs.write(found, 0, b"ok!!").expect("write");
    fs.commit().expect("commit");

    let _ = std::fs::remove_file(&img);
}
