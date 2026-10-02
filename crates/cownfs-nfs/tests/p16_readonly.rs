//! P16 gate: read-only server mode (Phase 1a of horizontal scaling).
//!
//! A read-only server rejects every mutating op with NFS4ERR_ROFS while
//! serving reads normally. This is the foundation for read replicas.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_read_only_server, NfsClient, Ops};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;

#[test]
fn read_only_rejects_mutations() {
    let srv = spawn_read_only_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"ro-test");

    // CREATE -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"newdir", 0o755);
    let (st, _) = c.call(b"ro-create", ops);
    assert_eq!(st, NFS4ERR_ROFS, "CREATE should be ROFS");

    // OPEN with CREATE -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open(id, b"o", 3, OPEN4_CREATE, UNCHECKED4, &[], 0, b"newfile");
    let (st, _) = c.call(b"ro-open-create", ops);
    assert_eq!(st, NFS4ERR_ROFS, "OPEN(CREATE) should be ROFS");

    // REMOVE -> ROFS (not NOENT — ROFS check comes first).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.remove(b"nonexistent");
    let (st, _) = c.call(b"ro-remove", ops);
    assert_eq!(st, NFS4ERR_ROFS, "REMOVE should be ROFS");

    // RENAME -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.rename(b"a", b"b");
    let (st, _) = c.call(b"ro-rename", ops);
    assert_eq!(st, NFS4ERR_ROFS, "RENAME should be ROFS");

    // WRITE -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.write(0, FILE_SYNC4, b"data");
    let (st, _) = c.call(b"ro-write", ops);
    assert_eq!(st, NFS4ERR_ROFS, "WRITE should be ROFS");

    // SETATTR -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.setattr(&[(FATTR4_MODE, 0o644u32.to_be_bytes().to_vec())]);
    let (st, _) = c.call(b"ro-setattr", ops);
    assert_eq!(st, NFS4ERR_ROFS, "SETATTR should be ROFS");

    // LINK -> ROFS.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.link(b"target");
    let (st, _) = c.call(b"ro-link", ops);
    assert_eq!(st, NFS4ERR_ROFS, "LINK should be ROFS");
}

#[test]
fn read_only_serves_reads() {
    let srv = spawn_read_only_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let _id = establish_client(&mut c, b"ro-read");

    // GETATTR on root works.
    let attrs = c
        .getattr(&srv.uuid, ROOT_INO, &[FATTR4_TYPE, FATTR4_FILEID])
        .expect("getattr on read-only");
    assert!(!attrs.is_empty());

    // READDIR on root works (empty).
    let entries = c.readdir_all(&srv.uuid, ROOT_INO, 4096, &[FATTR4_FILEID]);
    assert_eq!(entries.len(), 0);

    // OPEN without CREATE works (on root dir — will fail with ISDIR, not ROFS).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(1, b"o", 1, b".");
    let (st, _) = c.call(b"ro-open-ro", ops);
    // "." lookup may fail, but it must NOT be ROFS.
    assert_ne!(st, NFS4ERR_ROFS, "read-only OPEN must not be ROFS");
}
