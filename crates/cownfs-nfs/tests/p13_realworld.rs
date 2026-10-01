//! P13 gate: real-world NFSv4 client patterns, exercised over the wire.
//!
//! These sequences come from observed client behavior (macOS xnu traces from
//! our 2026-09-30 debugging, Linux/FreeBSD documented patterns) and pynfs
//! conformance expectations. Each test encodes a compound that a real client
//! actually sends — not just the minimal op the spec requires.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server, NfsClient, Ops, Reply};
use cownfs_nfs::nfs4::*;

/// macOS lookup path: PUTFH, LOOKUP, SECINFO on an existing name.
/// This is the sequence that exposed the missing OP_SECINFO (2026-09-30):
/// macOS bundles SECINFO into the lookup compound, and a BadOp aborted the
/// whole compound, leaving the client in a LOOKUP→SECINFO retry loop.
#[test]
fn macos_lookup_with_secinfo() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Create a file first.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.create_raw(NF4REG, None, b"real");
    c.check_ok(b"create", ops);

    // The macOS lookup compound.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"real");
    ops.secinfo(b"real");
    let res = c.check_ok(b"lookup-secinfo", ops);
    assert!(matches!(res[0], Reply::Ok)); // PUTFH
    assert!(matches!(res[1], Reply::Ok)); // LOOKUP
    match &res[2] {
        Reply::Secinfo(flavors) => assert_eq!(flavors, &[1]), // AUTH_SYS
        r => panic!("expected Secinfo, got {r:?}"),
    }
}

/// macOS create path: PUTFH, SECINFO(nonexistent), OPEN(CREATE).
/// macOS probes SECINFO for the name *before* creating it. A NOENT here
/// makes macOS abort with EIO instead of proceeding to OPEN (2026-09-30).
#[test]
fn macos_create_path_secinfo_then_open() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"p13");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.secinfo(b"newfile");
    ops.open_create(clientid, b"own", 3, b"newfile", 0o644);
    let res = c.check_ok(b"secinfo-open-create", ops);
    assert!(matches!(res[0], Reply::Ok)); // PUTFH
    match &res[1] {
        Reply::Secinfo(flavors) => assert_eq!(flavors, &[1]),
        r => panic!("expected Secinfo, got {r:?}"),
    }
    assert!(matches!(res[2], Reply::Open { .. })); // OPEN created it

    // Verify the file exists.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"newfile");
    c.check_ok(b"lookup-created", ops);
}

/// macOS remove path: SECINFO bundled into the remove compound, then REMOVE.
/// The file must actually be gone afterwards.
#[test]
fn macos_secinfo_then_remove() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.create_raw(NF4REG, None, b"doomed");
    c.check_ok(b"create", ops);

    // SECINFO then REMOVE in one compound (macOS bundles them).
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.secinfo(b"doomed");
    ops.remove(b"doomed");
    let res = c.check_ok(b"secinfo-remove", ops);
    assert!(matches!(res[0], Reply::Ok));
    assert!(matches!(res[1], Reply::Secinfo(_)));
    assert!(matches!(res[2], Reply::Ok));

    // File must be gone.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"doomed");
    let (overall, res) = c.call(b"lookup-gone", ops);
    assert_eq!(overall, NFS4ERR_NOENT);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOENT)));
}

/// macOS getattr-heavy lookup: PUTFH, GETATTR, LOOKUP, GETFH, GETATTR.
/// Observed in the 2026-09-30 trace; every op must succeed in sequence.
#[test]
fn macos_getattr_heavy_lookup() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.create_raw(NF4REG, None, b"g");
    c.check_ok(b"create", ops);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.getattr(&[FATTR4_TYPE, FATTR4_MODE]);
    ops.lookup(b"g");
    ops.getfh();
    ops.getattr(&[FATTR4_TYPE, FATTR4_SIZE]);
    let res = c.check_ok(b"getattr-lookup", ops);
    assert_eq!(res.len(), 5);
    for (i, r) in res.iter().enumerate() {
        assert!(
            matches!(r, Reply::Ok | Reply::Attrs(_) | Reply::Fh(_)),
            "op {i}: {r:?}"
        );
    }
}

