# Production Readiness Plan

Date: 2026-10-03. Every item below was verified against the code at
`main` (b84b582). Status marks: ✅ verified present, ❌ verified
missing, ⚠️ partial. Supersedes `docs/production-readiness.md`
(2026-10-02), which contained stale claims — see "Corrections" at the
end.

## Verified solid (no work needed)

- ✅ CoW B-tree, ping-pong superblock, CRC32C on data blocks
  (parent-stored in extents, verified on every read → `NFS4ERR_IO`).
- ✅ Crash-safe commit ordering (superblock last); drop-without-commit
  recovers to last committed generation (`p49_chaos`, 2 tests).
- ✅ Leader lease in superblock (`lease_holder`/`lease_expiry`,
  `superblock.rs:62-64`); `--node-id` fencing.
- ✅ `cownfs-backup` full create/restore/verify/list, per-block CRC32C,
  UUID check + `Fs::check()` on restore; incremental `create-inc`
  exports only changed blocks (`diff_roots`, 5 blocks vs full image).
- ✅ Prometheus `/metrics`, `/healthz` (`server.rs:2318-2320`).
- ✅ JSON structured logging + JSON audit log (`log.rs:49-60`).
- ✅ Graceful shutdown on SIGTERM/SIGINT (`server.rs:2339-2351`).
- ✅ Connection cap 1000 (`server.rs:2375`); per-client/per-file token
  buckets (`throttle.rs`).
- ✅ 100 concurrent clients × 10 getattrs in 386ms (`p51_conn_scale`);
  thread-per-connection does not collapse at this scale.
- ✅ Quotas: `set_quota`/`quota_usage`/`QuotaExceeded` in engine,
  exposed via server CLI; edge cases tested (`p44_quota_edge`).
- ✅ User xattrs via `.xattrs` file; hidden from NFS clients
  (`p43_xattr_hide`).
- ✅ Snapshot scheduler (telescoping tiers, `snapshot_sched.rs`).
- ✅ 244 tests, 0 failures; `cargo fmt --check` clean.

## P0 — data safety (blocks any prod claim)

1. **B2 remainder: full bitmap block CRCs + generation fallback.**
   ❌ Only delta entries have CRC32C (`p42_delta_crc`); full bitmap
   blocks have none (`bitmap.rs` has zero crc references). On a
   corrupt delta we fall back to the base bitmap, but there is no
   fallback to the *older superblock generation* when the newer one
   is corrupt. Work: per-bitmap-block CRCs, and on corrupt newer
   generation, mount the older one (both slots already exist).
   Test: corrupt newer delta + newer bitmap block, assert
   `Fs::open` recovers older generation and `Fs::check()` passes.

2. **Harden fault-injection assertions.** ⚠️ Harness exists
   (`p48_fault_inject`: torn/bit-flip/reorder). The torn-write test
   now asserts checksum failure, but bit-flip and reorder paths
   need the same treatment: every injected corruption must either
   recover to a valid prior generation or return a specific
   corruption error — no "may or may not" assertions.

3. **D5: real SIGKILL chaos.** ❌ `p49_chaos.rs` only drops `Fs`
   (simulated). Work: spawn `cownfs-server` as a subprocess, run
   NFS traffic, `SIGKILL` mid-flight, restart, assert recovery to
   last committed state + `cownfs-fsck` clean.

