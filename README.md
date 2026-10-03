# cownfs

A from-scratch userspace copy-on-write filesystem in the spirit of ZFS/Btrfs,
written in Rust, exported over NFS. One server binary, mountable from any OS
with an NFS client — no local mount, no FUSE, no Ganesha.

## Status

**Minimal production readiness** (2026-10-02). 178 tests pass. The core
engine (P0–P7) is done and heavily tested. Production hardening (P0–P2)
is complete: data checksums, backup/restore, leader fencing, metrics,
health checks, structured logging, audit log, graceful shutdown, throttling.

See [docs/production-readiness.md](docs/production-readiness.md) for the
full assessment.

What works:

- **P0** — 4 KiB block device, CRC32C checksums, free-space bitmap,
  ping-pong superblock with generation commit
- **P1** — in-memory generational CoW B-tree (model-tested against `BTreeMap`
  over 135k randomized ops)
- **P2** — block-backed CoW engine: files, dirs, symlinks, hard links,
  extents, refcounted blocks, **parent-stored data checksums**
- **P3** — snapshots: cheap shared-root copies with CoW divergence and
  snapshot-pinned data blocks
- **P4** — hand-rolled RPC/XDR and a read-only NFSv4.0 server
- **P5** — NFSv4 mutation path (OPEN/CREATE/WRITE/COMMIT/REMOVE/RENAME/…)
  with a userspace test client
- **P6** — NFSv4 state management (clientids, opens, locks, seqids,
  share/deny, OPEN_DOWNGRADE)
- **P7** — fault injection, unreachable-block reclaim (`fsck --reclaim`),
  soak test, malformed-XDR handling
- **P8–P11** — wire-level test harness (real TCP, ephemeral ports) + 40 tests:
  error paths, attribute coverage, I/O semantics, state machine
- **P12** — shared filesystem and NFSv4 state across connections; concurrent
  per-connection serving with multi-client tests
- **P13** — real-world client patterns: macOS `SECINFO` compounds, getattr-heavy
  lookups, invalid UTF-8 rejection, `GUARDED4`/`EXCLUSIVE4`, `ILLEGAL` op,
  oversized compounds
- **P14** — kernel-tarball workload test (untar + content/metadata verification)
- **P15** — concurrency stress tests
- **P16** — read-only server mode (for replicas)
- **P17–P18** — replication: snapshot diff, `cownfs-replicate` send/receive,
  fault-injection tests, lag metrics
- **P19** — `cownfs-ds` data server daemon (pNFS building block, experimental)
- **P20** — NFSv4.1 session semantics (`EXCHANGE_ID`, `CREATE_SESSION`,
  `SEQUENCE`) — experimental
- **P21** — pNFS file layouts (single data server, layout recall) — experimental
- **P22** — NFSv4.0 referrals: `fs_locations`, `NFS4ERR_MOVED`, transparent
  sharding via `cownfs-server --referrals`
- **P23** — replication ordering: superblock-last crash safety
- **P24** — referral protocol tests
- **P25** — data block checksums (parent-stored CRC32C, verified on read)
- **P26** — `cownfs-backup`: portable backup/restore with checksums
- **P27** — leader lease: superblock fencing prevents split-brain
- **P28** — throttling: per-client and per-file rate limits

**Production features:**
- **Observability**: Prometheus metrics (`:port+1000/metrics`), health
  (`:port+1000/healthz`), JSON structured logs (`--log-level`), audit log
- **Reliability**: graceful shutdown (SIGTERM → drain → commit), connection
  limits (1000), idle timeouts (300s), leader lease fencing
- **Data safety**: parent-stored CRC32C on all data blocks, `cownfs-backup`
  for offline backups, crash-safe replication

