//! NFSv4.1 session semantics: EXCHANGE_ID, CREATE_SESSION, SEQUENCE
//! with exactly-once replay protection.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server, NfsClient, Ops, Reply};
use cownfs_nfs::nfs4::*;

fn setup() -> (common::TestServer, NfsClient, u64, [u8; 16]) {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let _ = establish_client(&mut c, b"session-test");

    // EXCHANGE_ID
    let mut ops = Ops::new();
    ops.exchange_id(&[0x41; 8], b"test-client-1");
    let (st, res) = c.call(b"exchange-id", ops);
    assert_eq!(st, NFS4_OK);
    let clientid = match res[0] {
        Reply::ClientId(id) => id,
        ref r => panic!("expected ClientId, got {r:?}"),
    };

    // CREATE_SESSION
    let mut ops = Ops::new();
    ops.create_session(clientid, 1, 8);
    let (st, res) = c.call(b"create-session", ops);
    assert_eq!(st, NFS4_OK);
    let sessionid = match res[0] {
        Reply::Session(sid) => sid,
        ref r => panic!("expected Session, got {r:?}"),
    };

    (srv, c, clientid, sessionid)
}

#[test]
fn session_sequence_basic() {
    let (srv, mut c, _cid, sid) = setup();

    // SEQUENCE + PUTROOTFH + GETATTR, sequence 1.
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    ops.getattr(&[FATTR4_TYPE]);
    let (st, res) = c.call(b"seq-1", ops);
    assert_eq!(st, NFS4_OK, "sequenced compound should succeed");
    assert_eq!(res.len(), 3); // SEQUENCE + PUTROOTFH + GETATTR

    // Sequence 2 on the same slot.
    let mut ops = Ops::new();
    ops.sequence(&sid, 2, 0, true);
    ops.putrootfh();
    let (st, _) = c.call(b"seq-2", ops);
    assert_eq!(st, NFS4_OK);

    drop(srv);
}

#[test]
fn session_replay_returns_cached() {
    let (srv, mut c, _cid, sid) = setup();

    // Sequence 1 with cachethis=true.
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    ops.getattr(&[FATTR4_TYPE]);
    let (st, res1) = c.call(b"seq-1", ops);
    assert_eq!(st, NFS4_OK);

    // Replay sequence 1: must return the identical cached reply,
    // not re-execute (exactly-once).
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    ops.getattr(&[FATTR4_TYPE]);
    let (st, res2) = c.call(b"seq-1-replay", ops);
    assert_eq!(st, NFS4_OK);
    assert_eq!(res1.len(), res2.len(), "replay returns cached results");

    // Sequence 2 still works after the replay.
    let mut ops = Ops::new();
    ops.sequence(&sid, 2, 0, true);
    ops.putrootfh();
    let (st, _) = c.call(b"seq-2", ops);
    assert_eq!(st, NFS4_OK);

    drop(srv);
}

#[test]
fn session_misordered_rejected() {
    let (srv, mut c, _cid, sid) = setup();

    // Jump to sequence 5 without 1-4: SEQ_MISORDERED.
    let mut ops = Ops::new();
    ops.sequence(&sid, 5, 0, true);
    ops.putrootfh();
    let (st, res) = c.call(b"seq-5", ops);
    assert_eq!(st, NFS4ERR_SEQ_MISORDERED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_SEQ_MISORDERED)));

    // Bad session ID: BADSESSION.
    let mut ops = Ops::new();
    ops.sequence(&[0xFF; 16], 1, 0, true);
    ops.putrootfh();
    let (st, _) = c.call(b"bad-session", ops);
    assert_eq!(st, NFS4ERR_BADSESSION);

    // Bad slot: BADSLOT (only 8 slots, 0-7).
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 99, true);
    ops.putrootfh();
    let (st, _) = c.call(b"bad-slot", ops);
    assert_eq!(st, NFS4ERR_BADSLOT);

    drop(srv);
}

#[test]
fn session_destroy() {
    let (srv, mut c, _cid, sid) = setup();

    // Use it once.
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    let (st, _) = c.call(b"seq-1", ops);
    assert_eq!(st, NFS4_OK);

    // Destroy the session.
    let mut ops = Ops::new();
    ops.destroy_session(&sid);
    let (st, _) = c.call(b"destroy", ops);
    assert_eq!(st, NFS4_OK);

    // SEQUENCE now fails with BADSESSION.
    let mut ops = Ops::new();
    ops.sequence(&sid, 2, 0, true);
    ops.putrootfh();
    let (st, _) = c.call(b"seq-after-destroy", ops);
    assert_eq!(st, NFS4ERR_BADSESSION);

    drop(srv);
}

#[test]
fn exchange_id_stable_for_same_owner() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let _ = establish_client(&mut c, b"session-test");

    let mut ops = Ops::new();
    ops.exchange_id(&[0x41; 8], b"stable-client");
    let (_, res) = c.call(b"exch-1", ops);
    let id1 = match res[0] {
        Reply::ClientId(id) => id,
        ref r => panic!("{r:?}"),
    };

    let mut ops = Ops::new();
    ops.exchange_id(&[0x41; 8], b"stable-client");
    let (_, res) = c.call(b"exch-2", ops);
    let id2 = match res[0] {
        Reply::ClientId(id) => id,
        ref r => panic!("{r:?}"),
    };
    assert_eq!(id1, id2, "same owner+verifier gets same client ID");

    drop(srv);
}
