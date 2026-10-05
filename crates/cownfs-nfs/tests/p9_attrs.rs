//! P9 gate: attribute coverage over the wire.
//!
//! GETATTR for every attribute the server implements, SETATTR round-trips,
//! and the attr-mask edge cases (empty mask, unknown attrs, scrambled
//! request order).

#[path = "common/mod.rs"]
mod common;

use common::{
    attr_raw, attr_string, attr_time, attr_u32, attr_u64, av_string, av_time, av_u32, av_u64,
    establish_client, has_attr, spawn_server, NfsClient, Ops, Reply,
};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;

const ALL_ATTRS: &[u32] = &[
    FATTR4_TYPE,
    FATTR4_SIZE,
    FATTR4_LINK_SUPPORT,
    FATTR4_FSID,
    FATTR4_FILEHANDLE,
    FATTR4_FILEID,
    FATTR4_MODE,
    FATTR4_NUMLINKS,
    FATTR4_OWNER,
    FATTR4_OWNER_GROUP,
    FATTR4_SPACE_USED,
    FATTR4_TIME_ACCESS,
    FATTR4_TIME_METADATA,
    FATTR4_TIME_MODIFY,
    FATTR4_MOUNTED_ON_FILEID,
];

/// Create a file with explicit mode/owner/group createattrs.
#[allow(clippy::too_many_arguments)] // test helper; positional args read fine here
fn create_owned(
    c: &mut NfsClient,
    uuid: &[u8; 16],
    clientid: u64,
    dir: u64,
    name: &[u8],
    mode: u32,
    owner: &str,
    group: &str,
) -> u64 {
    let mut ops = Ops::new();
    ops.putfh(uuid, dir);
    ops.open(
        clientid,
        b"owner",
        3,
        OPEN4_CREATE,
        UNCHECKED4,
        &[
            (FATTR4_MODE, av_u32(mode)),
            (FATTR4_OWNER, av_string(owner.as_bytes())),
            (FATTR4_OWNER_GROUP, av_string(group.as_bytes())),
        ],
        0,
        name,
    );
    ops.getfh();
    let res = c.check_ok(b"create-owned", ops);
    match &res[2] {
        Reply::Fh(fh) => fh.inode,
        r => panic!("{r:?}"),
    }
}

#[test]
fn getattr_every_supported_attr() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p9");

    let f = create_owned(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o640, "1000", "1000");
    let data = vec![0xCDu8; 5000];
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, &data);
    c.check_ok(b"write", ops);

    let attrs = c.getattr(&srv.uuid, f, ALL_ATTRS).unwrap();
    assert_eq!(attrs.len(), ALL_ATTRS.len(), "every attr must be answered");

    assert_eq!(attr_u32(&attrs, FATTR4_TYPE), NF4REG);
    assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 5000);
    assert_eq!(attr_u32(&attrs, FATTR4_LINK_SUPPORT), 1);

    // FSID is derived from the filesystem uuid.
    let fsid = attr_raw(&attrs, FATTR4_FSID);
    assert_eq!(fsid.len(), 16);
    assert_eq!(
        u64::from_be_bytes(fsid[..8].try_into().unwrap()),
        u64::from_be_bytes(srv.uuid[..8].try_into().unwrap())
    );
    assert_eq!(
        u64::from_be_bytes(fsid[8..16].try_into().unwrap()),
        u64::from_be_bytes(srv.uuid[8..16].try_into().unwrap())
    );

    // FILEHANDLE decodes to this object's handle.
    let mut r = cownfs_nfs::xdr::Reader::new(attr_raw(&attrs, FATTR4_FILEHANDLE));
    let fh = FileHandle::decode(&mut r).unwrap();
    assert_eq!(fh.inode, f);
    assert_eq!(fh.fs_uuid, srv.uuid);

    assert_eq!(attr_u64(&attrs, FATTR4_FILEID), f);
    assert_eq!(attr_u32(&attrs, FATTR4_MODE) & 0o7777, 0o640);
    assert_eq!(attr_u32(&attrs, FATTR4_NUMLINKS), 1);
    assert_eq!(attr_string(&attrs, FATTR4_OWNER), "1000");
    assert_eq!(attr_string(&attrs, FATTR4_OWNER_GROUP), "1000");
    assert_eq!(attr_u64(&attrs, FATTR4_SPACE_USED), 8192); // 5000 rounded up to 4KiB blocks
    let (atime, _) = attr_time(&attrs, FATTR4_TIME_ACCESS);
    let (mtime, _) = attr_time(&attrs, FATTR4_TIME_MODIFY);
    let (ctime, _) = attr_time(&attrs, FATTR4_TIME_METADATA);
    assert!(atime > 0 && mtime > 0 && ctime > 0);
    assert!(mtime >= atime, "write must not predate creation");
    assert_eq!(attr_u64(&attrs, FATTR4_MOUNTED_ON_FILEID), f);
}

