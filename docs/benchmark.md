# cownfs benchmarks and stress test

Date: 2026-09-29. Machine: 2× AMD EPYC 9D25, 8 GiB RAM, Linux 7.0.0-38-generic.
Release build (`cargo build --release -p cownfs-bench`).

## How to run

```
cargo run --release -p cownfs-bench          # full run
cargo run --release -p cownfs-bench -- --quick
COWNFS_BENCH_DIR=/path/to/disk ./target/release/cownfs-bench
```

`COWNFS_BENCH_DIR` selects where bench images live (default: temp dir).
The stress test: `cargo test -p cownfs-core --test stress`
(`COWNFS_STRESS_ITERS`, default 20000).

## Results

Two configurations were measured: images on tmpfs (isolates engine/CPU
cost — no disk in the path) and on a btrfs virtual disk (realistic).

### Engine (direct `Fs` API)

| Benchmark | tmpfs (median of 3) | btrfs disk |
|---|---|---|
| seq_write, 32 MiB, 64 KiB writes, commit/4 MiB | ~620 MiB/s (329–651) | 80 MiB/s |
| rand_write, 8192× 4 KiB over 64 MiB, commit/1024 | ~95k IOPS (28k–119k) | ~16k IOPS |
| seq_read, 32 MiB, 64 KiB reads | ~1.3–2.8 GiB/s (page cache) | 2.7 GiB/s (page cache) |
| rand_read, 8192× 4 KiB over 64 MiB | ~480k IOPS (page cache) | ~440k IOPS (page cache) |
| create, 20k files in one dir, commit/2000 | ~60k files/s (21k–61k) | ~24k files/s |
| readdir, 20k entries | ~2.1M entries/s | ~1.1M entries/s |
| commit latency, 128 B dirty (mean / p50 / p99) | 105µs / 42µs / 385µs | 24ms / 23ms / 70ms |
| sync_write, 4 KiB write + commit each | ~3.4k IOPS (3.0k–6.2k) | 40 IOPS |
| snapshot create+delete | 3.7µs/op | 7µs/op |

### NFS (real server, loopback TCP)

| Benchmark | tmpfs | btrfs disk |
|---|---|---|
| WRITE round trip, 4 KiB FILE_SYNC (mean / p50 / p99) | 860µs / 580µs / 1.8ms | 27.5ms / 25.8ms / 71ms |
| READ round trip, 4 KiB (mean) | 22–35µs | 25µs |
| COMMIT round trip, clean (mean) | 32–51µs | 24ms |
| WRITE throughput, 32 KiB FILE_SYNC | 17–50 MiB/s | 1.1 MiB/s |

## What the numbers say

- **Commit cost dominates on real disk.** A commit (flush dirty blocks,
  persist bitmap, fsync, flip superblock) costs ~24ms on this VM's disk,
  vs ~8–10ms for a raw 4 KiB write+fsync baseline measured with
  `dd`-style probing. Every FILE_SYNC NFS write pays one commit, hence
  ~1.1 MiB/s for 32 KiB FILE_SYNC writes and 40 IOPS for 4 KiB
  write+commit at the engine level. UNSTABLE writes (no per-op commit)
  were not benchmarked; they would be far faster.
- **Reads are page-cache reads.** Both tmpfs and disk runs serve reads
  from the Linux page cache (images were just written). Cold-cache read
  numbers would be lower and were not measured.
- **tmpfs variance is high** (e.g. rand_write 28k–119k IOPS across
  runs): this VM's storage and scheduling are noisy. Treat all figures
  as order-of-magnitude, not datasheet values.
- **Snapshots are nearly free** (~4µs): create+delete is refcount bumps
  plus one snap-record insert.
- **NFS transport overhead is small** relative to commit cost: a clean
  COMMIT round trip is ~25µs on tmpfs; on disk the 24ms is the commit
  itself, not the wire.

## Stress test

`crates/cownfs-core/tests/stress.rs`: 20k randomized ops (create,
overlapping random writes, truncate, rename, unlink, hard link, mkdir,
snapshot create/delete) against a byte-exact in-memory shadow model.
Commits at random 1–20 op intervals; the image is reopened and `check()`ed
every 5000 ops. Snapshot isolation is probed: after snapshotting, the
live file is fully overwritten and `snapshot_read` must return the
pre-snapshot bytes. Final gate: every surviving file verified
byte-for-byte (lookup → size → full read) and `check()` clean. Passes.

**The stress test caught a real bug on its first full run** (fixed in
`store.rs`/`engine.rs`): after reopening an image that contains
snapshots, the arena live-node seed counted only blocks reachable from
the *live* tree roots. Blocks referenced exclusively by a snapshot
(a root the live tree CoW-cloned away from after the snapshot) are
allocated but uncounted, so the first `snapshot_delete` releasing such a
block underflowed `alloc_count` (`subtract with overflow` in
`free_block`). The seed now walks the union of live roots and every
snapshot record's pinned roots (`BlockArena::reachable_multi`).
