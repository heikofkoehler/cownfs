# cownfs

A from-scratch userspace copy-on-write filesystem in the spirit of ZFS/Btrfs,
exported **only** over NFSv4.0. No local mount, no FUSE — one server binary,
mountable from any OS with an NFS client.

Status: design phase. See [docs/architecture-plan.md](docs/architecture-plan.md)
for the full architecture, on-disk format, and phased implementation plan (P0–P7).

## Layout

- `src/block/`  — block device layer, allocator, superblock
- `src/btree/`  — copy-on-write B-tree, refcounts
- `src/engine/` — VFS-like API: inodes, directories, extents, snapshots, transactions
- `src/nfs/`    — hand-rolled RPC/XDR + NFSv4.0 server
- `tools/`      — `mkfs`, `fsck`
- `tests/`      — model tests, crash-injection harness, pynfs runs
- `docs/`       — design documents

## Principles

- Never overwrite live data in place.
- Checksums on all data and metadata (parent-stored, verified on read).
- Crash consistency via pure CoW + atomic superblock commit. No journal.
- Snapshots are cheap root copies; blocks diverge on write.