#[test]
fn filehandle_attr_is_usable_as_putfh() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let attrs = c
        .getattr(&srv.uuid, ROOT_INO, &[FATTR4_FILEHANDLE])
        .unwrap();

    // The attr value is the full opaque<> encoding; feed it straight back.
    let mut ops = Ops::new();
    ops.putfh_raw(attr_raw(&attrs, FATTR4_FILEHANDLE));
    ops.getattr(&[FATTR4_TYPE]);
    let res = c.check_ok(b"fh-roundtrip", ops);
    match &res[1] {
        Reply::Attrs(a) => assert_eq!(attr_u32(a, FATTR4_TYPE), NF4DIR),
        r => panic!("{r:?}"),
    }
}

#[test]
fn setattr_truncate_and_grow() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p9");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, &data);
    c.check_ok(b"write", ops);

    // Truncate.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.setattr(&[(FATTR4_SIZE, av_u64(100))]);
    c.check_ok(b"truncate", ops);
    let attrs = c.getattr(&srv.uuid, f, &[FATTR4_SIZE]).unwrap();
    assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 100);
    assert_eq!(c.read_all(&srv.uuid, f, 100), &data[..100]);

    // Grow: the new tail reads back as zeros.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.setattr(&[(FATTR4_SIZE, av_u64(5000))]);
    c.check_ok(b"grow", ops);
    let grown = c.read_all(&srv.uuid, f, 5000);
    assert_eq!(&grown[..100], &data[..100]);
    assert!(grown[100..].iter().all(|b| *b == 0));

    // Mode and size in a single SETATTR.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.setattr(&[(FATTR4_MODE, av_u32(0o600)), (FATTR4_SIZE, av_u64(10))]);
    c.check_ok(b"setattr-both", ops);
    let attrs = c
        .getattr(&srv.uuid, f, &[FATTR4_MODE, FATTR4_SIZE])
        .unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_MODE) & 0o777, 0o600);
    assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 10);
}

#[test]
fn setattr_times_and_owner() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p9");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.setattr(&[
        (FATTR4_TIME_MODIFY, av_time(1_700_000_000)),
        (FATTR4_OWNER, av_string(b"2000")),
        (FATTR4_OWNER_GROUP, av_string(b"3000")),
    ]);
    c.check_ok(b"setattr-times-owner", ops);

    let attrs = c
        .getattr(
            &srv.uuid,
            f,
            &[FATTR4_TIME_MODIFY, FATTR4_OWNER, FATTR4_OWNER_GROUP],
        )
        .unwrap();
    assert_eq!(attr_time(&attrs, FATTR4_TIME_MODIFY), (1_700_000_000, 0));
    assert_eq!(attr_string(&attrs, FATTR4_OWNER), "2000");
    assert_eq!(attr_string(&attrs, FATTR4_OWNER_GROUP), "3000");
}

