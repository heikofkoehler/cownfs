//! P11 gate: NFSv4 state edge cases over the wire.
//!
//! OPEN/CLOSE seqid replay, share-reservation conflicts between two
//! clientids, LOCK conflict detection and release, lease renewal, and
//! client reincarnation. All clientids share one TCP connection: the
//! server serves connections one at a time.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, establish_client_verifier, spawn_server, NfsClient, Ops, Reply};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;

fn open_stateid(r: &Reply) -> [u8; 16] {
    match r {
        Reply::Open { stateid } => *stateid,
        r => panic!("expected Open reply, got {r:?}"),
    }
}

fn lock_stateid(r: &Reply) -> [u8; 16] {
    match r {
        Reply::Lock { stateid } => *stateid,
        r => panic!("expected Lock reply, got {r:?}"),
    }
}

/// OPEN (no create) `name` under root with the given owner/flags; returns the
/// reply for the OPEN op.
fn open_file(
    c: &mut NfsClient,
    uuid: &[u8; 16],
    clientid: u64,
    owner: &[u8],
    flags: u32,
    name: &[u8],
) -> Reply {
    let mut ops = Ops::new();
    ops.putfh(uuid, ROOT_INO);
    ops.open_nocreate(clientid, owner, flags, name);
    let res = c.check_ok(b"open", ops);
    match res.into_iter().nth(1) {
        Some(r) => r,
        None => panic!("open produced no reply"),
    }
}

#[test]
fn open_two_owners_two_stateids() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p11");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);
    let _ = f;

    let s1 = open_stateid(&open_file(&mut c, &srv.uuid, id, b"o1", 3, b"f"));
    let s2 = open_stateid(&open_file(&mut c, &srv.uuid, id, b"o2", 3, b"f"));
    assert_ne!(s1, s2, "distinct owners need distinct stateids");

    for s in [s1, s2] {
        let mut ops = Ops::new();
        ops.close(1, &s);
        c.check_ok(b"close", ops);
    }

    // Reopening with a recycled owner mints a fresh stateid.
    let s3 = open_stateid(&open_file(&mut c, &srv.uuid, id, b"o1", 3, b"f"));
    assert_ne!(s1, s3);
    let mut ops = Ops::new();
    ops.close(1, &s3);
    c.check_ok(b"close3", ops);
}

#[test]
fn close_replay_and_bad_seqid() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p11");
    common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);
    let s = open_stateid(&open_file(&mut c, &srv.uuid, id, b"o", 3, b"f"));

    // CLOSE with the current seqid (0) is a replay: success, state kept.
    let mut ops = Ops::new();
    ops.close(0, &s);
    c.check_ok(b"close-replay", ops);

    // The real CLOSE.
    let mut ops = Ops::new();
    ops.close(1, &s);
    c.check_ok(b"close", ops);

    // The stateid is gone now: closing again is EXPIRED, not a replay.
    let mut ops = Ops::new();
    ops.close(1, &s);
    let (overall, res) = c.call(b"close-after", ops);
    assert_eq!(overall, NFS4ERR_EXPIRED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_EXPIRED)));

    // A skipped seqid is rejected.
    let s2 = open_stateid(&open_file(&mut c, &srv.uuid, id, b"o", 3, b"f"));
    let mut ops = Ops::new();
    ops.close(7, &s2);
    let (overall, res) = c.call(b"close-bad-seqid", ops);
    assert_eq!(overall, NFS4ERR_BAD_SEQID);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_BAD_SEQID)));
}

#[test]
fn share_deny_conflict_between_clients() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let ida = establish_client(&mut c, b"ca");
    let idb = establish_client(&mut c, b"cb");
    common::create_file(&mut c, &srv.uuid, ida, ROOT_INO, b"f", 0o644);

    // A opens read/write but denies writers.
    let deny_write = 3 | (2 << 4);
    let sa = open_stateid(&open_file(&mut c, &srv.uuid, ida, b"oa", deny_write, b"f"));

    // B wants write access -> DENIED.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(idb, b"ob", 2, b"f");
    let (overall, res) = c.call(b"open-deny-write", ops);
    assert_eq!(overall, NFS4ERR_DENIED);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_DENIED)));

    // B wants read-only access -> allowed.
    let sb = open_stateid(&open_file(&mut c, &srv.uuid, idb, b"ob", 1, b"f"));

    let mut ops = Ops::new();
    ops.close(1, &sa);
    c.check_ok(b"close-a", ops);
    let mut ops = Ops::new();
    ops.close(1, &sb);
    c.check_ok(b"close-b", ops);
}

