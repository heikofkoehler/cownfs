//! P8 gate: error paths and protocol edges, exercised over the wire.
//!
//! Every failure here must be a clean NFS status (or a bounded decode
//! error) — never a panic, hang, or dropped connection. After each
//! malformed input the server must still answer the next compound.

#[path = "common/mod.rs"]
mod common;

use common::{attr_u32, establish_client, spawn_server, NfsClient, Ops, Reply};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;

#[test]
fn getfh_without_cfh_is_nofilehandle() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.getfh();
    let (overall, res) = c.call(b"getfh-nocfh", ops);
    assert_eq!(overall, NFS4ERR_NOFILEHANDLE);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_NOFILEHANDLE)));
}

#[test]
fn bad_filehandle_fails_boundedly() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Garbage magic: the whole COMPOUND fails to decode -> BADXDR,
    // zero op results.
    let mut w = cownfs_nfs::xdr::Writer::new();
    w.opaque(&[0xFFu8; 32]);
    let mut ops = Ops::new();
    ops.putfh_raw(&w.into_bytes());
    let (overall, res) = c.call(b"bad-magic", ops);
    assert_eq!(overall, NFS4ERR_BADXDR);
    assert!(res.is_empty());

    // Well-formed handle, foreign fs uuid -> INVAL on the op.
    let mut ops = Ops::new();
    ops.putfh(&[0u8; 16], ROOT_INO);
    let (overall, res) = c.call(b"foreign-uuid", ops);
    assert_eq!(overall, NFS4ERR_INVAL);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_INVAL)));

    // The server is still alive.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getfh();
    let res = c.check_ok(b"alive", ops);
    assert!(matches!(res[1], Reply::Fh(_)));
}

#[test]
fn dot_and_dotdot() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Build root/d/sub.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o755);
    c.check_ok(b"mkdir-d", ops);
    let d = c.lookup_fh(&srv.uuid, ROOT_INO, b"d").unwrap();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, d.inode);
    ops.create_dir(b"sub", 0o755);
    c.check_ok(b"mkdir-sub", ops);
    let sub = c.lookup_fh(&srv.uuid, d.inode, b"sub").unwrap();

    // LOOKUP "." keeps the cfh.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, d.inode);
    ops.lookup(b".");
    ops.getfh();
    let res = c.check_ok(b"dot", ops);
    match &res[2] {
        Reply::Fh(fh) => assert_eq!(fh.inode, d.inode),
        r => panic!("{r:?}"),
    }

    // LOOKUP ".." and LOOKUPP at root stay at root.
    for (tag, use_lookup) in [
        (b"dotdot-root".as_slice(), true),
        (b"lookupp-root".as_slice(), false),
    ] {
        let mut ops = Ops::new();
        ops.putfh(&srv.uuid, ROOT_INO);
        if use_lookup {
            ops.lookup(b"..");
        } else {
            ops.lookupp();
        }
        ops.getfh();
        let res = c.check_ok(tag, ops);
        match &res[2] {
            Reply::Fh(fh) => assert_eq!(fh.inode, ROOT_INO),
            r => panic!("{r:?}"),
        }
    }

    // LOOKUPP from sub lands on d.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, sub.inode);
    ops.lookupp();
    ops.getfh();
    let res = c.check_ok(b"lookupp-sub", ops);
    match &res[2] {
        Reply::Fh(fh) => assert_eq!(fh.inode, d.inode),
        r => panic!("{r:?}"),
    }
}

#[test]
fn empty_getattr_mask_returns_type() {
    // Regression test: macOS probes fh validity with an empty GETATTR mask.
    // An empty reply makes xnu mark the vnode type VNON -> ESTALE, so the
    // server answers TYPE for an empty mask.
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getattr(&[]);
    let res = c.check_ok(b"empty-mask-root", ops);
    match &res[1] {
        Reply::Attrs(a) => assert_eq!(attr_u32(a, FATTR4_TYPE), NF4DIR),
        r => panic!("{r:?}"),
    }

    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);
    let attrs = c.getattr(&srv.uuid, f, &[]).unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_TYPE), NF4REG);
}

#[test]
fn compound_short_circuits_on_first_error() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"no-such-file");
    ops.getattr(&[FATTR4_TYPE]); // must not execute
    let (overall, res) = c.call(b"short-circuit", ops);
    assert_eq!(overall, NFS4ERR_NOENT);
    assert_eq!(res.len(), 2, "trailing ops must not run: {res:?}");
    assert!(matches!(res[0], Reply::Ok));
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOENT)));
}

