# Test Gap Analysis

Date: 2026-10-02

## Critical gaps (would have caught the Mac mount bug)

### 1. Non-blocking socket race — FIXED, needs regression test

**Bug:** `serve_concurrent` set listener to non-blocking; accepted sockets
inherited non-blocking mode; `serve_connection` failed on first read with
`WouldBlock`; Mac saw "connection reset by peer."

**Why tests missed it:** Test clients send requests immediately after
connect, so data is already available when server reads (no WouldBlock).
Race condition only manifests with timing delays.

**Fix:** Set accepted sockets back to blocking.

**Gap:** No test for slow/lazy clients. Need a test that connects,
waits, then sends (forcing the server to block on read).

### 2. Throttling integration — NOT TESTED

**Status:** `p28_throttle` tests the token bucket logic in isolation.
No test verifies that an actual NFS op returns `NFS4ERR_DELAY` when
the client exceeds the rate limit.

**Gap:** The `exec()` throttling check could have a bug (wrong opnum,
panic on unknown variant, etc.) and we'd never know.

### 3. Metrics/health endpoints — NOT TESTED

**Status:** The HTTP server on `:port+1000` serves `/metrics` and
`/healthz`. No test verifies:
- The endpoint starts
- `/metrics` returns valid Prometheus format
- `/healthz` returns 200

**Gap:** A typo in the HTTP response formatting would go unnoticed.

## Medium gaps

### 4. Graceful shutdown — NOT TESTED

**Status:** SIGTERM handler stops accepting, drains connections, commits.
No automated test (requires sending signals to a subprocess).

**Gap:** The drain logic, commit-on-shutdown, and 30s timeout are
untested. A bug here means data loss on deploy.

### 5. Lease background renewal — NOT TESTED

**Status:** `p27_lease` tests acquire/renew/release/check. The server's
background renewal thread (spawns on `--node-id`, renews every ttl/3,
exits on loss) is not tested.

**Gap:** If the renewal thread panics or the interval is wrong, the
lease expires and a second primary starts (split-brain).

### 6. Audit log format — NOT TESTED

**Status:** CREATE/REMOVE log JSON with client_addr. No test verifies
the output format or that it goes to stderr.

**Gap:** A format change could break SIEM ingestion.

## Low gaps (nice to have)

### 7. Error code coverage

Some `FsError` variants may not have NFS mapping tests:
- `FsError::Corrupt` → `NFS4ERR_IO` ✓ (tested in p27_conformance)
- `FsError::NoSpace` → `NFS4ERR_NOSPC` ✓ (constant verified)
- Others: need audit

### 8. Large file edge cases

- Files > 1MB (multi-block reads with checksums)
- Sparse files with holes at block boundaries
- Concurrent reads/writes to same file

### 9. Snapshot edge cases

- Checksum verification on snapshot reads ✓ (tested in p28_reliability)
- Snapshot of empty file
- Snapshot restore with corrupted data

## Recommendations

**P0 (fixed):**
1. ✅ Throttling integration test (NFS op → DELAY)
2. ✅ Slow-client regression test (caught the socket bug)
3. ✅ Metrics/health endpoint test

**P1 (fixed):**
4. ✅ Graceful shutdown test (subprocess + SIGTERM)
5. ✅ Lease renewal thread test

**P2 (backlog):**
6. Audit log format test
7. Large file stress tests
8. Snapshot edge cases
