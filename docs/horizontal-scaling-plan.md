# cownfs Horizontal Scaling Plan

**Status:** Active — Phases 1-3 complete (v4.0 only). Phase 4 (pNFS) DEPRECATED.
**Author:** Neo (for Heiko Koehler)
**Date:** 2026-10-02 (updated: pNFS deprecated, referrals complete)

## Goal

Scale cownfs beyond a single node using **NFSv4.0 only**. No pNFS required.

## Why CoW helps

Copy-on-write gives us three properties that make horizontal scaling
easier than in a traditional filesystem:

1. **Writes never modify in place.** A crashed writer leaves orphaned
   blocks, never corrupt metadata. Replicas and data servers can accept
   writes without distributed transactions on the data path.
2. **Snapshots are free.** A snapshot is just a pinned B-tree root.
   Shipping a snapshot to a replica means shipping the blocks reachable
   from that root that the replica doesn't already have.
3. **Incremental send is a tree diff.** Walking two roots (old and new)
   and comparing block ids yields exactly the changed blocks. No journal
   replay, no write-ahead log shipping.

## Architecture overview

```
                    ┌─────────────┐
                    │    Client   │  (mounts referral server only)
                    └──────┬──────┘
                           │ NFSv4.0
                    ┌──────▼──────┐
                    │  Referral   │
                    │  Server     │  (namespace skeleton)
                    └──────┬──────┘
                           │ LOOKUP → MOVED + fs_locations
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌─────────┐  ┌─────────┐  ┌─────────┐
        │ Shard 0 │  │ Shard 1 │  │ Shard 2 │  Phase 2+3
        │ primary │  │ primary │  │ primary │
        └───┬─────┘  └───┬─────┘  └───┬─────┘
            │            │            │  Phase 1 (per-shard)
     ┌──────▼──┐  ┌──────▼──┐  ┌──────▼──┐
     │Replica  │  │Replica  │  │Replica  │
     │(read)   │  │(read)   │  │(read)   │
     └─────────┘  └─────────┘  └─────────┘
```

## Phase 1 — Read replicas ✓ DONE

**Value:** Immediate read scaling with no protocol change.

**Consistency model:** Replica serves reads at a snapshot boundary.
Reads are always internally consistent (a single root), but may lag the
primary by the replication interval. No read-your-write guarantee on
replicas. This is documented, not hidden.

### 1a. Read-only server mode

- Add `--read-only` flag to `cownfs-server`.
- In read-only mode, mutating ops (CREATE, REMOVE, RENAME, WRITE,
  SETATTR with size/mode changes, LINK) return `NFS4ERR_ROFS`.
- Non-mutating ops (LOOKUP, READ, READDIR, GETATTR, ACCESS) work normally.
- `fs_read_only` is advertised via the appropriate filesystem attribute
  so clients know upfront.
- Regression test: p16 — verify every mutating op gets ROFS, reads work.

### 1b. Snapshot diff (incremental block send)

- New `cownfs-core` API: `diff_roots(old_root: BlockId, new_root: BlockId)
  -> Vec<BlockId>` — walk both B-trees in lockstep, emit blocks present
  in new but not old (or with different content ids).
- Because of CoW, any block reachable from the new root but not the old
  root is exactly the write set. Unchanged subtrees share block ids and
  are skipped without descent.
- Unit tests: empty diff on identical roots, full diff on fresh roots,
  incremental diff after N writes, diff correctness vs. brute-force
  reachable-set comparison (model test).

### 1c. Replication transport

- New binary `cownfs-replicate`: connects to primary, requests "send
  snapshot S", receives `(block_id, block_data)` pairs, writes them into
  the replica's local image, then atomically swings the replica's
  superblock to the new root.
- Wire protocol: minimal TCP framing (not NFS — this is block-level).
  `HELLO -> SNAPSHOT <root> -> BLOCK* -> COMMIT -> ACK`.
- Each block carries its checksum; replica verifies before writing.
  Corrupt block aborts the replication, replica keeps serving the old
  snapshot.
- Replica never serves a partially-applied snapshot: the superblock
  swing is the commit point (ping-pong superblock already gives us this).

### 1d. Replication driver

- Simplest viable: `cownfs-replicate` run on a cron/loop, or a
  `--replicate-to <addr>` flag on the primary that pushes after every N
  commits or T seconds.
- Track the last-replicated root per replica so each run is incremental.
- Metrics: blocks sent, bytes sent, lag behind primary (in commits and
  wall-clock).

### 1e. Testing

- p16 integration test: primary + replica, write on primary, replicate,
  read on replica, verify contents match. Then write more, replicate
  again, verify incremental (block counts).
- Fault test: kill replication mid-send, verify replica still serves the
  old snapshot consistently.
- Read-only test: every mutating op on replica returns ROFS.

### Phase 1 exit criteria

- [ ] `--read-only` server mode with ROFS on all mutating ops
- [ ] `diff_roots` with model-tested correctness
- [ ] `cownfs-replicate` binary doing incremental snapshot shipping
- [ ] p16 integration tests green
- [ ] Docs: replica setup, consistency model, lag metrics

## Phase 2 — Subtree sharding ✓ DONE

**Value:** Metadata write scaling. Split the namespace across servers
by directory subtree. `cownfs-shard` CLI + `cownfs-cluster init` for
automated setup.

- Shard map: `/home -> mds-a`, `/data -> mds-b`, configured statically
  (no dynamic rebalancing in v1).
- Cross-shard RENAME is rejected with `NFS4ERR_XDEV` (same as cross-device
  rename today — clients already handle this).
- Each shard is an independent cownfs image with its own replication
  (Phase 1 applies per-shard).
- Client-visible: mount each shard separately, or a thin referral layer.
- Hard part: none protocol-wise; the work is operational (shard map
  distribution, tooling).

## Phase 3 — Referrals (transparent sharding) ✓ DONE

**Value:** Makes sharding transparent. Client mounts one server; LOOKUP
on shard dirs returns MOVED + fs_locations, client follows automatically.

Implements RFC 7530 §6.4: FATTR4_FS_LOCATIONS, NFS4ERR_MOVED, referral
config file, `cownfs-server --referrals`.

## Phase 4 — pNFS data path — DEPRECATED

**Why deprecated:** With referrals (Phase 3) and read replicas (Phase 1),
we have transparent write sharding and read scaling on NFSv4.0 alone.
pNFS would add single-file parallel I/O (striping), which is not a
requirement.

The pNFS prototype code (`cownfs-ds`, layout ops) remains in the tree
as experimental, but it is not on the scaling path. See
`docs/nfsv41-compliance.md` for the gap analysis.

**If parallel I/O ever becomes important**, the pNFS work can be
revived. Until then, v4.0 referrals are the scale-out mechanism.

## Open questions

1. Replica lag target: is 30s acceptable, or do we need sub-second?
   (Drives push-vs-pull design in 1d.)
2. Should replicas serve NFS directly, or sit behind a load balancer?
3. For Phase 3: implement 4.1 sessions from scratch, or find an existing
   Rust 4.1 scaffold?
4. Block size: 4KiB is fine for metadata, but data servers may want
   larger blocks for throughput. Variable block sizes complicate the
   B-tree — defer.

## Non-goals

- Kerberos / RPCSEC_GSS (still AUTH_SYS everywhere).
- Dynamic rebalancing of shards or data blocks.
- Multi-writer to the same file across replicas (primary is the single
  writer — no conflict resolution).
