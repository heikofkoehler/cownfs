//! cownfs-bench: throughput and latency benchmarks for the cownfs engine
//! and the userspace NFSv4 server.
//!
//! Engine benches drive `cownfs_core::engine::Fs` directly. NFS benches run
//! the real server on 127.0.0.1 and speak NFSv4 COMPOUND over TCP with a
//! minimal client (see nfs_client.rs), measuring true round-trip cost.
//!
//! Usage: `cownfs-bench [--quick]` (quick reduces iteration counts).
//! `COWNFS_BENCH_DIR` overrides the image directory (default: temp dir).
//! Always run a release build; a debug build prints a warning.

mod nfs_client;

use cownfs_core::engine::{Fs, ROOT_INO};
use nfs_client::{Client, Ops, R};
use std::path::PathBuf;
use std::time::Instant;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

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

fn img_path(tag: &str) -> PathBuf {
    let mut p = std::env::var_os("COWNFS_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    p.push(format!("cownfs-bench-{}-{}.img", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    let idx = ((p / 100.0) * sorted.len() as f64) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// (mean, p50, p99) in nanoseconds.
fn lat_stats(ns: &mut Vec<u128>) -> (u128, u128, u128) {
    ns.sort_unstable();
    let mean = ns.iter().sum::<u128>() / ns.len() as u128;
    (mean, pct(ns, 50.0), pct(ns, 99.0))
}

fn fmt_dur(ns: u128) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

fn mib_per_s(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / (1 << 20) as f64 / secs
}

// ---------------------------------------------------------------------------
// engine benchmarks
// ---------------------------------------------------------------------------

fn b_seq_write(quick: bool) {
    let total: u64 = if quick { 8 << 20 } else { 32 << 20 };
    let chunk = 64 * 1024usize;
    let img = img_path("seqw");
    let mut fs = Fs::format(&img, 32768).unwrap(); // 128 MiB image
    let ino = fs.create(ROOT_INO, b"seq", 0o644, 0, 0).unwrap();
    let data = vec![0xabu8; chunk];
    let t = Instant::now();
    let mut off = 0u64;
    let mut n = 0u64;
    while off < total {
        fs.write(ino, off, &data).unwrap();
        off += chunk as u64;
        n += 1;
        if n % 64 == 0 {
            fs.commit().unwrap(); // every 4 MiB
        }
    }
    fs.commit().unwrap();
    let el = t.elapsed();
    println!(
        "seq_write    {:>6.1} MiB/s   ({:.1} MiB in {}, 64 KiB writes, commit/4MiB)",
        mib_per_s(total, el.as_secs_f64()),
        total as f64 / (1 << 20) as f64,
        fmt_dur(el.as_nanos()),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_rand_write(quick: bool) {
    let ops: usize = if quick { 2000 } else { 8192 };
    let file_size = 64u64 << 20;
    let img = img_path("randw");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"rand", 0o644, 0, 0).unwrap();
    fs.truncate(ino, file_size).unwrap();
    fs.commit().unwrap();
    let mut rng = XorShift(0x9e3779b97f4a7c15);
    let data = vec![0xcdu8; 4096];
    let blocks = file_size / 4096;
    let t = Instant::now();
    for i in 0..ops {
        let blk = rng.next() % blocks;
        fs.write(ino, blk * 4096, &data).unwrap();
        if i % 1024 == 1023 {
            fs.commit().unwrap();
        }
    }
    fs.commit().unwrap();
    let el = t.elapsed();
    let iops = ops as f64 / el.as_secs_f64();
    println!(
        "rand_write   {:>8.0} IOPS    ({ops} x 4 KiB random writes over 64 MiB, commit/1024, {})",
        iops,
        fmt_dur(el.as_nanos()),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_seq_read(quick: bool) {
    let total: u64 = if quick { 8 << 20 } else { 32 << 20 };
    let chunk = 64 * 1024usize;
    let img = img_path("seqr");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"seq", 0o644, 0, 0).unwrap();
    let data = vec![0xabu8; chunk];
    let mut off = 0u64;
    while off < total {
        fs.write(ino, off, &data).unwrap();
        off += chunk as u64;
    }
    fs.commit().unwrap();
    let t = Instant::now();
    let mut off = 0u64;
    let mut got = 0u64;
    while off < total {
        let v = fs.read(ino, off, chunk).unwrap();
        got += v.len() as u64;
        off += chunk as u64;
    }
    let el = t.elapsed();
    assert_eq!(got, total);
    println!(
        "seq_read     {:>6.1} MiB/s   ({:.1} MiB in {}, 64 KiB reads)",
        mib_per_s(got, el.as_secs_f64()),
        got as f64 / (1 << 20) as f64,
        fmt_dur(el.as_nanos()),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_rand_read(quick: bool) {
    let ops: usize = if quick { 2000 } else { 8192 };
    let file_size = 64u64 << 20;
    let img = img_path("randr");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"rand", 0o644, 0, 0).unwrap();
    // Fill with real data so reads hit allocated extents, not holes.
    let fill = vec![0x5du8; 1 << 20];
    let mut off = 0u64;
    while off < file_size {
        fs.write(ino, off, &fill).unwrap();
        off += fill.len() as u64;
    }
    fs.commit().unwrap();
    let mut rng = XorShift(0x123456789abcdef);
    let blocks = file_size / 4096;
    let t = Instant::now();
    let mut got = 0u64;
    for _ in 0..ops {
        let blk = rng.next() % blocks;
        let v = fs.read(ino, blk * 4096, 4096).unwrap();
        got += v.len() as u64;
    }
    let el = t.elapsed();
    assert_eq!(got, ops as u64 * 4096);
    let iops = ops as f64 / el.as_secs_f64();
    println!(
        "rand_read    {:>8.0} IOPS    ({ops} x 4 KiB random reads over 64 MiB, {})",
        iops,
        fmt_dur(el.as_nanos()),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_create(quick: bool) {
    let n: usize = if quick { 2000 } else { 20000 };
    let img = img_path("create");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let dir = fs.mkdir(ROOT_INO, b"d", 0o755, 0, 0).unwrap();
    let t = Instant::now();
    for i in 0..n {
        let name = format!("f{i:06}");
        fs.create(dir, name.as_bytes(), 0o644, 0, 0).unwrap();
        if i % 2000 == 1999 {
            fs.commit().unwrap();
        }
    }
    fs.commit().unwrap();
    let el = t.elapsed();
    println!(
        "create       {:>8.0} files/s ({n} creates in one dir, commit/2000, {})",
        n as f64 / el.as_secs_f64(),
        fmt_dur(el.as_nanos()),
    );
    let t = Instant::now();
    let entries = fs.readdir(dir).unwrap();
    let el = t.elapsed();
    println!(
        "readdir      {:>8.0} entries/s ({} entries in {})",
        entries.len() as f64 / el.as_secs_f64(),
        entries.len(),
        fmt_dur(el.as_nanos()),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_commit_latency(quick: bool) {
    let n: usize = if quick { 50 } else { 200 };
    let img = img_path("commit");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"c", 0o644, 0, 0).unwrap();
    let data = vec![0xefu8; 128];
    let mut ns = Vec::with_capacity(n);
    for i in 0..n {
        fs.write(ino, (i * 128) as u64, &data).unwrap();
        let t = Instant::now();
        fs.commit().unwrap();
        ns.push(t.elapsed().as_nanos());
    }
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "commit       mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} commits, 128 B dirty each)",
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_sync_write(quick: bool) {
    // FILE_SYNC-style: every 4 KiB write is followed by a commit.
    let n: usize = if quick { 100 } else { 500 };
    let img = img_path("syncw");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"s", 0o644, 0, 0).unwrap();
    let data = vec![0x99u8; 4096];
    let mut ns = Vec::with_capacity(n);
    for i in 0..n {
        fs.write(ino, (i * 4096) as u64, &data).unwrap();
        let t = Instant::now();
        fs.commit().unwrap();
        ns.push(t.elapsed().as_nanos());
    }
    let el_ns: u128 = ns.iter().sum();
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "sync_write   {:>8.0} IOPS    mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} x 4 KiB write+commit, {})",
        n as f64 / (el_ns as f64 / 1e9),
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
        fmt_dur(el_ns),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_snapshot(quick: bool) {
    let n: usize = if quick { 10 } else { 20 };
    let img = img_path("snap");
    let mut fs = Fs::format(&img, 32768).unwrap();
    for i in 0..5 {
        let name = format!("f{i}");
        let ino = fs.create(ROOT_INO, name.as_bytes(), 0o644, 0, 0).unwrap();
        fs.write(ino, 0, &vec![i as u8; 4096]).unwrap();
    }
    fs.commit().unwrap();
    let t = Instant::now();
    for i in 0..n {
        let name = format!("bsnap{i}");
        let id = fs.snapshot_create(name.as_bytes()).unwrap();
        fs.snapshot_delete(id).unwrap();
    }
    let el = t.elapsed();
    println!(
        "snapshot     {:>8} mean/op ({} create+delete cycles)",
        fmt_dur(el.as_nanos() / n as u128),
        n,
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

// ---------------------------------------------------------------------------
// NFS benchmarks (real server over loopback)
// ---------------------------------------------------------------------------

fn spawn_server(img: &std::path::Path, addr: &str) {
    let img2 = img.to_path_buf();
    let addr = addr.to_string();
    std::thread::spawn(move || {
        let fs = Fs::open(&img2).unwrap();
        let _ = cownfs_nfs::server::serve(&addr, cownfs_nfs::server::Shared::new(fs));
    });
}

/// Format an image with one pre-sized file; return (uuid, ino).
fn nfs_fixture(tag: &str, file_bytes: u64) -> (PathBuf, [u8; 16], u64) {
    let img = img_path(tag);
    let mut fs = Fs::format(&img, 16384).unwrap(); // 64 MiB
    let ino = fs.create(ROOT_INO, b"nfsfile", 0o644, 0, 0).unwrap();
    // Pre-size with zeros in 1 MiB chunks so reads have backing data.
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
    (img, uuid, ino)
}

fn b_nfs_write_rt(quick: bool) {
    let n: usize = if quick { 100 } else { 500 };
    let addr = "127.0.0.1:12061";
    let (img, uuid, ino) = nfs_fixture("nfsw", 4 << 20);
    spawn_server(&img, addr);
    let mut c = Client::connect(addr);
    let stable = Client::stable();
    let data = vec![0x5au8; 4096];
    let mut ns = Vec::with_capacity(n);
    for i in 0..n {
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.write((i * 4096) as u64, &data, stable);
        let t = Instant::now();
        let res = c.check(ops, 2);
        ns.push(t.elapsed().as_nanos());
        assert!(matches!(res[1], R::Written(4096)));
    }
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "nfs_write_rt mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} x 4 KiB FILE_SYNC WRITE round trips)",
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
    );
    let _ = std::fs::remove_file(&img);
}

fn b_nfs_read_rt(quick: bool) {
    let n: usize = if quick { 100 } else { 500 };
    let addr = "127.0.0.1:12062";
    let (img, uuid, ino) = nfs_fixture("nfsr", 4 << 20);
    spawn_server(&img, addr);
    let mut c = Client::connect(addr);
    let mut ns = Vec::with_capacity(n);
    let mut rng = XorShift(0xabcdef123456789);
    let blocks = (4 << 20) / 4096u64;
    for _ in 0..n {
        let blk = rng.next() % blocks;
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.read(blk * 4096, 4096);
        let t = Instant::now();
        let res = c.check(ops, 2);
        ns.push(t.elapsed().as_nanos());
        assert!(matches!(res[1], R::Data(ref d) if d.len() == 4096));
    }
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "nfs_read_rt  mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} x 4 KiB READ round trips)",
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
    );
    let _ = std::fs::remove_file(&img);
}

fn b_nfs_commit_rt(quick: bool) {
    let n: usize = if quick { 50 } else { 200 };
    let addr = "127.0.0.1:12063";
    let (img, uuid, ino) = nfs_fixture("nfsc", 1 << 20);
    spawn_server(&img, addr);
    let mut c = Client::connect(addr);
    let mut ns = Vec::with_capacity(n);
    for _ in 0..n {
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.commit();
        let t = Instant::now();
        c.check(ops, 2);
        ns.push(t.elapsed().as_nanos());
    }
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "nfs_commit_rt mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} COMMIT round trips)",
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
    );
    let _ = std::fs::remove_file(&img);
}

fn b_nfs_write_bw(quick: bool) {
    let total: u64 = if quick { 2 << 20 } else { 8 << 20 };
    let chunk = 32 * 1024usize;
    let addr = "127.0.0.1:12064";
    let (img, uuid, ino) = nfs_fixture("nfsbw", 0);
    spawn_server(&img, addr);
    let mut c = Client::connect(addr);
    let stable = Client::stable();
    let data = vec![0x7eu8; chunk];
    let t = Instant::now();
    let mut off = 0u64;
    while off < total {
        let mut ops = Ops::new();
        ops.putfh(&uuid, ino);
        ops.write(off, &data, stable);
        let res = c.check(ops, 2);
        assert!(matches!(res[1], R::Written(n) if n == chunk as u32));
        off += chunk as u64;
    }
    let el = t.elapsed();
    println!(
        "nfs_write_bw {:>6.1} MiB/s   ({:.1} MiB in {}, 32 KiB FILE_SYNC WRITEs)",
        mib_per_s(total, el.as_secs_f64()),
        total as f64 / (1 << 20) as f64,
        fmt_dur(el.as_nanos()),
    );
    let _ = std::fs::remove_file(&img);
}

// ---------------------------------------------------------------------------

fn b_bitmap_write(quick: bool) {
    // A1 metric: bytes of bitmap written per commit.
    let n: usize = if quick { 20 } else { 100 };
    let img = img_path("bmpw");
    let mut fs = Fs::format(&img, 32768).unwrap();
    let ino = fs.create(ROOT_INO, b"b", 0o644, 0, 0).unwrap();
    let data = vec![0xabu8; 4096];
    let mut total_bytes = 0u64;
    for i in 0..n {
        fs.write(ino, (i * 4096) as u64, &data).unwrap();
        fs.commit().unwrap();
        total_bytes += fs.last_bitmap_write_bytes();
    }
    println!(
        "bitmap_write avg {:>8.0} bytes/commit   ({n} commits)",
        total_bytes as f64 / n as f64,
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn b_alloc_latency(quick: bool) {
    // A2 metric: alloc latency at 80% full.
    let blocks = 8192u64;
    let img = img_path("alcl");
    let mut fs = Fs::format(&img, blocks).unwrap();
    // Fill to ~80%.
    let target = (blocks as f64 * 0.8) as u64;
    let mut allocated = 0u64;
    let mut ino_ctr = 0;
    while allocated < target {
        let ino = fs
            .create(ROOT_INO, format!("f{ino_ctr}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        fs.write(ino, 0, &vec![0u8; 4096 * 4]).unwrap();
        allocated += 5; // approx
        ino_ctr += 1;
    }
    fs.commit().unwrap();
    // Time allocs at 80% full.
    let n: usize = if quick { 100 } else { 1000 };
    let mut ns = Vec::with_capacity(n);
    for i in 0..n {
        let ino = fs
            .create(ROOT_INO, format!("g{i}").as_bytes(), 0o644, 0, 0)
            .unwrap();
        let t = Instant::now();
        fs.write(ino, 0, &[0u8; 4096]).unwrap();
        ns.push(t.elapsed().as_nanos());
    }
    let (mean, p50, p99) = lat_stats(&mut ns);
    println!(
        "alloc_80pct  mean {:>8}  p50 {:>8}  p99 {:>8}   ({n} allocs at 80% full)",
        fmt_dur(mean),
        fmt_dur(p50),
        fmt_dur(p99),
    );
    drop(fs);
    let _ = std::fs::remove_file(&img);
}

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build — numbers are not representative. Use --release.");
    }
    let quick = std::env::args().any(|a| a == "--quick");
    println!("cownfs-bench ({})", if quick { "quick" } else { "full" });
    println!("--- engine ---");
    b_seq_write(quick);
    b_rand_write(quick);
    b_seq_read(quick);
    b_rand_read(quick);
    b_create(quick);
    b_commit_latency(quick);
    b_sync_write(quick);
    b_bitmap_write(quick);
    b_alloc_latency(quick);
    b_snapshot(quick);
    println!("--- nfs (loopback) ---");
    b_nfs_write_rt(quick);
    b_nfs_read_rt(quick);
    b_nfs_commit_rt(quick);
    b_nfs_write_bw(quick);
}
