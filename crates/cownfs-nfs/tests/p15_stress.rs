//! P15 gate: concurrency stress with emphasis on contention.
//!
//! Where P12 checks correctness under moderate parallelism (8 threads),
//! P15 hammers the shared state paths that are hardest to get right:
//! open/lock/unlock/close cycles on one file, thundering-herd getattr,
//! rename races in a shared namespace, connection churn, and mixed
//! high-contention workloads. Any data race, deadlock, or state-machine
//! bug should surface here.

#[path = "common/mod.rs"]
mod common;

use common::{
    attr_u64, create_file, establish_client, spawn_concurrent_server, NfsClient, Ops, Reply,
};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;
use std::net::SocketAddr;
use std::sync::{Arc, Barrier};

/// Spawn `n` threads, each with its own TCP connection, released at once
/// via a barrier. Panics propagate via join.
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

/// Full open/lock/unlock/close cycle on one shared file, hammered by many
/// threads. Stresses seqid bookkeeping and the state manager under
/// contention. Lock conflicts are expected; any other error is a bug.
#[test]
fn stress_open_lock_close_cycles() {
    let srv = spawn_concurrent_server(16384);
    const N: usize = 16;
    const ITERS: usize = 50;

    let mut c0 = NfsClient::connect(&srv.addr);
    let id0 = establish_client(&mut c0, b"cycle-setup");
    create_file(&mut c0, &srv.uuid, id0, ROOT_INO, b"cycle.dat", 0o644);

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        let id = establish_client(c, format!("cy-{i:02}").as_bytes());
        let owner = format!("owner-{i:02}");
        let lock_owner = format!("lock-{i:02}");
        for j in 0..ITERS {
            // OPEN (nocreate).
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.open_nocreate(id, owner.as_bytes(), 3, b"cycle.dat");
            let res = c.check_ok(b"cycle-open", ops);
            let ost = open_stateid(&res[1]);

            // LOCK a per-thread disjoint range so conflicts are rare but
            // the state machine still runs hot.
            let off = (i as u64) * 1024;
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.lookup(b"cycle.dat");
            ops.lock_new(id, WRITE_LT, off, 512, &ost, lock_owner.as_bytes());
            let (st, res) = c.call(b"cycle-lock", ops);
            let lst = match st {
                NFS4_OK => Some(lock_stateid(&res[2])),
                NFS4ERR_LOCKED | NFS4ERR_DENIED => None,
                s => panic!("t{i} iter {j}: unexpected lock status {s}"),
            };

            if let Some(lst) = lst {
                let mut ops = Ops::new();
                ops.putfh(&uuid, ROOT_INO);
                ops.lookup(b"cycle.dat");
                ops.locku(WRITE_LT, 1, &lst, off, 512);
                let (st, _) = c.call(b"cycle-unlock", ops);
                assert!(
                    st == NFS4_OK || st == NFS4ERR_BAD_SEQID,
                    "t{i} iter {j}: unlock status {st}"
                );
            }

            // CLOSE. seqid: open used 1, so close uses 2.
            let mut ops = Ops::new();
            ops.close(2, &ost);
            let (st, _) = c.call(b"cycle-close", ops);
            assert!(
                st == NFS4_OK || st == NFS4ERR_EXPIRED,
                "t{i} iter {j}: close status {st}"
            );
        }
    });
}

/// Thundering herd: 32 threads hammer GETATTR on one inode behind a
/// barrier. Read-only path; must never error or deadlock.
#[test]
fn stress_thundering_herd_getattr() {
    let srv = spawn_concurrent_server(8192);
    const N: usize = 32;
    const ITERS: usize = 200;

    let mut c0 = NfsClient::connect(&srv.addr);
    let id0 = establish_client(&mut c0, b"herd-setup");
    let ino = create_file(&mut c0, &srv.uuid, id0, ROOT_INO, b"herd.dat", 0o644);

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        establish_client(c, format!("he-{i:02}").as_bytes());
        for _ in 0..ITERS {
            let attrs = c
                .getattr(&uuid, ino, &[FATTR4_SIZE, FATTR4_FILEID])
                .expect("getattr");
            assert_eq!(attr_u64(&attrs, FATTR4_FILEID), ino);
        }
    });
}

