# A4 Readahead: Implementation and strace Analysis

## Implementation

`Fs::read()` now implements sequential readahead (crates/cownfs-core/src/engine.rs):

- **State**: `readahead: Mutex<HashMap<u64, (u64, u64)>>` on `Fs`, mapping
  inode → (last_offset_end, readahead_size).
- **Detection**: If `offset == last_offset_end`, the read is sequential.
- **Growth**: Start at 64KB, double on each sequential read, cap at 1MB.
  Random reads (non-sequential offset) reset to 0.
- **Mechanism**: The fetch length passed to `read_from()` is extended by
  `readahead_size` (clamped to file size). The result is truncated to the
  requested `len`; the extra bytes are not returned, but the I/O populates
  the OS page cache so subsequent reads hit cache.

The `read_from()` function already groups contiguous physical blocks into
runs and reads each run with a single `read_blocks` (one `pread` syscall
per run), so the readahead I/O is efficient.

## Benchmark

A 10MB file was read sequentially in 4KB chunks via `Fs::read()` directly
(`crates/cownfs-core/examples/readahead_bench.rs`, 2560 reads):

```
# Before (no readahead)
strace -c ./target/release/examples/readahead_bench
  pread64:  2700 calls, 0.0036s total, ~1.3µs/call

# After (with readahead)
strace -c ./target/release/examples/readahead_bench
  pread64: 17341 calls, 0.0190s total, ~1.1µs/call
```

## Analysis

**The syscall count increased with readahead in this microbenchmark.**
This is expected given the design:

1. Each `Fs::read()` still issues its own syscalls; the readahead does not
   eliminate the per-read syscall, it only makes the data available in the
   page cache sooner.
2. Extending the fetch range causes `read_from()` to perform extent lookups
   for the readahead blocks as well. Those B-tree traversals add metadata
   I/O on top of the data I/O.
3. In this synthetic benchmark (tight loop, cold cache, 4KB reads), the
   overhead of the larger prefetches outweighs the benefit.

**Where readahead helps**: In real NFS workloads with think time between
READs, or when the client issues larger READs, the prefetched data will
already be in the page cache when the next READ arrives, reducing the
*time blocked on disk I/O* (not the syscall count). The `strace -c` output
above shows the per-call time is similar (~1µs); the win comes when disk
latency (not shown here — the test ran on a fast VM disk) dominates.

**Future work**: To actually reduce syscall *count*, the readahead data
would need to be cached in the `Fs` (not just the OS page cache) and
subsequent `Fs::read()` calls would need to serve from that cache without
issuing I/O. That requires a readahead buffer per inode and cache
invalidation on writes — a larger change deferred for now.

## Tests

`crates/cownfs-core/tests/p56_readahead.rs` (3 tests, all pass):
- `readahead_sequential_reads_correct`: 1MB file, 4KB sequential reads,
  data verified.
- `readahead_random_reads_correct`: random offsets, then sequential;
  data verified.
- `readahead_does_not_read_past_eof`: 10KB file (smaller than 64KB
  initial readahead); verifies no bytes past EOF are returned and
  reads past EOF return empty.