#[test]
fn readdir_pagination() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"pg", 0o755);
    c.check_ok(b"mkdir-pg", ops);
    let pg = c.lookup_fh(&srv.uuid, ROOT_INO, b"pg").unwrap();
    for i in 0..10 {
        let name = format!("f{i:02}");
        common::create_file(&mut c, &srv.uuid, id, pg.inode, name.as_bytes(), 0o644);
    }

    // Tiny maxcount forces truncation; the server always emits at least one
    // entry per call. Walk by cookie and collect everything.
    let entries = c.readdir_all(&srv.uuid, pg.inode, 128, &[FATTR4_TYPE]);
    assert_eq!(entries.len(), 10);
    let mut names: Vec<String> = entries
        .iter()
        .map(|e| String::from_utf8(e.name.clone()).unwrap())
        .collect();
    names.sort();
    let expect: Vec<String> = (0..10).map(|i| format!("f{i:02}")).collect();
    assert_eq!(names, expect);
    // Cookies strictly increase across the walk.
    let cookies: Vec<u64> = entries.iter().map(|e| e.cookie).collect();
    assert!(cookies.windows(2).all(|w| w[0] < w[1]));

    // One shot with a huge maxcount gets everything at once.
    let entries = c.readdir_all(&srv.uuid, pg.inode, u32::MAX, &[FATTR4_TYPE]);
    assert_eq!(entries.len(), 10);

    // A cookie past the end yields nothing.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, pg.inode);
    ops.readdir(999, 65536, &[FATTR4_TYPE]);
    let res = c.check_ok(b"readdir-past-end", ops);
    match &res[1] {
        Reply::Dir { entries: e, .. } => assert!(e.is_empty()),
        r => panic!("{r:?}"),
    }
}

#[test]
fn read_past_eof() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let data = vec![0xABu8; 100];
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, &data);
    c.check_ok(b"write", ops);

    // Entirely past EOF: empty data, eof set.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.read(1000, 100);
    let res = c.check_ok(b"read-past-eof", ops);
    match &res[1] {
        Reply::Read { eof, data } => {
            assert!(*eof);
            assert!(data.is_empty());
        }
        r => panic!("{r:?}"),
    }

    // Straddling EOF: short read, eof set.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.read(90, 100);
    let res = c.check_ok(b"read-straddle", ops);
    match &res[1] {
        Reply::Read { eof, data } => {
            assert!(*eof);
            assert_eq!(data.len(), 10);
            assert_eq!(data, &vec![0xABu8; 10]);
        }
        r => panic!("{r:?}"),
    }
}

#[test]
fn access_bits() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.access(0x3f);
    let res = c.check_ok(b"access-all", ops);
    match &res[1] {
        Reply::Access { supported, granted } => {
            assert_eq!(*supported, 0x3f);
            assert_eq!(*granted, 0x3f);
        }
        r => panic!("{r:?}"),
    }
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.access(0);
    let res = c.check_ok(b"access-none", ops);
    match &res[1] {
        Reply::Access { granted, .. } => assert_eq!(*granted, 0),
        r => panic!("{r:?}"),
    }
}

#[test]
fn open_errors() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");

    // OPEN without CREATE on a missing name -> NOENT.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(id, b"o", 3, b"missing");
    let (overall, res) = c.call(b"open-missing", ops);
    assert_eq!(overall, NFS4ERR_NOENT);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOENT)));

    // Non-CLAIM_NULL claims are unsupported.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open(id, b"o", 3, OPEN4_NOCREATE, 0, &[], 1, b"");
    let (overall, res) = c.call(b"open-claim", ops);
    assert_eq!(overall, NFS4ERR_NOTSUPP);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOTSUPP)));

    // cfh must be a directory.
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.open_create(id, b"o", 3, b"g", 0o644);
    let (overall, res) = c.call(b"open-in-file", ops);
    assert_eq!(overall, NFS4ERR_NOTDIR);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOTDIR)));
}

#[test]
fn create_errors() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Regular files are created via OPEN, not CREATE: NF4REG via CREATE
    // is NFS4ERR_BADTYPE.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_raw(NF4REG, None, b"f");
    let (overall, res) = c.call(b"create-reg", ops);
    assert_eq!(overall, NFS4ERR_BADTYPE);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_BADTYPE)));

    // Unsupported ftype (e.g. socket) -> NOTSUPP.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_raw(7, None, b"g"); // 7 = NF4SOCK, not supported
    let (overall, res) = c.call(b"create-badftype", ops);
    assert_eq!(overall, NFS4ERR_NOTSUPP);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOTSUPP)));

    // CREATE over an existing name -> EXIST (RFC 7530 15.3).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o755);
    ops.create_dir(b"d", 0o755);
    let (overall, res) = c.call(b"create-existing", ops);
    assert_eq!(overall, NFS4ERR_EXIST);
    assert!(matches!(res[0], Reply::Ok));
    assert!(matches!(res[1], Reply::Ok));
    assert!(matches!(res[2], Reply::Err(NFS4ERR_EXIST)));
}

