//! P10 gate: data-path round-trips over the wire.
//!
//! Sparse writes, overwrites, stable-write levels, cross-directory renames,
//! hard-link data sharing, and symlink reads — all verified by reading the
//! bytes back through NFS.

#[path = "common/mod.rs"]
mod common;

use common::{attr_u32, attr_u64, establish_client, spawn_server, NfsClient, Ops, Reply};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;

#[test]
fn sparse_write_reads_back_zeros() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"sparse", 0o644);

    // Write past EOF; the gap must read back as zeros.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(10000, UNSTABLE4, b"hello");
    let res = c.check_ok(b"sparse-write", ops);
    match &res[1] {
        Reply::Written { count, committed } => {
            assert_eq!(*count, 5);
            assert_eq!(*committed, UNSTABLE4);
        }
        r => panic!("{r:?}"),
    }

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.getattr(&[FATTR4_SIZE]);
    ops.read(9995, 10);
    let res = c.check_ok(b"sparse-read", ops);
    match &res[1] {
        Reply::Attrs(a) => assert_eq!(attr_u64(a, FATTR4_SIZE), 10005),
        r => panic!("{r:?}"),
    }
    match &res[2] {
        // 5 zero gap bytes, then "hello"; count is not past EOF here.
        Reply::Read { data, .. } => assert_eq!(data, b"\0\0\0\0\0hello"),
        r => panic!("{r:?}"),
    }

    // COMMIT makes it durable; data is identical afterwards.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.commit();
    c.check_ok(b"commit", ops);
    assert_eq!(c.read_all(&srv.uuid, f, 10005)[10000..], b"hello"[..]);
}

#[test]
fn overwrite_middle_of_file() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let orig: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, &orig);
    c.check_ok(b"write-orig", ops);

    let patch = vec![0xAAu8; 20];
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(50, FILE_SYNC4, &patch);
    c.check_ok(b"write-patch", ops);

    let back = c.read_all(&srv.uuid, f, 200);
    assert_eq!(&back[..50], &orig[..50]);
    assert_eq!(&back[50..70], &patch[..]);
    assert_eq!(&back[70..], &orig[70..]);
}

#[test]
fn write_reports_requested_stability() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    for (stable, expect) in [
        (UNSTABLE4, UNSTABLE4),
        (DATA_SYNC4, DATA_SYNC4),
        (FILE_SYNC4, FILE_SYNC4),
    ] {
        let mut ops = Ops::new();
        ops.putfh(&srv.uuid, f);
        ops.write(0, stable, b"x");
        let res = c.check_ok(b"write-stable", ops);
        match &res[1] {
            Reply::Written { committed, .. } => assert_eq!(*committed, expect),
            r => panic!("{r:?}"),
        }
    }
}

#[test]
fn rename_across_directories() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"a", 0o755);
    ops.create_dir(b"b", 0o755);
    c.check_ok(b"mkdirs", ops);
    let a = c.lookup_fh(&srv.uuid, ROOT_INO, b"a").unwrap();
    let b = c.lookup_fh(&srv.uuid, ROOT_INO, b"b").unwrap();

    let f = common::create_file(&mut c, &srv.uuid, id, a.inode, b"f", 0o644);
    let data = b"moved-bytes".to_vec();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, &data);
    c.check_ok(b"write", ops);

    // RENAME: cfh = source dir, saved_fh = destination dir.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, b.inode);
    ops.savefh();
    ops.putfh(&srv.uuid, a.inode);
    ops.rename(b"f", b"g");
    c.check_ok(b"rename", ops);

    assert_eq!(
        c.lookup_fh(&srv.uuid, a.inode, b"f").unwrap_err(),
        NFS4ERR_NOENT
    );
    let g = c.lookup_fh(&srv.uuid, b.inode, b"g").unwrap();
    assert_eq!(g.inode, f, "rename must move the inode, not copy it");
    assert_eq!(c.read_all(&srv.uuid, g.inode, data.len() as u64), data);
}

