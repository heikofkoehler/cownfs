# cownfs v2 Plan (P/R/S/T items)

Reconstructed 2026-10-05 from git history and work completed.
The authoritative source is the external `prs_plan_v2.md`; this is the
repo-local tracking copy.

## Phase 0 — Repair (data safety)

| Item | Description | Status |
|------|-------------|--------|
| R1 | Per-slot bitmap areas, always checkpoint (fixes delta-crash corruption). v3→v4 format, migration detection. | ✅ Done (9af7e0a, 561b5c4) |
| R2 | Boot verifier changes on restart; UNSTABLE lost, DATA_SYNC4 durable (wire sequence). | ✅ Done (503aac1, ab4cc54) |
| R3 | Two-generation deferred free; persist queue in bitmap padding; fix reopen leaks. | ✅ Done (52da559, b48c084, 4ea9082, 5759e77, 6bed057, fac3c8a) |
| R4 | Stale NodeId returns Corrupt (not panic); lock-poison recovery policy. | ✅ Done (a8c987f, c1a41f4) |
| R5 | Duplicate request cache. | ✅ Done (86c9995) |
| T8a | GitHub Actions PR checks (fmt, build, test). | ⚠️ Blocked: needs OAuth `workflow` scope or Mac push |
| T9 | NFS fault tests. | ✅ Done (479e8e1) |

## Phase 1 — Tactical (weeks)

| Item | Description | Status |
|------|-------------|--------|
| P1 | Commit outside write lock. | ✅ Done (95ffad1) |
| P2 | Grouped COMMIT/FILE_SYNC (prove requests share one durability op via concurrent instrumentation). | ❌ TODO |
| P3 | O(1) free-space counter; dedicated tests for set/clear idempotence, alloc, from_bytes, padding, reopen, statfs. | ⚠️ Partial (39eb627 done; tests TODO) |
| P9 | Dirty-data backpressure. | ✅ Done (ce302d2) |
| T1 | Finer crash enumeration. | ❌ TODO |
| T2 | Expanded model testing. | ❌ TODO |

## Phase 2 — Strategic (month)

Items map to workstreams in `docs/performance-resiliency-scale-plan.md`
(P←A Performance, R←B Resiliency, S←C Scale, T←D Testing).

| Item | Description | Status |
|------|-------------|--------|
| P4 | Workstream A4/A5: vectored/readahead reads, B-tree node cache (see `performance-resiliency-scale-plan.md` §A4, §A5) | ❌ TODO |
| R6 | Workstream B6: quota/xattr crash consistency (see §B6) | ❌ TODO |
| R7 | Workstream B3/B4: fault-injection expansion, lease failover (see §B3, §B4) | ❌ TODO |
| S3 | Workstream C3: connection-scale measurement (see §C3) | ❌ TODO |
| S4 | Workstream C4: shard rebalance (see §C4) | ❌ TODO |
| T3 | Workstream D3: fault-injection harness (see §D3) | ❌ TODO |
| T4 | Workstream D4: performance regression suite (see §D4) | ❌ TODO |
| T5 | Workstream D1: close known gaps (see §D1) | ❌ TODO |
| T7 | (see external `prs_plan_v2.md` for T7 specifics) | ❌ TODO |

## Phase 3 — Strategic (quarter)

| Item | Description | Status |
|------|-------------|--------|
| S1 | Workstream C1: bitmap memory / paged bitmap (see `performance-resiliency-scale-plan.md` §C1; also `docs/c1-paged-bitmap-deferred.md`) | ❌ TODO |
| P5 | Workstream A6: async I/O server (see §A6; also `docs/a6-c4-deferred.md`) | ❌ TODO |
| S5 | (see external `prs_plan_v2.md` for S5 specifics) | ❌ TODO |
| S6 | (see external `prs_plan_v2.md` for S6 specifics) | ❌ TODO |
| P6 | (see external `prs_plan_v2.md` for P6 specifics) | ❌ TODO |
| Migration tooling | v3→v4 automatic (beyond detection) | ❌ TODO |
| T6 | (see external `prs_plan_v2.md` for T6 specifics) | ❌ TODO |

## Preserved (not deferred)

R8, R9, P8, S7, S8, T10 — remain in scope, not silently dropped.

## Known issues (not in plan)

- p38/p14: server deadlock under concurrent load (pre-existing, needs investigation).
- 262 passed / 9 failed (2026-10-05); 7 of the 9 are the deadlock/flakiness.
