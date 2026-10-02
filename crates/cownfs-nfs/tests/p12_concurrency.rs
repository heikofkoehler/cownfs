//! P12 gate: concurrency with multiple parallel clients.
//!
//! The server now serves each TCP connection on its own thread with the
//! filesystem and NFSv4 state shared across connections. These tests hammer
//! it from N OS threads, each with its own TCP connection and clientid:
//! independent files, disjoint writes to one file, a guarded-create race,
//! lock and share-deny conflicts across connections, clientid
//! establishment, and a mixed workload with mutation under readdir.

#[path = "common/mod.rs"]
mod common;

use common::{
    attr_u64, av_u32, create_file, establish_client, spawn_concurrent_server, NfsClient, Ops, Reply,
};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;
use std::net::SocketAddr;
use std::sync::{Arc, Barrier, Mutex};

/// Run `n` threads, each with its own TCP connection, released at once via
/// a barrier so the load actually overlaps. Panics propagate via join.
fn run_parallel<F>(n: usize, addr: SocketAddr, uuid: [u8; 16], f: F)
where
    F: Fn(usize, SocketAddr, [u8; 16], &mut NfsClient) + Send + Sync + 'static,
{
    let f = Arc::new(f);
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let f = Arc::clone(&f);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = NfsClient::connect(&addr);
            f(i, addr, uuid, &mut c);
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

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

/// PUTFH + WRITE (FILE_SYNC4). The server writes against the cfh.
fn write_at(c: &mut NfsClient, uuid: &[u8; 16], ino: u64, offset: u64, data: &[u8]) {
    let mut ops = Ops::new();
    ops.putfh(uuid, ino);
    ops.write(offset, FILE_SYNC4, data);
    let res = c.check_ok(b"write", ops);
    match &res[1] {
        Reply::Written { count, .. } => assert_eq!(*count as usize, data.len()),
        r => panic!("expected Written, got {r:?}"),
    }
}

#[test]
fn parallel_independent_files() {
    let srv = spawn_concurrent_server(8192);
    const N: usize = 8;

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        let name = format!("t{i}.dat");
        let id = establish_client(c, format!("pi-{i}").as_bytes());
        let ino = create_file(c, &uuid, id, ROOT_INO, name.as_bytes(), 0o644);
        let data = vec![i as u8; 4096];
        write_at(c, &uuid, ino, 0, &data);
        let back = c.read_all(&uuid, ino, 4096);
        assert_eq!(back, data, "thread {i}: data mismatch");
    });

    // All files visible from a fresh connection.
    let mut c = NfsClient::connect(&srv.addr);
    let entries = c.readdir_all(&srv.uuid, ROOT_INO, 8192, &[FATTR4_FILEID]);
    assert_eq!(entries.len(), N, "readdir count after parallel creates");
    for i in 0..N {
        assert!(
            entries
                .iter()
                .any(|e| e.name == format!("t{i}.dat").as_bytes()),
            "t{i}.dat missing after parallel creates"
        );
    }
}

#[test]
fn parallel_disjoint_writes_one_file() {
    let srv = spawn_concurrent_server(8192);
    const N: usize = 8;

    let mut c0 = NfsClient::connect(&srv.addr);
    let id0 = establish_client(&mut c0, b"shared");
    let ino = create_file(&mut c0, &srv.uuid, id0, ROOT_INO, b"shared.dat", 0o644);

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        establish_client(c, format!("pw-{i}").as_bytes());
        let data = vec![(i as u8).wrapping_mul(37).wrapping_add(11); 4096];
        write_at(c, &uuid, ino, (i as u64) * 4096, &data);
    });

    let back = c0.read_all(&srv.uuid, ino, (N as u64) * 4096);
    assert_eq!(back.len(), N * 4096);
    for i in 0..N {
        let expect = vec![(i as u8).wrapping_mul(37).wrapping_add(11); 4096];
        assert_eq!(
            &back[i * 4096..(i + 1) * 4096],
            &expect[..],
            "region {i} corrupt"
        );
    }
}

