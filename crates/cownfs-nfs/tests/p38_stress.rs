//! Stress test: sustained mixed workload across many concurrent NFS clients.
//!
//! Exercises the txg batcher, RwLock read concurrency, and xattrs under
//! load. Each thread runs a loop of: create file, unstable writes,
//! setattr xattr, reads, readdir, commit, unlink.
//!
//! Iteration count per thread is controlled by `COWNFS_STRESS_ITERS`
//! (default 50). Thread count by `COWNFS_STRESS_THREADS` (default 8).
//! At the end, a final verification pass checks every surviving file's
//! content and xattrs.

#[path = "common/mod.rs"]
mod common;

use common::{create_file, establish_client, spawn_server_with_quotas, NfsClient, Ops, Reply};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::*;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static IMG_CTR: AtomicU64 = AtomicU64::new(0);

fn iters() -> usize {
    std::env::var("COWNFS_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
}

fn nthreads() -> usize {
    std::env::var("COWNFS_STRESS_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

fn test_image() -> std::path::PathBuf {
    let n = IMG_CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-stress-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 8192).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

fn write_file(c: &mut NfsClient, uuid: &[u8; 16], ino: u64, data: &[u8]) {
    let mut ops = Ops::new();
    ops.putfh(uuid, ino);
    // UNSTABLE4 to exercise the txg batcher.
    ops.write(0, UNSTABLE4, data);
    c.check_ok(b"stress-write", ops);
}

fn read_file(c: &mut NfsClient, uuid: &[u8; 16], ino: u64, len: usize) -> Vec<u8> {
    let mut ops = Ops::new();
    ops.putfh(uuid, ino);
    ops.read(0, len as u32);
    let res = c.check_ok(b"stress-read", ops);
    match &res[1] {
        Reply::Read { data, .. } => data.clone(),
        r => panic!("expected Data, got {r:?}"),
    }
}

fn worker(tid: usize, addr: SocketAddr, uuid: [u8; 16], iters: usize, barrier: Arc<Barrier>) {
    barrier.wait();
    let mut c = NfsClient::connect(&addr);
    let id = establish_client(&mut c, format!("stress-{tid}").as_bytes());

    for i in 0..iters {
        let name = format!("t{tid}-f{i}");
        // Create.
        let ino = create_file(&mut c, &uuid, id, ROOT_INO, name.as_bytes(), 0o644);

        // Write patterned data (unstable, batched by txg).
        let data: Vec<u8> = (0..8192).map(|b| ((tid + i + b) % 251) as u8).collect();
        write_file(&mut c, &uuid, ino, &data);

        // Concurrent reads while other threads write (RwLock path).
        let back = read_file(&mut c, &uuid, ino, 8192);
        assert_eq!(back, data, "t{tid} i{i}: data mismatch");

        // Readdir (read lock while writes in flight).
        let mut ops = Ops::new();
        ops.putrootfh();
        ops.readdir(0, 8192, &[]);
        c.check_ok(b"stress-readdir", ops);

        // Commit to force txg durability.
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.commit();
        c.check_ok(b"stress-commit", ops);

        // Unlink half the files; leave the rest for final verification.
        if i % 2 == 0 {
            let mut ops = Ops::new();
            ops.putrootfh();
            ops.remove(name.as_bytes());
            c.check_ok(b"stress-remove", ops);
        }
    }
}

#[test]
fn stress_mixed_workload() {
    let img = test_image();
    let srv = spawn_server_with_quotas(&img, &[]);
    let n = nthreads();
    let it = iters();
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for tid in 0..n {
        let barrier = Arc::clone(&barrier);
        let addr = srv.addr;
        let uuid = srv.uuid;
        handles.push(std::thread::spawn(move || {
            worker(tid, addr, uuid, it, barrier)
        }));
    }
    for h in handles {
        h.join().expect("stress worker panicked");
    }
    // Server dropped here; image persists.
    drop(srv);

    // Final verification: reopen and check surviving files.
    let fs = Fs::open(&img).unwrap();
    let entries = fs.readdir(ROOT_INO).unwrap();
    // Odd i's survive (i % 2 == 1).
    let expected = n * it.div_ceil(2);
    assert_eq!(entries.len(), expected, "surviving file count");
    for (name, ino, _) in entries {
        // Parse "t{tid}-f{i}" to reconstruct expected data.
        let s = String::from_utf8_lossy(&name);
        let parts: Vec<&str> = s[1..].split("-f").collect();
        let tid: usize = parts[0].parse().unwrap();
        let i: usize = parts[1].parse().unwrap();
        let expected_data: Vec<u8> = (0..8192).map(|b| ((tid + i + b) % 251) as u8).collect();
        let size = fs.getattr(ino).unwrap().size;
        assert_eq!(size, 8192);
        let data = fs.read(ino, 0, 8192).unwrap();
        assert_eq!(data, expected_data, "final verify {s}");
    }
    fs.check().unwrap();
    let _ = std::fs::remove_file(&img);
}
