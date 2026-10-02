//! pNFS file layouts (single data server): LAYOUTGET -> direct DS write
//! -> LAYOUTCOMMIT -> LAYOUTRETURN.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_layout_server, NfsClient, Ops, Reply};
use cownfs_nfs::nfs4::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Write one block to the DS, returning the checksum.
fn ds_write(addr: &str, block_id: u64, data: &[u8; 4096]) -> u64 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(&0x4453_3031u32.to_be_bytes()).unwrap();
    s.write_all(&1u32.to_be_bytes()).unwrap();
    s.write_all(&[2u8]).unwrap(); // WRITE
    s.write_all(&block_id.to_be_bytes()).unwrap();
    s.write_all(data).unwrap();
    let mut st = [0u8; 4];
    s.read_exact(&mut st).unwrap();
    assert_eq!(u32::from_be_bytes(st), 0);
    let mut sum = [0u8; 8];
    s.read_exact(&mut sum).unwrap();
    u64::from_be_bytes(sum)
}

fn establish_session(c: &mut NfsClient) -> [u8; 16] {
    let mut ops = Ops::new();
    ops.exchange_id(&[0x42; 8], b"layout-client");
    let (_, res) = c.call(b"exch", ops);
    let clientid = match res[0] {
        Reply::ClientId(id) => id,
        ref r => panic!("{r:?}"),
    };
    let mut ops = Ops::new();
    ops.create_session(clientid, 1, 8);
    let (_, res) = c.call(b"cs", ops);
    match res[0] {
        Reply::Session(sid) => sid,
        ref r => panic!("{r:?}"),
    }
}

#[test]
fn layout_write_commit_read() {
    // Start the data server.
    let dir = std::env::temp_dir().join(format!("cownfs-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ds_port = free_port();
    let ds_addr = format!("127.0.0.1:{ds_port}");
    let mut ds = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_cownfs-ds")))
        .args([dir.join("ds.bin").to_str().unwrap(), &ds_addr])
        .spawn()
        .expect("spawn ds");
    std::thread::sleep(Duration::from_millis(300));

    // MDS with layouts enabled.
    let srv = spawn_layout_server(4096, &ds_addr);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"layout-test");

    // Create a file (4.0 mode, before the session).
    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open(id, b"o", 3, OPEN4_CREATE, UNCHECKED4, &[], 0, b"pfile");
    let (st, _) = c.call(b"create", ops);
    assert_eq!(st, NFS4_OK);

    let sid = establish_session(&mut c);

    // Look it up to get the filehandle.
    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    ops.lookup(b"pfile");
    ops.getfh();
    let (st, res) = c.call(b"lookup", ops);
    assert_eq!(st, NFS4_OK);
    let fh = match &res[3] {
        Reply::Fh(fh) => fh.clone(),
        r => panic!("expected Fh, got {r:?}"),
    };

    // LAYOUTGET for 8 KiB.
    let mut ops = Ops::new();
    ops.sequence(&sid, 2, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.layoutget(0, 8192);
    let (st, res) = c.call(b"layoutget", ops);
    assert_eq!(st, NFS4_OK);
    let (first_bid, nblocks, ds_addr_ret) = match &res[2] {
        Reply::Layout {
            first_block_id,
            nblocks,
            ds_addr,
        } => (*first_block_id, *nblocks, ds_addr.clone()),
        r => panic!("expected Layout, got {r:?}"),
    };
    assert_eq!(nblocks, 2);
    assert_eq!(ds_addr_ret, ds_addr);

    // Write both blocks directly to the DS.
    let mut b0 = [0u8; 4096];
    b0[..12].copy_from_slice(b"layout-data-");
    let mut b1 = [0u8; 4096];
    b1[..12].copy_from_slice(b"block-two!!!");
    let s0 = ds_write(&ds_addr, first_bid, &b0);
    let s1 = ds_write(&ds_addr, first_bid + 1, &b1);

    // LAYOUTCOMMIT: MDS verifies checksums and swings the B-tree.
    let mut ops = Ops::new();
    ops.sequence(&sid, 3, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.layoutcommit(0, 8192, &[(first_bid, s0), (first_bid + 1, s1)], Some(8192));
    let (st, _) = c.call(b"commit", ops);
    assert_eq!(st, NFS4_OK, "layout commit should succeed");

    // Read back via normal NFS READ: the committed data must be there.
    let data = c
        .getattr(&srv.uuid, fh.inode, &[FATTR4_SIZE])
        .expect("getattr");
    drop(data);
    let mut ops = Ops::new();
    ops.sequence(&sid, 4, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.read(0, 8192);
    let (st, res) = c.call(b"read", ops);
    assert_eq!(st, NFS4_OK);
    match &res[2] {
        Reply::Read { data, .. } => {
            assert_eq!(&data[..12], b"layout-data-");
            assert_eq!(&data[4096..4108], b"block-two!!!");
        }
        r => panic!("expected Read, got {r:?}"),
    }

    // LAYOUTRETURN releases the layout.
    let mut ops = Ops::new();
    ops.sequence(&sid, 5, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.layoutreturn(0, 8192);
    let (st, _) = c.call(b"return", ops);
    assert_eq!(st, NFS4_OK);

    ds.kill().ok();
    ds.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn layoutcommit_bad_checksum_rejected() {
    let dir = std::env::temp_dir().join(format!("cownfs-layout2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ds_port = free_port();
    let ds_addr = format!("127.0.0.1:{ds_port}");
    let mut ds = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_cownfs-ds")))
        .args([dir.join("ds.bin").to_str().unwrap(), &ds_addr])
        .spawn()
        .expect("spawn ds");
    std::thread::sleep(Duration::from_millis(300));

    let srv = spawn_layout_server(4096, &ds_addr);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"layout-test");

    let mut ops = Ops::new();
    ops.putrootfh();
    ops.open(id, b"o", 3, OPEN4_CREATE, UNCHECKED4, &[], 0, b"pfile2");
    assert_eq!(c.call(b"create", ops).0, NFS4_OK);

    let sid = establish_session(&mut c);

    let mut ops = Ops::new();
    ops.sequence(&sid, 1, 0, true);
    ops.putrootfh();
    ops.lookup(b"pfile2");
    ops.getfh();
    let (st, res) = c.call(b"lookup", ops);
    assert_eq!(st, NFS4_OK);
    let fh = match &res[3] {
        Reply::Fh(fh) => fh.clone(),
        r => panic!("{r:?}"),
    };

    let mut ops = Ops::new();
    ops.sequence(&sid, 2, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.layoutget(0, 4096);
    let (st, res) = c.call(b"layoutget", ops);
    assert_eq!(st, NFS4_OK);
    let first_bid = match &res[2] {
        Reply::Layout { first_block_id, .. } => *first_block_id,
        r => panic!("{r:?}"),
    };

    let b0 = [0xCCu8; 4096];
    let real_sum = ds_write(&ds_addr, first_bid, &b0);

    // Commit with a WRONG checksum: must be rejected, file untouched.
    let mut ops = Ops::new();
    ops.sequence(&sid, 3, 0, true);
    ops.putfh(&srv.uuid, fh.inode);
    ops.layoutcommit(0, 4096, &[(first_bid, real_sum ^ 1)], Some(4096));
    let (st, _) = c.call(b"commit-bad", ops);
    assert_ne!(st, NFS4_OK, "bad checksum must be rejected");

    ds.kill().ok();
    ds.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
}