#[test]
fn rename_over_existing_file() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");

    let f1 = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f1", 0o644);
    let f2 = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f2", 0o644);
    for (ino, data) in [(f1, b"one".as_slice()), (f2, b"two".as_slice())] {
        let mut ops = Ops::new();
        ops.putfh(&srv.uuid, ino);
        ops.write(0, FILE_SYNC4, data);
        c.check_ok(b"write", ops);
    }

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.rename(b"f1", b"f2");
    c.check_ok(b"rename-over", ops);

    assert_eq!(
        c.lookup_fh(&srv.uuid, ROOT_INO, b"f1").unwrap_err(),
        NFS4ERR_NOENT
    );
    let g = c.lookup_fh(&srv.uuid, ROOT_INO, b"f2").unwrap();
    assert_eq!(g.inode, f1);
    assert_eq!(c.read_all(&srv.uuid, g.inode, 3), b"one");
}

#[test]
fn hardlink_shares_data() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, b"shared");
    c.check_ok(b"write", ops);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.savefh();
    ops.putfh(&srv.uuid, f);
    ops.link(b"g");
    c.check_ok(b"link", ops);
    let g = c.lookup_fh(&srv.uuid, ROOT_INO, b"g").unwrap();
    assert_eq!(g.inode, f);

    // Writes through one name are visible through the other.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, g.inode);
    ops.write(6, FILE_SYNC4, b"-and-more");
    c.check_ok(b"write-via-link", ops);
    assert_eq!(c.read_all(&srv.uuid, f, 15), b"shared-and-more");

    // Removing the original leaves the link (and data) intact.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.remove(b"f");
    c.check_ok(b"remove-orig", ops);
    assert_eq!(c.read_all(&srv.uuid, g.inode, 15), b"shared-and-more");
}

#[test]
fn remove_empty_dir() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o755);
    ops.remove(b"d");
    c.check_ok(b"rmdir", ops);
    assert_eq!(
        c.lookup_fh(&srv.uuid, ROOT_INO, b"d").unwrap_err(),
        NFS4ERR_NOENT
    );
}

#[test]
fn symlink_read_returns_target() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_symlink(b"s", b"some/target/path");
    c.check_ok(b"symlink", ops);

    let s = c.lookup_fh(&srv.uuid, ROOT_INO, b"s").unwrap();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, s.inode);
    ops.getattr(&[FATTR4_TYPE]);
    ops.read(0, 64);
    let res = c.check_ok(b"readlink", ops);
    match &res[1] {
        Reply::Attrs(a) => assert_eq!(attr_u32(a, FATTR4_TYPE), NF4LNK),
        r => panic!("{r:?}"),
    }
    match &res[2] {
        Reply::Read { data, .. } => assert_eq!(data, b"some/target/path"),
        r => panic!("{r:?}"),
    }
}

#[test]
fn lookupp_chain_returns_to_root() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"a", 0o755);
    c.check_ok(b"mkdir-a", ops);
    let a = c.lookup_fh(&srv.uuid, ROOT_INO, b"a").unwrap();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, a.inode);
    ops.create_dir(b"b", 0o755);
    c.check_ok(b"mkdir-b", ops);
    let b = c.lookup_fh(&srv.uuid, a.inode, b"b").unwrap();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, b.inode);
    ops.create_dir(b"c", 0o755);
    c.check_ok(b"mkdir-c", ops);
    let cc = c.lookup_fh(&srv.uuid, b.inode, b"c").unwrap();

    // Walk back up three levels; must land on root.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, cc.inode);
    ops.lookupp();
    ops.lookupp();
    ops.lookupp();
    ops.getfh();
    let res = c.check_ok(b"lookupp-chain", ops);
    match &res[4] {
        Reply::Fh(fh) => assert_eq!(fh.inode, ROOT_INO),
        r => panic!("{r:?}"),
    }
}

#[test]
fn commit_is_idempotent() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p10");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.commit();
    ops.commit();
    c.check_ok(b"commit-twice", ops);
}
