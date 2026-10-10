//! nfs-loadgen: fio-like sustained load generator over NFSv4.
//!
//! Drives the userspace NFSv4 server (spawned in-process) with sequential
//! and random read/write jobs and reports throughput/IOPS. This is the
//! "fio over NFS" context for P7: real NFS COMPOUND round trips over
//! loopback TCP, since this environment has no kernel NFS client.
//!
//! Usage: `nfs-loadgen` (all jobs, ~1 GiB file, 64 MiB image for metadata).

#[path = "../nfs_client.rs"]
mod nfs_client;

use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::{DATA_SYNC4, FILE_SYNC4, UNSTABLE4};
use nfs_client::{Client, Ops, R};
use std::time::Instant;

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn spawn_server(img: &std::path::Path, addr: &str) {
    let img2 = img.to_path_buf();
    let addr = addr.to_string();
    let addr2 = addr.clone();
    std::thread::spawn(move || {
        let fs = Fs::open(&img2).unwrap();
        let _ = cownfs_nfs::server::serve(&addr, cownfs_nfs::server::Shared::new(fs));
    });
    // Wait for the port.
    for _ in 0..100 {
        if std::net::TcpStream::connect(&addr2).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("server did not come up on {addr2}");
}

struct Job {
    name: &'static str,
    /// (is_write, offset) sequence.
    plan: Vec<(bool, u64)>,
    block: usize,
    stable: u32,
    commit_at_end: bool,
}

fn run_job(c: &mut Client, uuid: &[u8; 16], ino: u64, job: &Job) {
    let data = vec![0x5au8; job.block];
    let t = Instant::now();
    let mut ios = 0u64;
    for &(is_write, off) in &job.plan {
        let mut ops = Ops::new();
        ops.putfh(uuid, ino);
        if is_write {
            ops.write(off, &data, job.stable);
        } else {
            ops.read(off, job.block as u32);
        }
        let res = c.call(ops);
        match &res[1] {
            R::Written(n) => assert_eq!(*n as usize, job.block),
            R::Data(d) => assert_eq!(d.len(), job.block),
            r => panic!("unexpected {r:?}"),
        }
        ios += 1;
    }
    if job.commit_at_end {
        let mut ops = Ops::new();
        ops.putfh(uuid, ino);
        ops.commit();
        c.call(ops);
    }
    let dt = t.elapsed().as_secs_f64();
    let mib = ios as f64 * job.block as f64 / (1 << 20) as f64;
    println!(
        "{:<28} {:>8.1} MiB/s  {:>9.0} IOPS  ({} x {} {}, stable={})",
        job.name,
        mib / dt,
        ios as f64 / dt,
        ios,
        fmt_size(job.block),
        if job.plan.iter().any(|&(w, _)| w) {
            "writes"
        } else {
            "reads"
        },
        match job.stable {
            UNSTABLE4 => "UNSTABLE",
            DATA_SYNC4 => "DATA_SYNC",
            _ => "FILE_SYNC",
        },
    );
}

fn fmt_size(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{}MiB", n >> 20)
    } else {
        format!("{}KiB", n >> 10)
    }
}

fn seq_plan(blocks: u64, writes: bool) -> Vec<(bool, u64)> {
    (0..blocks).map(|b| (writes, b * 65536)).collect()
}

fn rand_plan(blocks: u64, n: u64, writes: bool, seed: u64) -> Vec<(bool, u64)> {
    let mut rng = XorShift(seed);
    (0..n)
        .map(|_| (writes, (rng.next() % blocks) * 4096))
        .collect()
}

fn main() {
    // 1 GiB file in a 2 GiB image (sparse). Lives in the workspace, not
    // /tmp, because the pre-sized file occupies real blocks.
    let dir = std::env::var_os("COWNFS_LOADGEN_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/home/hatch/workspace/cownfs"));
    let img = dir.join(format!("cownfs-loadgen-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let file_bytes: u64 = 1 << 30;
    let mut fs = Fs::format(&img, 1 << 19).unwrap(); // 2 GiB
    let ino = fs.create(ROOT_INO, b"loadfile", 0o644, 0, 0).unwrap();
    let z = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < file_bytes {
        let n = ((file_bytes - off) as usize).min(z.len());
        fs.write(ino, off, &z[..n]).unwrap();
        off += n as u64;
    }
    fs.commit().unwrap();
    let uuid = fs.uuid();
    drop(fs);

    let addr = "127.0.0.1:12101";
    spawn_server(&img, addr);
    let mut c = Client::connect(addr);

    let blocks_64k = file_bytes / 65536;
    let blocks_4k = file_bytes / 4096;
    let jobs = [
        Job {
            name: "seqread 64K",
            plan: seq_plan(blocks_64k, false),
            block: 65536,
            stable: UNSTABLE4,
            commit_at_end: false,
        },
        Job {
            name: "seqwrite 64K UNSTABLE",
            plan: seq_plan(blocks_64k, true),
            block: 65536,
            stable: UNSTABLE4,
            commit_at_end: true,
        },
        Job {
            name: "randread 4K",
            plan: rand_plan(blocks_4k, 20000, false, 0x1234),
            block: 4096,
            stable: UNSTABLE4,
            commit_at_end: false,
        },
        Job {
            name: "randwrite 4K UNSTABLE",
            plan: rand_plan(blocks_4k, 20000, true, 0x5678),
            block: 4096,
            stable: UNSTABLE4,
            commit_at_end: true,
        },
        Job {
            name: "randwrite 4K FILE_SYNC",
            plan: rand_plan(blocks_4k, 2000, true, 0x9abc),
            block: 4096,
            stable: FILE_SYNC4,
            commit_at_end: false,
        },
    ];
    println!("NFS loadgen over loopback TCP (1 GiB file, release build):");
    for job in &jobs {
        run_job(&mut c, &uuid, ino, job);
    }
    let _ = std::fs::remove_file(&img);
}