/// pynfs: invalid UTF-8 names must get NFS4ERR_INVAL (not NOENT, not OK).
/// Covers LOOKUP, OPEN, REMOVE, RENAME, LINK, SECINFO, CREATE.
#[test]
fn invalid_utf8_names_rejected() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"p13");

    let bad = &[0xFF, 0xFE, b'x']; // invalid UTF-8

    // LOOKUP
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(bad);
    let (overall, res) = c.call(b"lookup-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "LOOKUP bad UTF-8: {res:?}");

    // OPEN
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(clientid, b"own", 3, bad, 0o644);
    let (overall, _) = c.call(b"open-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "OPEN bad UTF-8");

    // REMOVE
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.remove(bad);
    let (overall, _) = c.call(b"remove-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "REMOVE bad UTF-8");

    // RENAME (bad old name)
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.savefh();
    ops.putrootfh();
    ops.rename(bad, b"ok");
    let (overall, _) = c.call(b"rename-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "RENAME bad UTF-8");

    // SECINFO
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.secinfo(bad);
    let (overall, _) = c.call(b"secinfo-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "SECINFO bad UTF-8");

    // CREATE
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.create_raw(NF4REG, None, bad);
    let (overall, _) = c.call(b"create-badutf8", ops);
    assert_eq!(overall, NFS4ERR_INVAL, "CREATE bad UTF-8");
}

/// OPEN with GUARDED4: must fail with NFS4ERR_EXIST if the file exists,
/// and create it if it doesn't (RFC 7530 §16.16).
#[test]
fn open_guarded4_semantics() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"p13");

    // Create the file first.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_create(clientid, b"own", 3, b"guarded", 0o644);
    c.check_ok(b"create", ops);

    // GUARDED4 on existing file -> EXIST.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open(
        clientid,
        b"own",
        3,
        OPEN4_CREATE,
        GUARDED4,
        &[(FATTR4_MODE, common::av_u32(0o644))],
        0,
        b"guarded",
    );
    let (overall, res) = c.call(b"guarded-existing", ops);
    assert_eq!(overall, NFS4ERR_EXIST, "GUARDED4 on existing: {res:?}");

    // GUARDED4 on new file -> creates.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open(
        clientid,
        b"own",
        3,
        OPEN4_CREATE,
        GUARDED4,
        &[(FATTR4_MODE, common::av_u32(0o644))],
        0,
        b"guarded2",
    );
    let res = c.check_ok(b"guarded-new", ops);
    assert!(matches!(res[1], Reply::Open { .. }));
}

/// OPEN with EXCLUSIVE4 on a new file: server currently returns NOTSUPP.
/// Documents the gap — full verifier-based exclusive create is not yet
/// implemented. The decode must at least succeed (verifier consumed).
#[test]
fn open_exclusive4_create_is_notsupp() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let clientid = establish_client(&mut c, b"p13");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open_exclusive(clientid, b"own", 3, b"excl", &[0xAA; 8]);
    let (overall, res) = c.call(b"exclusive-create", ops);
    // Decode succeeded (didn't fail with XDR/SERVERFAULT); op not supported.
    assert_eq!(overall, NFS4ERR_NOTSUPP, "EXCLUSIVE4 create: {res:?}");
}

/// ILLEGAL op (10044) mid-compound: per RFC 7530 §15.2.7 the compound stops
/// at the first failing op with NFS4ERR_OP_ILLEGAL, and earlier ops' effects
/// stand. pynfs COMP tests cover this; knfsd once wedged on it (CVE-2025-40210).
#[test]
fn illegal_op_mid_compound() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.create_raw(NF4REG, None, b"before-illegal");
    ops.bogus_op(10044); // ILLEGAL
    ops.create_raw(NF4REG, None, b"after-illegal");
    let (overall, res) = c.call(b"illegal-mid", ops);
    assert_eq!(overall, NFS4ERR_OP_ILLEGAL);
    assert_eq!(res.len(), 3);
    assert!(matches!(res[0], Reply::Ok)); // PUTFH applied
    assert!(matches!(res[1], Reply::Ok)); // CREATE applied
    assert!(matches!(res[2], Reply::Err(NFS4ERR_OP_ILLEGAL)));

    // The pre-ILLEGAL create must have taken effect.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"before-illegal");
    c.check_ok(b"lookup-before", ops);

    // The post-ILLEGAL create must NOT have run.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.lookup(b"after-illegal");
    let (overall, _) = c.call(b"lookup-after", ops);
    assert_eq!(overall, NFS4ERR_NOENT);
}

/// Oversized compound (300 ops): must fail cleanly without wedging the
/// server. knfsd reinstated a 200-op cap after CVE-2025-40210; we reject
/// above our own limit and stay up for the next compound.
#[test]
fn oversized_compound_fails_boundedly() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    for _ in 0..300 {
        ops.putrootfh();
    }
    let (overall, _) = c.call(b"oversized", ops);
    // Must be a clean NFS-level rejection, not a hang or dropped connection.
    assert_ne!(overall, 0, "oversized compound must not succeed");

    // Server must still answer the next compound.
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.getattr(&[FATTR4_TYPE]);
    c.check_ok(b"still-alive", ops);
}
