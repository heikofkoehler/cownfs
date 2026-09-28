# cownfs

A from-scratch userspace copy-on-write filesystem in the spirit of ZFS/Btrfs,
exported **only** over NFSv4.0. No local mount, no FUSE — one server binary,
mountable from any OS with an NFS client.

Status: design phase. See [docs/architecture-plan.md](docs/architecture-plan.md)
for the full architecture, on-disk format, and phased implementation plan (P0–P7).

## Layout (Rust workspace)

- `crates/cownfs-core/` — block device, superblock, bitmap, checksums (P0);
  CoW B-tree and engine (P1–P3)
- `crates/cownfs-mkfs/` — `mkfs` binary
- `crates/cownfs-fsck/` — `fsck` binary
- `crates/cownfs-nfs/` — hand-rolled RPC/XDR + NFSv4.0 server (P4+)
- `docs/` — design documents

## Principles

- Never overwrite live data in place.
- Checksums on all data and metadata (parent-stored, verified on read).
- Crash consistency via pure CoW + atomic superblock commit. No journal.
- Snapshots are cheap root copies; blocks diverge on write.