#[test]
fn open_with_unconfirmed_client_is_stale() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"owner");
    common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    // Register a second client but never confirm it.
    let mut ops = Ops::new();
    ops.setclientid(&[1u8; 8], b"unconfirmed");
    let res = c.check_ok(b"setclientid", ops);
    let pending = match &res[0] {
        Reply::ClientId(id) => *id,
        r => panic!("{r:?}"),
    };
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(pending, b"o", 3, b"f");
    let (overall, res) = c.call(b"open-unconfirmed", ops);
    assert_eq!(overall, NFS4ERR_STALE_CLIENTID);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_STALE_CLIENTID)));
}

#[test]
fn lock_conflict_and_unlock() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let ida = establish_client(&mut c, b"ca");
    let idb = establish_client(&mut c, b"cb");
    common::create_file(&mut c, &srv.uuid, ida, ROOT_INO, b"f", 0o644);
    let sa = open_stateid(&open_file(&mut c, &srv.uuid, ida, b"oa", 3, b"f"));
    let sb = open_stateid(&open_file(&mut c, &srv.uuid, idb, b"ob", 3, b"f"));
    // cfh is the file after the last OPEN; LOCK operates on the cfh.

    let mut ops = Ops::new();
    ops.lock_new(ida, WRITE_LT, 0, 100, &sa, b"la");
    let res = c.check_ok(b"lock-a", ops);
    let sla = lock_stateid(&res[0]);

    // Overlapping WRITE from another owner -> LOCKED.
    let mut ops = Ops::new();
    ops.lock_new(idb, WRITE_LT, 50, 100, &sb, b"lb");
    let (overall, res) = c.call(b"lock-conflict", ops);
    assert_eq!(overall, NFS4ERR_LOCKED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_LOCKED)));

    // Disjoint range is fine.
    let mut ops = Ops::new();
    ops.lock_new(idb, WRITE_LT, 200, 100, &sb, b"lb");
    let res = c.check_ok(b"lock-b", ops);
    let slb = lock_stateid(&res[0]);

    for (sid, off, len) in [(sla, 0, 100), (slb, 200, 100)] {
        let mut ops = Ops::new();
        ops.locku(WRITE_LT, 1, &sid, off, len);
        c.check_ok(b"locku", ops);
    }

    // After unlock, B can take the range A held.
    let mut ops = Ops::new();
    ops.lock_new(idb, WRITE_LT, 0, 100, &sb, b"lb2");
    c.check_ok(b"relock", ops);
}

#[test]
fn read_locks_do_not_conflict() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let ida = establish_client(&mut c, b"ca");
    let idb = establish_client(&mut c, b"cb");
    common::create_file(&mut c, &srv.uuid, ida, ROOT_INO, b"f", 0o644);
    let sa = open_stateid(&open_file(&mut c, &srv.uuid, ida, b"oa", 3, b"f"));
    let sb = open_stateid(&open_file(&mut c, &srv.uuid, idb, b"ob", 3, b"f"));

    let mut ops = Ops::new();
    ops.lock_new(ida, READ_LT, 0, 100, &sa, b"la");
    let res = c.check_ok(b"read-lock-a", ops);
    let sla = lock_stateid(&res[0]);

    let mut ops = Ops::new();
    ops.lock_new(idb, READ_LT, 50, 100, &sb, b"lb");
    let res = c.check_ok(b"read-lock-b", ops);
    let slb = lock_stateid(&res[0]);

    for (sid, off, len) in [(sla, 0, 100), (slb, 50, 100)] {
        let mut ops = Ops::new();
        ops.locku(READ_LT, 1, &sid, off, len);
        c.check_ok(b"locku", ops);
    }
}

#[test]
fn renew_and_client_reincarnation() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.renew(424242);
    let (overall, res) = c.call(b"renew-unknown", ops);
    assert_eq!(overall, NFS4ERR_EXPIRED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_EXPIRED)));

    let v1 = [0x11u8; 8];
    let v2 = [0x22u8; 8];
    let id = establish_client_verifier(&mut c, &v1, b"cc");

    let mut ops = Ops::new();
    ops.renew(id);
    c.check_ok(b"renew", ops);

    // Same name, new verifier: reincarnation keeps the clientid but drops
    // confirmation. The old verifier no longer confirms.
    let mut ops = Ops::new();
    ops.setclientid(&v2, b"cc");
    let res = c.check_ok(b"reincarnate", ops);
    match &res[0] {
        Reply::ClientId(new_id) => assert_eq!(*new_id, id),
        r => panic!("{r:?}"),
    }
    let mut ops = Ops::new();
    ops.confirm(id, &v1);
    let (overall, res) = c.call(b"confirm-old", ops);
    assert_eq!(overall, NFS4ERR_STALE_CLIENTID);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_STALE_CLIENTID)));

    let mut ops = Ops::new();
    ops.confirm(id, &v2);
    c.check_ok(b"confirm-new", ops);
}