The server has been exercised against the real macOS NFS client (xnu), which
exposed and drove fixes for: `GETATTR` with empty attribute masks (ESTALE),
`ACCESS` write-bit grants (macOS won't attempt CREATE without them), and
`OP_SECINFO` bundled into lookup/create/remove compounds.

Known gaps (tracked as [GitHub issues](https://github.com/heikofkoehler/cownfs/issues)):

- **Security**: AUTH_SYS only (trusted networks only). Kerberos deferred.
- **Features**: No quotas, ACLs, xattrs, or delegations (P3, not started).
- pNFS is experimental; v4.0 referrals are the production scale-out path.
  See [docs/v40-scaleout.md](docs/v40-scaleout.md).
- Fault injection doesn't yet cover torn/reordered device writes.

## Quick start

Prerequisites: a Rust toolchain — install from https://rustup.rs.

```sh
git clone https://github.com/heikofkoehler/cownfs.git
cd cownfs
cargo build --release
./target/release/cownfs-mkfs --size 1G /tmp/cow.img
./target/release/cownfs-server /tmp/cow.img 127.0.0.1:2049 &
```

### Linux

```sh
sudo mkdir -p /mnt/cow
sudo mount -t nfs -o vers=4.0,port=2049 127.0.0.1:/ /mnt/cow
# ... use /mnt/cow ...
sudo umount /mnt/cow
```

### macOS

```sh
sudo mkdir -p /Volumes/cow
sudo mount -t nfs -o vers=4.0,tcp,port=2049,resvport 127.0.0.1:/ /Volumes/cow
# ... use /Volumes/cow ...
sudo umount /Volumes/cow
```

(On older macOS releases that reject a `4.x` minor, use `vers=4` instead of
`vers=4.0`.)

Check and repair the image (either platform):

```sh
./target/release/cownfs-fsck /tmp/cow.img            # check
./target/release/cownfs-fsck --reclaim /tmp/cow.img  # check + reclaim
```

Replicate to a second image:

```sh
./target/release/cownfs-replicate /tmp/cow.img /tmp/replica.img
```

Tests and benchmarks:

```sh
cargo test --workspace                       # 178 tests
cargo run --release -p cownfs-bench          # throughput/latency numbers
COWNFS_STRESS_ITERS=50000 cargo test -p cownfs-core --test stress
```

### Production deployment

```sh
# Primary with leader lease (prevents split-brain)
./target/release/cownfs-server --node-id primary1 --lease-ttl 30 /data/cow.img 0.0.0.0:2049 &

# Read replica
./target/release/cownfs-server --read-only /data/replica.img 0.0.0.0:2050 &

# Metrics and health (on port+1000)
curl http://localhost:3049/metrics
curl http://localhost:3049/healthz

# Backup (cron daily)
./target/release/cownfs-backup create /data/cow.img /backups/cow-$(date +%F).bak

# Referral server for transparent sharding
./target/release/cownfs-server --referrals /etc/cownfs/referrals.conf /data/ns.img 0.0.0.0:2049 &
```

See [docs/production-readiness.md](docs/production-readiness.md) for the
full operational guide.

## Layout

- `crates/cownfs-core/` — block device, superblock, bitmap, checksums (P0);
  CoW B-trees and the filesystem engine (P1–P3, P7)
- `crates/cownfs-nfs/` — hand-written RPC/XDR, NFSv4.0 COMPOUND dispatcher,
  state manager, v4.1 sessions, pNFS layouts (P4–P6, P12–P13, P20–P21);
  `cownfs-server` binary
- `crates/cownfs-bench/` — benchmark harness (engine + NFS round trips)
- `crates/cownfs-mkfs/` — `cownfs-mkfs` binary
- `crates/cownfs-fsck/` — `cownfs-fsck` binary
- `crates/cownfs-replicate/` — `cownfs-replicate` binary (P17)
- `crates/cownfs-ds/` — `cownfs-ds` data server daemon (P19)
- `docs/` — [architecture-plan.md](docs/architecture-plan.md) (the full
  design, on-disk format, and phase gates),
  [benchmark.md](docs/benchmark.md) (measured numbers),
  [horizontal-scaling-plan.md](docs/horizontal-scaling-plan.md) (replication
  → sharding → referrals; pNFS deprecated),
  [v40-scaleout.md](docs/v40-scaleout.md) (v4.0-only scale-out architecture),
  [production-readiness.md](docs/production-readiness.md) (operational guide),
  [sharding.md](docs/sharding.md), [p7-soak-results.md](docs/p7-soak-results.md)

## Performance snapshot

Release build, btrfs virtual disk, 2026-09-29 — see
[docs/benchmark.md](docs/benchmark.md) for methodology and full tables.
Headline numbers: ~80 MiB/s sequential write, ~24k file creates/s,
snapshot create+delete ~7µs. A commit (flush + bitmap + fsync +
superblock flip) costs ~24ms on this VM's disk, so every FILE_SYNC NFS
write pays one commit: 4 KiB FILE_SYNC writes run at ~40 IOPS. Reads are
served from the page cache in these runs.

## Principles

- Never overwrite live data in place; crash consistency via pure CoW +
  atomic superblock commit. No journal.
- Checksums on all data and metadata (parent-stored, verified on read).
- Snapshots are cheap root copies; blocks diverge on write.
- NFS is only the transport: the core engine is fully testable without it,
  and every engine test bypasses NFS.
- Safety first: Rust, no `unsafe` in the engine's core paths.
- Interop over purity: when a real client (macOS, Linux) and the spec
  disagree, the client wins and the deviation is documented.