#[test]
fn remove_nonempty_dir_fails() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o755);
    c.check_ok(b"mkdir", ops);
    let d = c.lookup_fh(&srv.uuid, ROOT_INO, b"d").unwrap();
    common::create_file(&mut c, &srv.uuid, id, d.inode, b"f", 0o644);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.remove(b"d");
    let (overall, res) = c.call(b"rmdir-nonempty", ops);
    assert_eq!(overall, NFS4ERR_NOTSUPP);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOTSUPP)));

    // The directory is untouched.
    assert!(c.lookup_fh(&srv.uuid, ROOT_INO, b"d").is_ok());
}

#[test]
fn setattr_unsupported_attr() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p8");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.setattr(&[(FATTR4_TYPE, common::av_u32(NF4REG))]);
    let (overall, res) = c.call(b"setattr-type", ops);
    assert_eq!(overall, NFS4ERR_NOTSUPP);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_NOTSUPP)));
}

#[test]
fn state_error_paths() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let v = [7u8; 8];

    // Confirming an unknown client id.
    let mut ops = Ops::new();
    ops.confirm(999, &v);
    let (overall, res) = c.call(b"confirm-unknown", ops);
    assert_eq!(overall, NFS4ERR_STALE_CLIENTID);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_STALE_CLIENTID)));

    // Renewing an unknown client id.
    let mut ops = Ops::new();
    ops.renew(999);
    let (overall, res) = c.call(b"renew-unknown", ops);
    assert_eq!(overall, NFS4ERR_EXPIRED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_EXPIRED)));

    // Closing a bogus stateid.
    let mut ops = Ops::new();
    ops.close(1, &[0xFFu8; 16]);
    let (overall, res) = c.call(b"close-bogus", ops);
    assert_eq!(overall, NFS4ERR_EXPIRED);
    assert!(matches!(res[0], Reply::Err(NFS4ERR_EXPIRED)));
}

#[test]
fn unknown_op_and_bad_minor() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    // Unknown op number -> NOTSUPP, no op results.
    let mut ops = Ops::new();
    ops.bogus_op(999);
    let (overall, res) = c.call(b"bogus-op", ops);
    assert_eq!(overall, NFS4ERR_NOTSUPP);
    assert!(res.is_empty());

    // minorversion != 0 -> decode error -> SERVERFAULT, server survives.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getfh();
    let (overall, res) = c.call_raw(b"bad-minor", 1, ops.finish());
    assert_eq!(overall, NFS4ERR_SERVERFAULT);
    assert!(res.is_empty());

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getfh();
    let res = c.check_ok(b"alive", ops);
    assert!(matches!(res[1], Reply::Fh(_)));
}

#[test]
fn rpc_null_probe() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    c.rpc_null();
    // And the connection still serves compounds afterwards.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.getfh();
    c.check_ok(b"after-null", ops);
}

#[test]
fn write_to_symlink_is_inval_and_target_intact() {
    // T2 follow-up: WRITE to a symlink filehandle must not corrupt the
    // link target (POSIX makes write-to-symlink unrepresentable; RFC 7530
    // WRITE is for regular files). Expect NFS4ERR_INVAL.
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let _cid = establish_client(&mut c, b"p8-symwrite");

    // CREATE the symlink, then LOOKUP it to put its fh in cfh.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_symlink(b"s", b"original-target");
    c.check_ok(b"mksymlink", ops);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"s");
    ops.write(0, FILE_SYNC4, b"CORRUPT");
    let (overall, res) = c.call(b"write-symlink", ops);
    assert_eq!(overall, NFS4ERR_INVAL);
    assert!(matches!(res[2], Reply::Err(NFS4ERR_INVAL)));

    // The target is intact: READ the symlink fh returns the original path.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"s");
    ops.read(0, 64);
    let res = c.check_ok(b"read-symlink", ops);
    match &res[2] {
        Reply::Read { data, .. } => assert_eq!(data, b"original-target"),
        r => panic!("read: unexpected reply {r:?}"),
    }

    // WRITE to a directory fh is INVAL too.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.write(0, FILE_SYNC4, b"x");
    let (overall, res) = c.call(b"write-dir", ops);
    assert_eq!(overall, NFS4ERR_INVAL);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_INVAL)));
    // The server still serves after the rejected WRITEs.
}
