# cownfs

A from-scratch userspace copy-on-write filesystem in the spirit of ZFS/Btrfs,
written in Rust, exported **only** over NFSv4.0. No local mount, no FUSE — one
server binary, mountable from any OS with an NFS client.

## Status

**Working prototype, not production.** All seven implementation phases (P0–P7)
are done: the engine formats, mounts (over NFS), mutates, snapshots, and
survives fault injection. 56 tests pass, including a 20k-op shadow-model stress
test and an NFSv4 client-driven mutation suite. Benchmarks live in
[docs/benchmark.md](docs/benchmark.md).

What works:

- **P0** — 4 KiB block device, CRC32C checksums, free-space bitmap,
  ping-pong superblock with generation commit
- **P1** — in-memory generational CoW B-tree (model-tested against `BTreeMap`
  over 135k randomized ops)
- **P2** — block-backed CoW engine: files, dirs, symlinks, hard links,
  extents, refcounted blocks
- **P3** — snapshots: cheap shared-root copies with CoW divergence and
  snapshot-pinned data blocks
- **P4** — hand-rolled RPC/XDR and a read-only NFSv4.0 server
- **P5** — NFSv4 mutation path (OPEN/CREATE/WRITE/COMMIT/REMOVE/RENAME/…)
  with a userspace test client
- **P6** — NFSv4 state management (clientids, opens, locks, seqids)
- **P7** — fault injection, unreachable-block reclaim (`fsck --reclaim`),
  soak test, malformed-XDR handling

Known gaps (tracked as [GitHub issues](https://github.com/heikofkoehler/cownfs/issues)):

- No full RFC 7530 conformance audit yet — `pynfs` is the target standard
  but its write/state suites haven't been run
- The server handles one TCP connection at a time; no tested concurrent
  multi-client behavior (share/lock/replay across connections)
- No large-scale validation yet: kernel-tree untar + content/metadata
  comparison through a real NFS client is still pending
- Fault injection doesn't yet cover torn/reordered device writes; no
  multi-hour soak has been run
- By design, v1 has no delegations, Kerberos, NFSv4.1 sessions, or pNFS

## Quick start

```sh
cargo build --release
./target/release/cownfs-mkfs --size 1G /tmp/cow.img
./target/release/cownfs-server /tmp/cow.img 127.0.0.1:2049 &
sudo mount -t nfs -o vers=4.0,port=2049 127.0.0.1:/ /mnt/cow
# ... use it ...
./target/release/cownfs-fsck /tmp/cow.img            # check
./target/release/cownfs-fsck --reclaim /tmp/cow.img  # check + reclaim
```

Tests and benchmarks:

```sh
cargo test --workspace                       # 56 tests
cargo run --release -p cownfs-bench          # throughput/latency numbers
COWNFS_STRESS_ITERS=50000 cargo test -p cownfs-core --test stress
```

## Layout

- `crates/cownfs-core/` — block device, superblock, bitmap, checksums (P0);
  CoW B-trees and the filesystem engine (P1–P3, P7)
- `crates/cownfs-nfs/` — hand-written RPC/XDR, NFSv4.0 COMPOUND dispatcher
  and state manager (P4–P6); `cownfs-server` binary
- `crates/cownfs-bench/` — benchmark harness (engine + NFS round trips)
- `crates/cownfs-mkfs/` — `cownfs-mkfs` binary
- `crates/cownfs-fsck/` — `cownfs-fsck` binary
- `docs/` — [architecture-plan.md](docs/architecture-plan.md) (the full
  design, on-disk format, and phase gates) and
  [benchmark.md](docs/benchmark.md) (measured numbers)

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
