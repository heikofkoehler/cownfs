//! T9: NFS fault tests — wire-level fault injection.
//!
//! Arms deterministic fault points on the running server and verifies:
//! - the NFS op fails with NFS4ERR_IO (simulated disk failure),
//! - the server stays up and serves subsequent requests,
//! - a retry after the (one-shot) fault succeeds and data is intact.

#[path = "common/mod.rs"]
mod common;

use common::{NfsClient, Ops};
use cownfs_core::engine::{FaultPoint, ROOT_INO};
use cownfs_nfs::nfs4::{self, FILE_SYNC4, NFS4ERR_IO};

/// Create file `name` in root via OPEN_CREATE + GETFH and FILE_SYNC-write `data`.
/// Returns the overall compound status of the WRITE.
fn write_file(c: &mut NfsClient, uuid: &[u8; 16], clientid: u64, name: &[u8], data: &[u8]) -> u32 {
    let ino = common::create_file(c, uuid, clientid, ROOT_INO, name, 0o644);
    let mut ops = Ops::new();
    ops.putfh(uuid, ino);
    ops.write(0, FILE_SYNC4, data);
    let (overall, _) = c.call(b"write", ops);
    overall
}

#[test]
fn fault_after_flush_returns_io_and_server_survives() {
    let srv = common::spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = common::establish_client_verifier(&mut c, &[1, 2, 3, 4, 5, 6, 7, 8], b"t9");

    srv.arm_fault(FaultPoint::AfterFlush);
    let overall = write_file(&mut c, &srv.uuid, clientid, b"f", b"hello");
    assert_eq!(
        overall, NFS4ERR_IO,
        "faulted WRITE should return NFS4ERR_IO"
    );

    // Server still alive: GETATTR works.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getattr(&[nfs4::FATTR4_TYPE]);
    c.check_ok(b"alive", ops);

    // Retry (fault is one-shot) succeeds and data is intact.
    let overall = write_file(&mut c, &srv.uuid, clientid, b"g", b"hello");
    assert_eq!(overall, nfs4::NFS4_OK, "retry after fault should succeed");
    // LOOKUP sets cfh; READ directly.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"g");
    ops.read(0, 5);
    let replies = c.check_ok(b"read-back", ops);
    match &replies[2] {
        common::Reply::Read { data, .. } => assert_eq!(data, b"hello"),
        r => panic!("read: unexpected reply {r:?}"),
    }
}

#[test]
fn fault_after_bitmap_returns_io() {
    let srv = common::spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = common::establish_client_verifier(&mut c, &[1, 2, 3, 4, 5, 6, 7, 8], b"t9");

    srv.arm_fault(FaultPoint::AfterBitmap);
    let overall = write_file(&mut c, &srv.uuid, clientid, b"f", b"world");
    assert_eq!(
        overall, NFS4ERR_IO,
        "faulted WRITE should return NFS4ERR_IO"
    );

    // Server still alive and retry succeeds.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getattr(&[nfs4::FATTR4_TYPE]);
    c.check_ok(b"alive", ops);
    let overall = write_file(&mut c, &srv.uuid, clientid, b"g", b"world");
    assert_eq!(overall, nfs4::NFS4_OK, "retry after fault should succeed");
}
