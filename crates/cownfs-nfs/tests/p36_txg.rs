//! Transaction group (txg) wire tests.
//!
//! FILE_SYNC4 writes stage into the open txg and block until the sync
//! thread makes it durable (coalescing concurrent sync writes onto one
//! fsync). UNSTABLE writes only stage; NFS COMMIT forces durability.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server, spawn_server_on, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-txg-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 1024).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn file_sync4_write_is_durable() {
    let img = test_image();
    let srv = spawn_server_on(&img);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"txg");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"sync", 0o644);

    // FILE_SYNC4 write must report FILE_SYNC4 (stable).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, FILE_SYNC4, b"stable-data");
    let res = c.check_ok(b"sync-write", ops);
    match &res[1] {
        Reply::Written { count, committed } => {
            assert_eq!(*count, 11);
            assert_eq!(*committed, FILE_SYNC4);
        }
        r => panic!("{r:?}"),
    }
    drop(c);
    drop(srv);

    // Data survives: reopen the image directly with the engine.
    let fs = Fs::open(&img).unwrap();
    assert_eq!(fs.read(f, 0, 20).unwrap(), b"stable-data");
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

#[test]
fn unstable_write_plus_commit_is_durable() {
    let srv = spawn_server(4096);
    let mut c = NfsClient::connect(&srv.addr);
    let id = establish_client(&mut c, b"txg2");
    let f = common::create_file(&mut c, &srv.uuid, id, ROOT_INO, b"uns", 0o644);

    // UNSTABLE write reports UNSTABLE.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.write(0, UNSTABLE4, b"unstable-data");
    let res = c.check_ok(b"unstable-write", ops);
    match &res[1] {
        Reply::Written { committed, .. } => assert_eq!(*committed, UNSTABLE4),
        r => panic!("{r:?}"),
    }

    // NFS COMMIT makes it stable.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, f);
    ops.commit();
    c.check_ok(b"commit", ops);

    // Read back.
    assert_eq!(c.read_all(&srv.uuid, f, 13), b"unstable-data");
}

#[test]
fn concurrent_sync_writes_all_succeed() {
    // 16 threads doing FILE_SYNC4 writes concurrently: all must succeed
    // (each waits on the txg condvar; the sync thread coalesces).
    let srv = spawn_server(4096);
    let addr = srv.addr;
    let uuid = srv.uuid;

    // Create the files first (single client).
    let mut c = NfsClient::connect(&addr);
    let id = establish_client(&mut c, b"txg3");
    let mut inos = Vec::new();
    for i in 0..16 {
        let name = format!("cw{i}");
        inos.push(common::create_file(
            &mut c,
            &uuid,
            id,
            ROOT_INO,
            name.as_bytes(),
            0o644,
        ));
    }
    drop(c);

    let handles: Vec<_> = inos
        .into_iter()
        .enumerate()
        .map(|(i, ino)| {
            let uuid = uuid;
            std::thread::spawn(move || {
                let mut c = NfsClient::connect(&addr);
                let data = vec![i as u8; 100];
                let mut ops = Ops::new();
                ops.putfh(&uuid, ino);
                ops.write(0, FILE_SYNC4, &data);
                let res = c.check_ok(b"cw", ops);
                match &res[1] {
                    Reply::Written { committed, .. } => {
                        assert_eq!(*committed, FILE_SYNC4)
                    }
                    r => panic!("{r:?}"),
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}
