//! P57: NFS wire-level quota enforcement for non-root UIDs.
//!
//! Verifies that per-UID block quotas are enforced through the NFS wire
//! path when the file owner is a non-zero UID (set via FATTR4_OWNER).
//! This complements p37_quota.rs which only exercises UID 0.

#[path = "common/mod.rs"]
mod common;

use common::{
    av_string, av_u32, establish_client, spawn_server_with_quotas, NfsClient, Ops, Reply,
};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-quota-wire-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

/// Create a file with an explicit OWNER attribute (as a UID string).
fn create_file_as(
    c: &mut NfsClient,
    uuid: &[u8; 16],
    clientid: u64,
    dir_ino: u64,
    name: &[u8],
    mode: u32,
    owner_uid: u32,
) -> u64 {
    let mut ops = Ops::new();
    ops.putfh(uuid, dir_ino);
    // OPEN with CREATE, passing MODE and OWNER createattrs.
    ops.open(
        clientid,
        b"owner",
        3, // share_access READ|WRITE
        OPEN4_CREATE,
        UNCHECKED4,
        &[
            (FATTR4_MODE, av_u32(mode)),
            (FATTR4_OWNER, av_string(owner_uid.to_string().as_bytes())),
        ],
        0, // CLAIM_NULL
        name,
    );
    ops.getfh();
    let res = c.check_ok(b"create_file_as", ops);
    match &res[2] {
        Reply::Fh(fh) => fh.inode,
        r => panic!("create_file_as: unexpected reply {r:?}"),
    }
}

#[test]
fn quota_wire_uid1000_enforced() {
    let img = test_image();
    // UID 1000 limited to 100 blocks.
    let srv = spawn_server_with_quotas(&img, &[(1000, 100)]);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"quota-wire");

    // Create a file owned by UID 1000 (1 inode block).
    let ino = create_file_as(&mut c, &srv.uuid, id, ROOT_INO, b"q1000", 0o644, 1000);

    // Write 50 blocks (200 KiB) -> UID 1000 usage = 51. Within quota.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(0, FILE_SYNC4, &vec![1u8; 50 * 4096]);
    c.check_ok(b"write-within-quota", ops);

    // Write 50 more blocks -> usage would be 101 > 100. Must fail DQUOT.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write((50 * 4096) as u64, FILE_SYNC4, &vec![2u8; 50 * 4096]);
    let (status, _) = c.call(b"write-over-quota", ops);
    assert_eq!(
        status, NFS4ERR_DQUOT,
        "expected NFS4ERR_DQUOT when exceeding UID 1000 quota"
    );

    // A smaller write that fits (49 blocks -> usage 100, exactly at limit)
    // should succeed.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write((50 * 4096) as u64, FILE_SYNC4, &vec![3u8; 49 * 4096]);
    c.check_ok(b"write-at-limit", ops);

    // One more block past the limit must fail.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write((99 * 4096) as u64, FILE_SYNC4, &[4u8; 4096]);
    let (status, _) = c.call(b"write-one-over", ops);
    assert_eq!(status, NFS4ERR_DQUOT);

    let _ = std::fs::remove_file(&img);
}

#[test]
fn quota_wire_other_uid_unaffected() {
    let img = test_image();
    // Only UID 1000 is limited. UID 2000 has no quota.
    let srv = spawn_server_with_quotas(&img, &[(1000, 10)]);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"quota-wire2");

    // File owned by UID 2000 (no quota): large write should succeed.
    let ino = create_file_as(&mut c, &srv.uuid, id, ROOT_INO, b"q2000", 0o644, 2000);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(0, FILE_SYNC4, &vec![7u8; 50 * 4096]);
    c.check_ok(b"write-no-quota-uid", ops);

    let _ = std::fs::remove_file(&img);
}
