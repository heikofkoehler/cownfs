# Performance, Resiliency & Scale Plan

Date: 2026-10-03. Author: Neo (for Heiko Koehler).
Basis: codebase analysis at `6919569` + `docs/benchmark.md` (2026-09-29)
+ `docs/production-readiness.md` + `docs/test-gaps.md` + GH issue #3.

## Where we stand

The last session landed the four requested features: async txg batching,
per-UID quotas, `RwLock<Fs>` read concurrency, xattrs. The 2026-09-29
benchmarks showed **commit cost dominates on real disk** (~24ms/commit vs
~8–10ms raw 4K write+fsync). The txg attacks exactly that, but several
second-order costs remain, and resiliency/scale have open gaps.

---

## Workstream A — Performance

### A1. Bitmap write amplification (tactical, high ROI)

**Problem.** `persist_bitmap()` rewrites the *entire* bitmap every txg
(`bitmap_blocks × 4K`). For a 1 TiB image: 256M blocks → 32 MiB bitmap →
8192 blocks rewritten per commit, even if one bit changed. At 100ms txg
intervals that's up to ~320 MiB/s of pure bitmap write overhead.

**Fix.** Track dirty words (a `Vec<u64>` dirty-word index or a
generation counter per word). `persist_bitmap` writes only dirty bitmap
blocks; clear the dirty set after. Expected: bitmap write cost → O(changed).

**Test.** Unit: alloc/free N blocks, assert `persist_bitmap` wrote ≤ K
blocks (instrument via `FileDevice` write counter). Wire: sustained
write workload, measure `dstat`/`iostat` write bytes before/after.

### A2. Allocation cursor (tactical, high ROI)

**Problem.** `Bitmap::alloc()` is first-fit scanning from word 0 every
time — O(words) per alloc, degrading as the disk fills. No hint, no
cursor, no locality (sequential file blocks scatter across the disk).

**Fix.** (1) Persistent allocation cursor: resume scan where the last
alloc succeeded (wraps around). (2) Try to allocate runs contiguously
for sequential writes: when `write()` extends a file, first probe
`last_block+1`. Expected: alloc → amortized O(1); sequential files get
contiguous runs (helps readahead later).

**Test.** Fill disk to 80%, time 10k allocs before/after. Fragmentation
metric: count extents per file after sequential 100 MiB write (want ~1).

### A3. In-place overwrite when block is unshared (tactical, medium ROI)

**Problem.** `write()` *always* allocates a fresh block per 4K touched,
even when overwriting a block no snapshot pins and whose refcount is 1.
That's 2× write amplification (read old + write new + free old at commit)
for the common no-snapshot overwrite case.

**Fix.** In `write()`: if the existing extent's block has refcount 1 and
is not in `snapshot_pinned`, overwrite in place (still update checksum,
still mark txg dirty). Keep CoW when shared. This is what makes the
no-snapshot path competitive with ext4.

**Test.** Shadow-model stress already covers correctness. Add: overwrite
workload, assert allocated-block delta ≈ 0 when no snapshots exist;
assert CoW still happens with an active snapshot (read old data via snap).

### A4. Vectored / readahead reads (tactical, medium ROI)

**Problem.** `read_from()` loops `read_block` per 4K — one `pread` syscall
per block. A 1 MiB NFS READ = 256 syscalls. No readahead: sequential
scans pay full latency per block.

**Fix.** (1) Batch contiguous extents into `preadv` (or a single `pread`
for contiguous runs — A2 makes runs common). (2) Simple readahead:
on sequential `read()`, speculatively read the next 128 KiB into a
per-connection cache. Expected: large-read syscall count → ~1/64th.

**Test.** `strace -c` block counts on 32 MiB sequential read before/after.
Correctness: existing p10_io + p38_stress.

### A5. B-tree node cache (strategic, medium ROI)

**Problem.** Every B-tree descent reads nodes from the device; there is
no in-memory node cache across ops (verify: `BlockArena` caching).
Repeated getattr/lookup on hot files re-reads the same nodes.

