# C1: Paged Bitmap — Deferred

## Problem
The bitmap is fully in-memory: 100 TiB → 3.35 GiB RAM. This limits
maximum image size on memory-constrained systems.

## Why deferred
A paged bitmap requires:
1. On-demand word loading from disk in `alloc()`/`set()`/`clear()`/`test()`
   (hot paths — would add I/O latency).
2. A page cache with eviction policy.
3. Integration with the delta bitmap (base + delta areas).
4. Crash consistency for dirty pages.

This is a multi-day architectural change. The current in-memory bitmap
is simple, fast, and correct. The 3.35GB limit only affects 100 TiB+
images, which are not a current use case.

## Future direction
- Option A: mmap the bitmap area (OS handles paging). Conflicts with
  delta design; would need rethinking.
- Option B: Two-level bitmap with on-demand page loads. Complex but
  doable.
- Option C: Roaring bitmap compression for sparse allocations.

## Recommendation
Defer until a real 10 TiB+ deployment needs it. The delta bitmap (A1)
already reduced write amplification, which was the urgent problem.
