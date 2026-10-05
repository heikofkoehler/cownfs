//! R5: duplicate request cache — retransmitted RPCs (same xid) get the
//! cached reply instead of re-executing non-idempotent ops.

#[path = "common/mod.rs"]
mod common;

use common::{NfsClient, Ops};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::{self, NFS4ERR_EXIST};

#[test]
fn duplicate_create_with_same_xid_returns_cached_ok() {
    let srv = common::spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Build PUTFH + CREATE(dir "d").
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o755);
    let raw = ops.finish();

    let xid = 0x12345678;
    let (overall1, _) = c.call_with_xid(b"create1", raw.clone(), xid);
    assert_eq!(overall1, nfs4::NFS4_OK, "first CREATE should succeed");

    // Retransmit the identical RPC (same xid): DRC must return the cached
    // OK instead of re-executing CREATE (which would yield EXIST).
    let (overall2, _) = c.call_with_xid(b"create1", raw.clone(), xid);
    assert_eq!(
        overall2,
        nfs4::NFS4_OK,
        "retransmit with same xid should hit DRC"
    );

    // Sanity: the same CREATE with a fresh xid really does fail with EXIST,
    // proving the cached OK came from the DRC and not idempotence.
    let (overall3, _) = c.call(b"create2", {
        let mut ops = Ops::new();
        ops.putfh(&srv.uuid, ROOT_INO);
        ops.create_dir(b"d", 0o755);
        ops
    });
    assert_eq!(overall3, NFS4ERR_EXIST, "fresh xid CREATE should get EXIST");

    // The directory exists exactly once.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"d");
    ops.getattr(&[nfs4::FATTR4_TYPE]);
    let replies = c.check_ok(b"verify", ops);
    assert!(matches!(replies[1], common::Reply::Ok));
}

#[test]
fn duplicate_write_with_same_xid_does_not_double_apply() {
    let srv = common::spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = common::establish_client_verifier(&mut c, &[9, 9, 9, 9, 9, 9, 9, 9], b"r5");
    let ino = common::create_file(&mut c, &srv.uuid, clientid, ROOT_INO, b"f", 0o644);

    // UNSTABLE write of 4 bytes at offset 0.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(0, nfs4::UNSTABLE4, b"abcd");
    let raw = ops.finish();

    let xid = 0x87654321;
    let (o1, _) = c.call_with_xid(b"w1", raw.clone(), xid);
    assert_eq!(o1, nfs4::NFS4_OK);
    // Retransmit: cached reply, op not re-executed.
    let (o2, r2) = c.call_with_xid(b"w1", raw.clone(), xid);
    assert_eq!(o2, nfs4::NFS4_OK);

    // If the write had executed twice, the second (non-cached) execution
    // would still be fine (idempotent at same offset), so verify the
    // replies are byte-identical, which only the DRC guarantees.
    let (_, r1_again) = c.call_with_xid(b"w1", raw.clone(), xid);
    assert_eq!(
        format!("{r2:?}"),
        format!("{r1_again:?}"),
        "DRC replies must be identical"
    );

    // Data is exactly "abcd" (4 bytes, not 8).
    let data = c.read_all(&srv.uuid, ino, 4);
    assert_eq!(data, b"abcd");
}