#[test]
fn parallel_guarded_create_race_one_winner() {
    let srv = spawn_concurrent_server(4096);
    const N: usize = 8;

    let results = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let results = Arc::clone(&results);
        let barrier = Arc::clone(&barrier);
        let addr = srv.addr;
        let uuid = srv.uuid;
        handles.push(std::thread::spawn(move || {
            let mut c = NfsClient::connect(&addr);
            let id = establish_client(&mut c, format!("pr-{i}").as_bytes());
            barrier.wait();
            // GUARDED4 create: exactly one must win, the rest get EXIST.
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.open(
                id,
                format!("o{i}").as_bytes(),
                3,
                OPEN4_CREATE,
                GUARDED4,
                &[(FATTR4_MODE, av_u32(0o644))],
                0,
                b"race.dat",
            );
            let (overall, _) = c.call(b"race", ops);
            results.lock().unwrap().push(overall);
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
    let results = results.lock().unwrap();
    assert_eq!(results.len(), N);
    assert_eq!(
        results.iter().filter(|&&s| s == NFS4_OK).count(),
        1,
        "expected exactly one winner, got {results:?}"
    );
    assert!(
        results.iter().all(|&s| s == NFS4_OK || s == NFS4ERR_EXIST),
        "unexpected statuses: {results:?}"
    );
}

#[test]
fn cross_connection_lock_conflict() {
    let srv = spawn_concurrent_server(4096);

    let mut ca = NfsClient::connect(&srv.addr);
    let ida = establish_client(&mut ca, b"lock-a");
    create_file(&mut ca, &srv.uuid, ida, ROOT_INO, b"lock.dat", 0o644);

    // A (connection 1) takes a write lock on 0..100.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(ida, b"oa", 3, b"lock.dat");
    let res = ca.check_ok(b"open-a", ops);
    let sa = open_stateid(&res[1]);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"lock.dat");
    ops.lock_new(ida, WRITE_LT, 0, 100, &sa, b"la");
    let res = ca.check_ok(b"lock-a", ops);
    let sla = lock_stateid(&res[2]);

    // Four other connections try an overlapping lock: all must be LOCKED.
    // A disjoint range on each must succeed.
    const N: usize = 4;
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let barrier = Arc::clone(&barrier);
        let addr = srv.addr;
        let uuid = srv.uuid;
        handles.push(std::thread::spawn(move || {
            let mut c = NfsClient::connect(&addr);
            let id = establish_client(&mut c, format!("lock-b{i}").as_bytes());
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.open_nocreate(id, b"ob", 3, b"lock.dat");
            let res = c.check_ok(b"open-b", ops);
            let sb = open_stateid(&res[1]);
            barrier.wait();
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.lookup(b"lock.dat");
            ops.lock_new(id, WRITE_LT, 50, 100, &sb, b"lb");
            let (overall, res) = c.call(b"lock-overlap", ops);
            assert_eq!(overall, NFS4ERR_LOCKED, "thread {i}: expected LOCKED");
            assert!(matches!(res[2], Reply::Err(NFS4ERR_LOCKED)));
            // Disjoint range is fine even while A holds 0..100.
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.lookup(b"lock.dat");
            ops.lock_new(id, WRITE_LT, 1000 + (i as u64) * 100, 100, &sb, b"lb2");
            c.check_ok(b"lock-disjoint", ops);
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    // A unlocks; a fresh contender can now take the range.
    let mut ops = Ops::new();
    ops.locku(WRITE_LT, 1, &sla, 0, 100);
    ca.check_ok(b"locku-a", ops);

    let mut cb = NfsClient::connect(&srv.addr);
    let idb = establish_client(&mut cb, b"lock-c");
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(idb, b"oc", 3, b"lock.dat");
    let res = cb.check_ok(b"open-c", ops);
    let sc = open_stateid(&res[1]);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.lookup(b"lock.dat");
    ops.lock_new(idb, WRITE_LT, 0, 100, &sc, b"lc");
    cb.check_ok(b"relock", ops);
}

#[test]
fn cross_connection_share_deny() {
    let srv = spawn_concurrent_server(4096);

    let mut ca = NfsClient::connect(&srv.addr);
    let ida = establish_client(&mut ca, b"deny-a");
    create_file(&mut ca, &srv.uuid, ida, ROOT_INO, b"deny.dat", 0o644);

    // A opens read/write and denies writers.
    let deny_write = 3 | (2 << 4);
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(ida, b"oa", deny_write, b"deny.dat");
    let res = ca.check_ok(b"open-deny", ops);
    let _sa = open_stateid(&res[1]);

    // B on a separate connection wants write access -> DENIED.
    let mut cb = NfsClient::connect(&srv.addr);
    let idb = establish_client(&mut cb, b"deny-b");
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(idb, b"ob", 2, b"deny.dat");
    let (overall, res) = cb.call(b"open-denied", ops);
    assert_eq!(overall, NFS4ERR_DENIED);
    assert!(matches!(res[1], Reply::Err(NFS4ERR_DENIED)));

    // B read-only -> allowed.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ROOT_INO);
    ops.open_nocreate(idb, b"ob", 1, b"deny.dat");
    cb.check_ok(b"open-read", ops);
}

#[test]
fn parallel_clientid_establish() {
    let srv = spawn_concurrent_server(4096);
    const N: usize = 16;

    let ids = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let ids = Arc::clone(&ids);
        let barrier = Arc::clone(&barrier);
        let addr = srv.addr;
        handles.push(std::thread::spawn(move || {
            let mut c = NfsClient::connect(&addr);
            barrier.wait();
            let id = establish_client(&mut c, format!("est-{i:02}").as_bytes());
            ids.lock().unwrap().push(id);
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
    let mut ids = ids.lock().unwrap();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), N, "clientids not unique: {ids:?}");
}

#[test]
fn parallel_mixed_workload_with_readdir() {
    let srv = spawn_concurrent_server(16384);
    const N: usize = 8;
    const ITERS: usize = 25;

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        let id = establish_client(c, format!("mw-{i}").as_bytes());
        for j in 0..ITERS {
            let name = format!("w{i}-{j}.dat");
            let ino = create_file(c, &uuid, id, ROOT_INO, name.as_bytes(), 0o644);
            let data = vec![(i as u8) ^ (j as u8); 512];
            write_at(c, &uuid, ino, 0, &data);
            // getattr size must match what we wrote.
            let attrs = c.getattr(&uuid, ino, &[FATTR4_SIZE]).expect("getattr");
            assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 512, "t{i} iter {j}: size");
            let back = c.read_all(&uuid, ino, 512);
            assert_eq!(back, data, "t{i} iter {j}: data");
            // Readdir while others mutate: must not error.
            let _ = c.readdir_all(&uuid, ROOT_INO, 8192, &[FATTR4_FILEID]);
        }
    });

    // Final consistency: all N*ITERS files present with exact contents.
    let mut c = NfsClient::connect(&srv.addr);
    let entries = c.readdir_all(&srv.uuid, ROOT_INO, 8192, &[FATTR4_FILEID]);
    assert_eq!(
        entries.len(),
        N * ITERS,
        "readdir count mismatch: {}",
        entries.len()
    );
    for i in 0..N {
        for j in 0..ITERS {
            let name = format!("w{i}-{j}.dat");
            assert!(
                entries.iter().any(|e| e.name == name.as_bytes()),
                "{name} missing"
            );
            let fh = c
                .lookup_fh(&srv.uuid, ROOT_INO, name.as_bytes())
                .expect("lookup");
            let back = c.read_all(&srv.uuid, fh.inode, 512);
            assert_eq!(back, vec![(i as u8) ^ (j as u8); 512], "{name} corrupt");
        }
    }
}
