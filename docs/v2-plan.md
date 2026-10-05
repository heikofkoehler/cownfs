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

| Item | Description | Status |
|------|-------------|--------|
| P4 | (see external plan) | ❌ TODO |
| R6 | (see external plan) | ❌ TODO |
| R7 | (see external plan) | ❌ TODO |
| S3 | (see external plan) | ❌ TODO |
| S4 | (see external plan) | ❌ TODO |
| T3 | (see external plan) | ❌ TODO |
| T4 | (see external plan) | ❌ TODO |
| T5 | (see external plan) | ❌ TODO |
| T7 | (see external plan) | ❌ TODO |

## Phase 3 — Strategic (quarter)

| Item | Description | Status |
|------|-------------|--------|
| S1 | (see external plan) | ❌ TODO |
| P5 | (see external plan) | ❌ TODO |
| S5 | (see external plan) | ❌ TODO |
| S6 | (see external plan) | ❌ TODO |
| P6 | (see external plan) | ❌ TODO |
| Migration tooling | v3→v4 (beyond detection) | ❌ TODO |
| T6 | (see external plan) | ❌ TODO |

## Preserved (not deferred)

R8, R9, P8, S7, S8, T10 — remain in scope, not silently dropped.

## Known issues (not in plan)

- p38/p14: server deadlock under concurrent load (pre-existing, needs investigation).
- 262 passed / 9 failed (2026-10-05); 7 of the 9 are the deadlock/flakiness.
