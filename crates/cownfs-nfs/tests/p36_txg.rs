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

#[test]
fn p2_group_commit_coalesces() {
    // P2 exit criteria: 64 concurrent FILE_SYNC writes → ≤ 2 physical commits.
    // This test FAILS before the P2 fix (each FILE_SYNC does a full
    // commit_async flush, so 64 writes = 64 physical commits).
    let img = test_image();
    let srv = spawn_server_on(&img);
    // Use a short background txg interval so the FILE_SYNC wait() has a
    // syncer to wake it. The 64 concurrent writes should coalesce into
    // very few physical syncs.
    srv.shared.set_txg_interval_ms(200);

    let uuid = srv.uuid;
    let baseline = srv.shared.fs.read().unwrap().sync_count();

    // 64 threads, each doing a FILE_SYNC write to its own file.
    // Use a barrier so all threads hit commit_async simultaneously,
    // maximizing coalescing into a single txg.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(64));
    let handles: Vec<_> = (0..64)
        .map(|i| {
            let addr = srv.addr;
            let uuid = uuid;
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut c = NfsClient::connect(&addr);
                let id = establish_client(&mut c, format!("p2-{i}").as_bytes());
                let ino = common::create_file(
                    &mut c,
                    &uuid,
                    id,
                    ROOT_INO,
                    format!("p2-f{i}").as_bytes(),
                    0o644,
                );
                // Barrier: all threads hit the FILE_SYNC write simultaneously.
                barrier.wait();
                let mut ops = Ops::new();
                ops.putfh(&uuid, ino);
                ops.write(0, FILE_SYNC4, b"data");
                c.check_ok(b"filesync", ops);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let final_count = srv.shared.fs.read().unwrap().sync_count();
    let physical_commits = final_count - baseline;

    drop(srv);
    let _ = std::fs::remove_file(&img);

    assert!(
        physical_commits <= 2,
        "P2: 64 concurrent FILE_SYNC writes should coalesce to ≤ 2 physical commits, got {physical_commits}"
    );
}

#[test]
fn p2_commit_coalesces() {
    // P2 exit criteria: 64 concurrent COMMITs → ≤ 2 physical commits.
    // This test FAILS before the P2 fix (op_commit called sync_txg directly,
    // bypassing group commit, so 64 COMMITs = 64 physical commits).
    let img = test_image();
    let srv = spawn_server_on(&img);
    // Short background interval for reasonable COMMIT latency.
    srv.shared.set_txg_interval_ms(200);

    let uuid = srv.uuid;
    let baseline = srv.shared.fs.read().unwrap().sync_count();

    // 64 threads, each doing an UNSTABLE write followed by COMMIT.
    // The UNSTABLE writes dirty the trees; the COMMITs should coalesce.
    // Use a barrier for maximum coalescing.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(64));
    let handles: Vec<_> = (0..64)
        .map(|i| {
            let addr = srv.addr;
            let uuid = uuid;
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut c = NfsClient::connect(&addr);
                let id = establish_client(&mut c, format!("p2c-{i}").as_bytes());
                let ino = common::create_file(
                    &mut c,
                    &uuid,
                    id,
                    ROOT_INO,
                    format!("p2c-f{i}").as_bytes(),
                    0o644,
                );
                // UNSTABLE write (does not sync).
                let mut ops = Ops::new();
                ops.putfh(&uuid, ino);
                ops.write(0, UNSTABLE4, b"data");
                c.check_ok(b"unstable", ops);
                // Barrier: all threads hit COMMIT simultaneously.
                barrier.wait();
                // COMMIT (should coalesce via txg).
                let mut ops = Ops::new();
                ops.putfh(&uuid, ino);
                ops.commit();
                c.check_ok(b"commit", ops);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let final_count = srv.shared.fs.read().unwrap().sync_count();
    let physical_commits = final_count - baseline;

    drop(srv);
    let _ = std::fs::remove_file(&img);

    assert!(
        physical_commits <= 2,
        "P2: 64 concurrent COMMITs should coalesce to ≤ 2 physical commits, got {physical_commits}"
    );
}