4. **p38_stress defects.** ❌ Line 139: survivor formula uses
   `it.div_ceil(2)` but the comment says odd `i` survive, which is
   `it / 2` for odd `it` (e.g. 5 iters → 2 survivors, not 3). Work:
   fix formula, add an odd-iteration run, fix the header's xattr
   claim (it doesn't test xattrs).

5. **Incremental restore path.** ⚠️ `create-inc` exists; there is no
   `restore-inc` — restoring an incremental onto its base is
   undocumented/manual. Work: `restore-inc <base-image> <inc-file>
   <new-image>` (copy base, apply changed blocks, point superblock
   at new roots), plus an end-to-end test: full + 3 incrementals,
   restore each stage, compare contents, run `cownfs-fsck`.

## P1 — correctness / operability

6. **C2: plumb paged readdir through NFS.** ❌ `Fs::readdir_paged`
   and `BTree::range_limit` exist but `op_readdir` still calls full
   `Fs::readdir()` (zero references to `readdir_paged` in
   `crates/cownfs-nfs/src/`). Work: cursor-based `op_readdir`,
   wire-level test with a 100k-entry directory.

7. **Audit UID extraction.** ❌ `server.rs:588-603` hardcodes uid
   `0` with `// TODO: extract UID from RPC credentials`. Work:
   parse AUTH_SYS uid/gid from the RPC credential and pass through.

8. **D4: real perf gate.** ❌ `scripts/perf-gate.sh:36` is
   `TODO: implement baseline comparison` — it prints metrics but
   never fails. Work: compare against `docs/perf-baseline.txt`,
   fail on >20% regression on any metric.

9. **D2: soak + fio.** ⚠️ 10k-op soak passes (11s); the multi-hour
   100M soak never ran and fio was never installed. Work: run both,
   record in `docs/p7-soak-results.md`, close GH issue #3.

10. **A4: readahead.** ⚠️ Vectored reads done (contiguous extents,
    one syscall); readahead not implemented and no `strace -c`
    syscall-count comparison recorded. Work: sequential prefetch,
    before/after syscall counts.

## P2 — scale

11. **C1: paged bitmap.** ❌ Bitmap fully resident (`docs/
    c1-paged-bitmap-deferred.md`). Memory = blocks/8 bytes → 32 MiB
    per TiB. Fine now; becomes a P0 past ~100 TiB images.

12. **A5: LRU hot-path cost + eviction test.** ⚠️ 10k-node/arena
    LRU exists, but hit maintenance is O(cache size) (linear
    `VecDeque` search) and no test forces >10k clean nodes and
    verifies reread correctness. Work: indexed LRU (HashMap +
    intrusive list or `lru` crate), eviction test.

## P3 — security / features (deployment-dependent)

13. **AUTH_SYS only.** ❌ No Kerberos/GSSAPI anywhere in the tree.
    UIDs are client-asserted. Decision needed: Kerberos (3-4 weeks)
    or document trusted-network-only deployment.
14. **No transport encryption.** ❌ No TLS in tree. Workaround:
    stunnel/WireGuard — document as required for untrusted networks.
15. **No ACLs; no delegations** (only `OPEN_DELEGATE_NONE`,
    `server.rs:1633`). NFSv4.1 support is prototype-grade
    (`docs/nfsv41-compliance.md`: "not interoperable with real v4.1
    clients"). Not required for v4.0-only deployment.
16. **Quota NFS end-to-end.** ⚠️ Engine + CLI exist; verify
    enforcement through the NFS wire path (no wire test found).

## Explicit non-goals

- Async I/O: C3 measurement shows no need (deferred, `docs/a6-c4-deferred.md`).
- Online shard rebalance: design sketched, deferred until a real
  multi-TB deployment needs it.
- A3 full "refcount-1 unpinned" in-place overwrite: current-txg-only
  scope is the crash-safe subset; the rest needs a log.

## Corrections to the 2026-10-02 assessment

- "No quotas" / "No extended attributes" — wrong; both implemented.
- "7 pynfs tests" — no pynfs tests exist in the repo; unverifiable.
- "166 Rust tests" — now 244.
- "Minimal production readiness achieved" — premature; P0 items
  above (especially 1, 3, 5) block that claim.

## Check results (2026-10-03)

- `cargo fmt --check`: clean (fixed 10 files).
- `cargo test --workspace`: 244 passed, 0 failed (pre-plan-run).
- `cargo build --workspace --all-targets`: warnings present in
  earlier runs (e.g. unused `TxgCoord::clear_error`); make
  warning-free part of P0 item cleanup.