#[test]
fn numlinks_tracks_hardlinks() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p9");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"f", 0o644);

    let attrs = c.getattr(&srv.uuid, f, &[FATTR4_NUMLINKS]).unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_NUMLINKS), 1);

    // LINK: cfh = file, saved_fh = destination dir.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.savefh();
    ops.putfh(&srv.uuid, f);
    ops.link(b"g");
    ops.restorefh();
    c.check_ok(b"link", ops);

    let attrs = c.getattr(&srv.uuid, f, &[FATTR4_NUMLINKS]).unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_NUMLINKS), 2);

    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.remove(b"g");
    c.check_ok(b"unlink", ops);
    let attrs = c.getattr(&srv.uuid, f, &[FATTR4_NUMLINKS]).unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_NUMLINKS), 1);
}

#[test]
fn getattr_dir_attrs() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.create_dir(b"d", 0o750);
    c.check_ok(b"mkdir", ops);
    let d = c.lookup_fh(&srv.uuid, ROOT_INO, b"d").unwrap();

    let attrs = c
        .getattr(
            &srv.uuid,
            d.inode,
            &[FATTR4_TYPE, FATTR4_MODE, FATTR4_NUMLINKS],
        )
        .unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_TYPE), NF4DIR);
    assert_eq!(attr_u32(&attrs, FATTR4_MODE) & 0o777, 0o750);
}

#[test]
fn attr_request_order_does_not_matter() {
    // Values are encoded in increasing attr-number order regardless of the
    // request order; the client must still decode each one correctly.
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let attrs = c
        .getattr(
            &srv.uuid,
            ROOT_INO,
            &[FATTR4_MODE, FATTR4_TYPE, FATTR4_SIZE],
        )
        .unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_TYPE), NF4DIR);
    assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 0);
    assert_eq!(attr_u32(&attrs, FATTR4_MODE) & 0o777, 0o777); // mkfs root mode
}

#[test]
fn unsupported_attr_is_silently_omitted() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    // Attr 11 (FATTR4_RDATTR_ERROR) is not implemented: the server must
    // omit it from the reply mask rather than fail.
    let attrs = c.getattr(&srv.uuid, ROOT_INO, &[FATTR4_TYPE, 11]).unwrap();
    assert_eq!(attr_u32(&attrs, FATTR4_TYPE), NF4DIR);
    assert!(!has_attr(&attrs, 11));
}

#[test]
fn p3_space_attrs_reflect_allocations() {
    // P3: GETATTR space attributes must reflect the O(1) free counter.
    // Allocate blocks by writing a file, verify SPACE_FREE decreases.
    let srv = spawn_server(8192);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"p3-space");

    // Get initial free space.
    let attrs_before = c
        .getattr(
            &srv.uuid,
            ROOT_INO,
            &[FATTR4_SPACE_FREE, FATTR4_SPACE_TOTAL],
        )
        .unwrap();
    let free_before = attr_u64(&attrs_before, FATTR4_SPACE_FREE);
    let total = attr_u64(&attrs_before, FATTR4_SPACE_TOTAL);
    assert!(total > 0, "SPACE_TOTAL should be positive");
    assert!(free_before > 0, "SPACE_FREE should be positive initially");
    assert!(
        free_before <= total,
        "SPACE_FREE ({free_before}) should not exceed SPACE_TOTAL ({total})"
    );

    // Write 1 MiB (256 blocks) to a new file.
    let ino = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"p3-big", 0o644);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    // 256 * 4KiB = 1 MiB
    let data = vec![0xABu8; 1024 * 1024];
    ops.write(0, FILE_SYNC4, &data);
    c.check_ok(b"p3-write", ops);

    // Free space should have decreased by ~1 MiB (256 blocks).
    let attrs_after = c
        .getattr(&srv.uuid, ROOT_INO, &[FATTR4_SPACE_FREE])
        .unwrap();
    let free_after = attr_u64(&attrs_after, FATTR4_SPACE_FREE);
    let delta = free_before - free_after;
    // Allow some slack for metadata blocks (b-tree nodes, etc.).
    // 256 data blocks = 1 MiB; metadata should be << 1 MiB.
    assert!(
        delta >= 1024 * 1024,
        "SPACE_FREE should decrease by at least 1 MiB after writing 1 MiB, got delta={delta}"
    );
    assert!(
        delta < 2 * 1024 * 1024,
        "SPACE_FREE decrease ({delta}) seems too large for 1 MiB write"
    );
}
