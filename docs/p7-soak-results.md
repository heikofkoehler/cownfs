# cownfs #3 — P7 Hardening: Benchmark & Soak Results

Date: 2026-10-02
Build: release

## Engine benchmarks (local, release)

```
seq_write      84.7 MiB/s   (32.0 MiB in 377.60ms, 64 KiB writes, commit/4MiB)
rand_write      24529 IOPS    (8192 x 4 KiB random writes over 64 MiB, commit/1024, 333.97ms)
seq_read     2145.2 MiB/s   (32.0 MiB in 14.92ms, 64 KiB reads)
rand_read      384693 IOPS    (8192 x 4 KiB random reads over 64 MiB, 21.29ms)
create          22542 files/s (20000 creates in one dir, commit/2000, 887.25ms)
readdir       1199422 entries/s (20000 entries in 16.67ms)
commit       mean  180.7µs  p50   87.6µs  p99   1.68ms   (200 commits, 128 B dirty each)
sync_write       1349 IOPS    mean  741.1µs  p50  194.3µs  p99   5.97ms   (500 x 4 KiB write+commit, 370.56ms)
snapshot        4.0µs mean/op (20 create+delete cycles)
```

## NFS loopback benchmarks (release)

```
nfs_write_rt mean   2.29ms  p50   2.26ms  p99   8.65ms   (500 x 4 KiB FILE_SYNC WRITE round trips)
nfs_read_rt  mean   57.7µs  p50   22.9µs  p99   1.14ms   (500 x 4 KiB READ round trips)
nfs_commit_rt mean  182.7µs  p50  158.1µs  p99  875.0µs   (200 COMMIT round trips)
nfs_write_bw    8.7 MiB/s   (8.0 MiB in 923.59ms, 32 KiB FILE_SYNC WRITEs)
```

Note: fio over NFS requires a kernel NFS mount, which is unavailable on the
dev host. The nfs_* loopback numbers above are the userspace-client
equivalent for context.

## Soak test

`p7_soak` with COWNFS_SOAK_ITERS=100000000 (100M, ~3 hours at 10k iters/sec).
Workload: create/write/rename/delete/snapshot mix. Verifies the image is
clean after the run.

Status: RUNNING (started 2026-10-02 ~15:30 PDT, expected done ~18:30 PDT)
