# A6/C4: Async I/O and Shard Rebalance — Deferred

## A6: Async I/O — Not Warranted

**Measurement (C3):** 100 concurrent clients × 10 getattrs completed in
386ms. Thread-per-connection handles this load without collapse.

**Trigger for A6:** If 1000+ concurrent clients show p99 latency
degradation, or if thread stack memory (8MB × 1000 = 8GB virtual)
becomes a problem. Not currently observed.

**Recommendation:** Defer until real need. The current model is simple,
correct, and fast enough.

## C4: Shard Rebalance — Deferred (Design Notes)

**Goal:** Online shard split/move for the referral-based sharding
(Phases 1-3 complete per docs/horizontal-scaling-plan.md).

**Design sketch:**
1. Snapshot the shard to move.
2. `cownfs-backup create` the snapshot (full) or use incremental.
3. Restore to the new node.
4. Update the referral table to point to the new node.
5. Delete the source shard (after verifying the new one serves).

**Why deferred:** Requires:
- Online referral table updates (currently static config).
- Verification that the new shard is consistent before cutover.
- Handling of in-flight writes during migration.
- Multi-day distributed systems work.

**Recommendation:** Defer until a real multi-TB deployment needs
rebalancing. The current static sharding works for the use case.
