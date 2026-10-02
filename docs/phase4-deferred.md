# Phase 4 — Clustered MDS: deferred by design

Per the horizontal scaling plan, Phase 4 (clustered metadata server
with Raft/Paxos consensus on the metadata path) is **not planned**.
It is listed for completeness. This document records why, and the
threshold that would reopen it.

## Why deferred

- Phases 1–3 cover the realistic scaling needs:
  - **Phase 1** (read replicas): read-heavy workloads (builds,
    analytics, static assets) scale horizontally with no protocol
    change.
  - **Phase 2** (subtree sharding): metadata *write* scaling by
    partitioning the namespace. Each shard is independent.
  - **Phase 3** (pNFS data path): data *write* throughput scaling;
    `cownfs-ds` is delivered, 4.1 sessions remain.
- A single cownfs MDS handles the metadata rate of one shard. Sharding
  (Phase 2) multiplies that linearly without any consensus.
- Consensus on the metadata path is the hardest distributed-systems
  problem in this design space (leader election, log replication,
  membership change, snapshot transfer). It is a project in itself —
  comparable in scope to Phases 0–3 combined — not an incremental phase.

## What would reopen it

Phase 4 becomes worth considering only when **all** of these hold:

1. A single shard's metadata write rate saturates one MDS instance,
   measured (not projected).
2. The namespace cannot be partitioned further into useful subtrees
   (i.e., Phase 2 sharding is exhausted).
3. The workload needs strong consistency across the whole namespace
   (ruling out the snapshot-consistency replicas of Phase 1).

If that day comes, the starting points are: Raft (etcd/raft or
tikv/raft-rs) for the metadata log, with the CoW B-tree's atomic
root-swing as the natural state-machine apply point — each committed
Raft entry advances the superblock generation. Layout recall (Phase 3)
interacts with leadership changes and needs a fencing design first.

## Status

Deferred. Not started, not scheduled. Revisit when the measurements
above say so.