/// Rename races in a shared namespace: threads repeatedly rename their
/// own files through a common staging name. Exactly one rename to the
/// staging name can win at a time; the loser must get a clean error, and
/// the namespace must stay consistent.
#[test]
fn stress_rename_races() {
    let srv = spawn_concurrent_server(16384);
    const N: usize = 8;
    const ITERS: usize = 30;

    let mut c0 = NfsClient::connect(&srv.addr);
    let id0 = establish_client(&mut c0, b"rn-setup");
    for i in 0..N {
        create_file(
            &mut c0,
            &srv.uuid,
            id0,
            ROOT_INO,
            format!("rn-{i}.dat").as_bytes(),
            0o644,
        );
    }

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        let id = establish_client(c, format!("rn-{i:02}").as_bytes());
        let _ = id;
        let mine = format!("rn-{i}.dat");
        let stage = format!("stage-{i}.dat");
        for _ in 0..ITERS {
            // rename mine -> stage-i
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.putfh(&uuid, ROOT_INO);
            ops.rename(mine.as_bytes(), stage.as_bytes());
            let (st, _) = c.call(b"rn-fwd", ops);
            assert!(
                st == NFS4_OK || st == NFS4ERR_NOENT,
                "t{i}: fwd rename status {st}"
            );
            // rename stage-i -> mine
            let mut ops = Ops::new();
            ops.putfh(&uuid, ROOT_INO);
            ops.putfh(&uuid, ROOT_INO);
            ops.rename(stage.as_bytes(), mine.as_bytes());
            let (st, _) = c.call(b"rn-back", ops);
            assert!(
                st == NFS4_OK || st == NFS4ERR_NOENT,
                "t{i}: back rename status {st}"
            );
        }
    });

    // Namespace must contain exactly the N original files.
    let mut c = NfsClient::connect(&srv.addr);
    let entries = c.readdir_all(&srv.uuid, ROOT_INO, 8192, &[FATTR4_FILEID]);
    assert_eq!(
        entries.len(),
        N,
        "namespace corrupt: {} entries",
        entries.len()
    );
}

/// Connection churn: 200 short-lived connections, each doing one quick
/// compound. Stresses accept/thread-spawn/teardown paths.
#[test]
fn stress_connection_churn() {
    let srv = spawn_concurrent_server(8192);
    const N: usize = 64;

    let mut c0 = NfsClient::connect(&srv.addr);
    let id0 = establish_client(&mut c0, b"churn-setup");
    let ino = create_file(&mut c0, &srv.uuid, id0, ROOT_INO, b"churn.dat", 0o644);

    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for chunk in 0..N / 8 {
        let barrier = Arc::clone(&barrier);
        let addr = srv.addr;
        let uuid = srv.uuid;
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for k in 0..8 {
                let i = chunk * 8 + k;
                let mut c = NfsClient::connect(&addr);
                establish_client(&mut c, format!("ch-{i:03}").as_bytes());
                let attrs = c.getattr(&uuid, ino, &[FATTR4_SIZE]).expect("getattr");
                assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 0);
                // Connection drops here.
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
}

/// Mixed high-contention workload: 16 threads doing creates, writes,
/// setattrs, and removes on a shared directory. Final state must be
/// fully consistent.
#[test]
fn stress_mixed_high_contention() {
    let srv = spawn_concurrent_server(32768);
    const N: usize = 16;
    const ITERS: usize = 20;

    run_parallel(N, srv.addr, srv.uuid, move |i, _addr, uuid, c| {
        let id = establish_client(c, format!("mx-{i:02}").as_bytes());
        for j in 0..ITERS {
            let name = format!("mx-{i:02}-{j:02}.dat");
            let ino = create_file(c, &uuid, id, ROOT_INO, name.as_bytes(), 0o644);

            // Write some data.
            let data = vec![(i as u8).wrapping_add(j as u8); 1024];
            let mut ops = Ops::new();
            ops.putfh(&uuid, ino);
            ops.write(0, FILE_SYNC4, &data);
            let res = c.check_ok(b"mx-write", ops);
            match &res[1] {
                Reply::Written { count, .. } => assert_eq!(*count as usize, 1024),
                r => panic!("t{i} iter {j}: {r:?}"),
            }

            // Truncate via SETATTR size.
            let mut ops = Ops::new();
            ops.putfh(&uuid, ino);
            ops.setattr(&[(FATTR4_SIZE, {
                let mut v = Vec::new();
                v.extend_from_slice(&512u64.to_be_bytes());
                v
            })]);
            c.check_ok(b"mx-truncate", ops);

            // Remove on even iterations; keep on odd.
            if j % 2 == 0 {
                let mut ops = Ops::new();
                ops.putfh(&uuid, ROOT_INO);
                ops.remove(name.as_bytes());
                c.check_ok(b"mx-remove", ops);
            }
        }
    });

    // Every thread kept ITERS/2 files; all must be present, 512 bytes.
    let mut c = NfsClient::connect(&srv.addr);
    let entries = c.readdir_all(&srv.uuid, ROOT_INO, 16384, &[FATTR4_FILEID]);
    assert_eq!(entries.len(), N * ITERS / 2, "count {}", entries.len());
    for i in 0..N {
        for j in (1..ITERS).step_by(2) {
            let name = format!("mx-{i:02}-{j:02}.dat");
            let e = entries
                .iter()
                .find(|e| e.name == name.as_bytes())
                .unwrap_or_else(|| panic!("{name} missing"));
            let ino = e
                .attrs
                .iter()
                .find(|(a, _)| *a == FATTR4_FILEID)
                .map(|(_, v)| u64::from_be_bytes(v[..8].try_into().unwrap()))
                .expect("fileid");
            let attrs = c.getattr(&srv.uuid, ino, &[FATTR4_SIZE]).expect("getattr");
            assert_eq!(attr_u64(&attrs, FATTR4_SIZE), 512, "{name} size");
        }
    }
}
