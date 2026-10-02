# cownfs Horizontal Scaling Plan

**Status:** Active — Phase 1 in progress
**Author:** Neo (for Heiko Koehler)
**Date:** 2026-10-02

## Goal

Scale cownfs beyond a single node. The long-term target is pNFS-style
separation of metadata and data paths, but we get there in phases ordered
by value-per-protocol-risk. Each phase must be independently useful and
shippable.

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
                    │    Client   │
                    └──────┬──────┘
                           │ NFSv4.0 (today)
              ┌────────────┼────────────┐
              │            │            │
     ┌────────▼───┐ ┌──────▼─────┐ ┌────▼────────┐
     │  Primary   │ │  Replica 1 │ │  Replica 2  │  Phase 1
     │  (r/w)     │ │  (read)    │ │  (read)     │
     └────────────┘ └────────────┘ └────────────┘
              │ snapshot shipping (block diff)

     ┌────────┴───┐ ┌──────┴─────┐ ┌────┴────────┐
     │  Shard A   │ │  Shard B   │ │  Shard C    │  Phase 2
     │  /home     │ │  /data     │ │  /archive   │
     └────────────┘ └────────────┘ └────────────┘

     ┌────────────┐        ┌────────┐ ┌────────┐
     │    MDS     │───────▶│  DS 1  │ │  DS 2  │  Phase 3 (pNFS)
     │ (metadata) │ layout │ (data) │ │ (data) │
     └────────────┘        └────────┘ └────────┘
```

## Phase 1 — Read replicas

**Value:** Immediate read scaling with no protocol change. Read-heavy
workloads (builds, analytics, static assets) mount a replica; writes go
to the primary.

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

## Phase 2 — Subtree sharding

**Value:** Metadata write scaling. Split the namespace across MDS
instances by directory subtree.

- Shard map: `/home -> mds-a`, `/data -> mds-b`, configured statically
  (no dynamic rebalancing in v1).
- Cross-shard RENAME is rejected with `NFS4ERR_XDEV` (same as cross-device
  rename today — clients already handle this).
- Each shard is an independent cownfs image with its own replication
  (Phase 1 applies per-shard).
- Client-visible: mount each shard separately, or a thin referral layer.
- Hard part: none protocol-wise; the work is operational (shard map
  distribution, tooling).

## Phase 3 — pNFS data path

**Value:** Write throughput scaling. Clients write data blocks directly
to data servers; MDS handles metadata only.

**Delivered (Phase 3a):** `cownfs-ds` — the thin data-server daemon.
Dumb 4 KiB block store over TCP (`read_block`/`write_block`/status),
CRC32C checksums computed on write and returned on read (matching
`cownfs_core::checksum`), sparse-file backed. The MDS validates these
checksums at LAYOUTCOMMIT time. Tested: unit (checksum match, store
roundtrip) + TCP integration (write/read/checksum/status).

**Remaining (all large):** Full pNFS requires NFSv4.1 sessions. We are
on 4.0 with no session machinery.

1. **NFSv4.1 sessions.** Needed: session establishment, per-session
   sequence handling, backchannel for layout recalls and device
   notifications.
2. **Layout issuance (file layouts, RFC 5661 §12).** MDS maps file offset
   ranges to `(data_server, block_id)` extents. Client I/O goes direct
   to DS.
3. **Write commit flow.** Client writes new blocks to DS, then calls
   LAYOUTCOMMIT on the MDS. MDS validates block checksums (DS returns
   them), then atomically swings B-tree pointers. Uncommitted blocks are
   orphans — GC reclaims them.
4. **Layout recall.** When GC, snapshot deletion, or defrag moves blocks,
   MDS recalls outstanding layouts via the 4.1 backchannel. Recall races
   are the #1 pNFS bug source — this needs dedicated testing.

**Why CoW still helps here:** the MDS commit is a single atomic
pointer-swing. There is no distributed transaction — the DS writes are
provisional until the MDS blesses them.

## Phase 4 — Clustered MDS

Only if Phases 1–3 prove insufficient. This is distributed consensus
(Raft/Paxos) on the metadata path — a project in itself, not a phase.
Listed here for completeness, not planned.

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