**Fix.** LRU cache of deserialized B-tree nodes (e.g. 10k nodes ~ 40 MiB),
invalidated on CoW clone of the node. Must be careful with snapshots
(nodes are immutable once written — cache key = (tree, block)).

**Test.** getattr loop on one file: measure device reads via counter;
expect ~1 miss then hits.

### A6. Async I/O server (strategic, low-medium ROI)

**Problem.** Thread-per-connection, 1000 max. Fine to ~hundreds of
connections; beyond that, thread stack + context-switch overhead.
**Not urgent** — defer until A1–A5 are done and a real bottleneck is
measured. If pursued: tokio + `tokio::fs`-style async device, or
io_uring for the block device.

---

## Workstream B — Resiliency

### B1. Txg failure propagation (tactical, correctness)

**Problem (mine, from the txg build).** The background sync thread ignores
`sync_txg()` errors. A `FILE_SYNC4` waiter blocks on the condvar forever
if syncing fails (disk error, read-only remount).

**Fix.** `TxgCoord` gets an `error: Option<FsError>` field. `sync_txg`
records the error and wakes all waiters; `wait()` returns the error.
Server maps to `NFS4ERR_IO`. Add bounded retries (3×) before giving up.

**Test.** Fault-inject `dev.sync` failure → `FILE_SYNC4` must return
`NFS4ERR_IO`, not hang (test with timeout).

### B2. Bitmap checksums (tactical)

**Problem.** Superblock has CRC32C, data blocks have parent-stored
checksums, but bitmap blocks have none. A torn bitmap write → silent
misallocation (double-alloc = data corruption).

**Fix.** Store CRC32C per bitmap block (first 8 bytes of each 4K bitmap
block, or a parallel checksum area). Verify on load; on mismatch, fall
back to the other ping-pong bitmap area (we already have two areas —
use them).

**Test.** Corrupt one bitmap area byte → open must succeed via the other
area. Corrupt both → clean `NoValidSuperblock`-style error, not panic.

### B3. Fault-injection expansion (GH #3, strategic)

Current: 3 points (AfterFlush/AfterBitmap/AfterSync). Expand to:
- Per-stage faults: after each tree flush, after bitmap word write,
  after superblock slot write.
- **Torn writes**: fake block device that writes only the first N bytes
  of a block (power-loss simulation).
- **Reordered writes**: fake device that reorders the write queue
  within a commit (tests the "superblock last" ordering claim).
- **Bit flips**: already have one gate; extend to bitmap + data blocks.

**Test.** This *is* the test workstream item; see D3.

### B4. Lease renewal + failover test (tactical)

`docs/test-gaps.md` marks lease renewal "fixed" — verify the test exists
and actually kills the renewal thread / expires the lease. Add:
- Two primaries racing for the lease → exactly one wins.
- Primary kill -9 → replica with `--node-id` takes over within 2× TTL.

### B5. Incremental backup (strategic)

**Problem.** `cownfs-backup` is full-only. For TB-scale images, daily
fulls are untenable.

**Fix.** `backup --incremental --since <snapshot>`: walk tree diff
(`diff_roots` exists) and ship only changed blocks + a block list.
Restore applies on top of the base image. Snapshots make this natural.

**Test.** Full + 3 incrementals → restore each → byte-compare vs live.

### B6. Quota/xattr crash consistency (tactical)

- Quotas are in-memory; usage rebuilt on open — fine. But a crash
  between `write()` and `commit()` leaves usage over-counted until
  reopen. Document; or rebuild lazily on first quota check after open.
- Xattrs: `.xattrs` rewrite is a normal CoW file write — crash-safe.
  Add a crash test: kill -9 mid-`setxattr` storm → reopen → xattr map
  must be parseable (no torn records).

---

## Workstream C — Scale

### C1. Bitmap memory (tactical)

**Problem.** Bitmap is fully in-memory: 1 TiB → 32 MiB, 100 TiB → 3.2 GiB.
Plus `pending_free` vec and `snapshot_pinned` set.

**Fix.** Paged bitmap: keep hot words resident, page cold words from the
inactive bitmap area on demand. Or: roaring-bitmap-style run encoding
for the common mostly-empty/mostly-full cases. Start with lazy load —
the bitmap is already checksummed per block after B2.

### C2. Readdir scalability (tactical)

**Problem.** `readdir()` materializes the full entry vec; large dirs
(1M files) blow memory per READDIR call.

**Fix.** Cursor-based iteration: `readdir_range(dir_ino, cookie, maxcount)`
that walks the B-tree range without collecting. The NFS layer already
pages via cookies — plumb it through.

### C3. Connection-scale measurement (tactical)

We claim 1000 max connections but never measured past dozens. Run:
500 idle + 100 active clients, measure p99 latency vs 10 clients.
If thread-per-connection collapses, that's the trigger for A6.

### C4. Shard rebalance (strategic)

Referrals/sharding exist (docs/horizontal-scaling-plan.md Phases 1–3).
Missing: online shard split/move. Design: snapshot shard → send to new
node → update referral table → delete source (all offline-safe via CoW).
Defer until a real multi-TB deployment needs it.

---

## Workstream D — Testing (explicit)

### D1. Close the known gaps (tactical)

From `docs/test-gaps.md` backlog + new-feature gaps:
- [ ] Audit-log format test (JSON schema assertion).
- [ ] NFS `.xattrs` hiding: LOOKUP `.xattrs` → NOENT (wire test).
- [ ] Quota edge cases: rename-over-existing, SETATTR uid change,
      rmdir release, snapshot-pinned charging policy (decide + test).
- [ ] Txg: `wait()` wakes on error (B1); interval=0 disables thread.
- [ ] RwLock contention: N readers + 1 writer, assert reader p99
      doesn't regress vs no-writer baseline.

### D2. Soak + fio (GH #3, strategic)

- [ ] Multi-hour soak: `COWNFS_SOAK_ITERS=100M` create/write/rename/
      delete/snapshot mix; record results in `docs/p7-soak-results.md`.
- [ ] **fio over NFS**: randread/randwrite/seqread/seqwrite, 4K/64K/1M,
      iodepth 1/16 — the industry baseline. Publish numbers next to the
      engine benchmarks in `docs/benchmark.md`.
- [ ] Long-run fd exhaustion / connection churn: open/close 100k
      connections sequentially, assert no fd leak (`/proc/self/fd` count).

### D3. Fault-injection harness (strategic)

Build the fake block device (torn + reordered + bit-flip modes) as a
`cownfs-core` test backend. Gate: every commit-stage fault point ×
every fault mode → reopen → `check()` clean, no panic. This is the
meat of GH #3.

### D4. Performance regression suite (tactical)

`cownfs-bench` exists but isn't gated. Add:
- `cargo bench` smoke subset (30s) run in CI-ish fashion before pushes.
- Track: commit latency p99, 4K sync-write IOPS, bitmap bytes/commit
  (A1's metric), alloc latency at 80% full (A2's metric).
- Fail the push gate if p99 regresses >20% vs recorded baseline.

### D5. Chaos: kill -9 the server mid-workload (tactical)

p38_stress + `kill -9` at random points → restart → `fsck` clean +
data matches last COMMITTed state. Automate as `p39_chaos.rs`.

---

## Suggested order

**Phase 1 (weeks, tactical):** A1, A2, B1, B2, D1, D4.
Biggest perf wins + correctness gaps, all testable in days each.

**Phase 2 (month, strategic):** A3, A4, B3/D3, D2, D5, C1, C2.
Write amplification, reads, fault harness, soak/fio, chaos.

**Phase 3 (quarter, strategic):** A5, B5, C3→A6, C4.
Cache, incremental backup, async I/O if C3 says so, rebalance.

## What I'd do first

A1 (bitmap dirty tracking) — it's the largest remaining write
amplification, it's a 1–2 day change, and D4 gives us the metric to
prove it. Pair it with B1 (txg error propagation) since both touch
the commit path and both are correctness-adjacent.
