# Production Readiness Assessment

Date: 2026-10-02 (updated: P0+P1+P2 complete)
Status: **Minimal production readiness achieved** (Kerberos deferred)

## What's solid

- **On-disk format**: CoW B-tree, ping-pong superblock, CRC32C on
  metadata AND data blocks (parent-stored in extents). Crash during
  write leaves old generation intact.
- **Replication**: Crash-safe ordering (superblock last), UUID validation.
- **Backup/restore**: `cownfs-backup` with checksummed portable format.
- **Fencing**: Leader lease in superblock prevents split-brain.
- **Protocol**: 166 Rust tests, 7 pynfs tests. v4.0 core ops work.
- **Scale-out**: Replicas + referrals on v4.0.
- **Observability**: Prometheus metrics, /healthz, JSON logs, audit log.
- **Reliability**: Graceful shutdown, connection limits, idle timeouts.

## Completed (this session)

### P0: Data safety ✅

1. **Data block checksums** — CRC32C stored in extent (parent-stored).
   Verified on every read. Corruption → `NFS4ERR_IO`. Backward
   compatible (old extents with cksum=0 skip verification).

2. **Backup/restore** — `cownfs-backup create/restore/verify/list`.
   Portable file format with per-block checksums. Restore verifies
   UUID and runs fsck.

3. **Leader lease** — Superblock stores `lease_holder` + `lease_expiry`.
   `cownfs-server --node-id` acquires on startup, refuses if held.
   Background renewal; fences (exits) if lost. Expiry allows takeover
   after crash.

### P1: Operability ✅ (Kerberos deferred)

1. **Metrics** — Per-op counters, errors, latency histograms.
   Prometheus text format at `:port+1000/metrics`.

2. **Health** — `:port+1000/healthz` returns `ok`.

3. **Structured logging** — JSON to stderr, `--log-level`
   (error/warn/info/debug).

4. **Audit log** — Mutating ops (CREATE, REMOVE, etc.) with client
   address, timestamp. JSON format.

### P2: Reliability ✅

1. **Graceful shutdown** — SIGTERM/SIGINT: stop accepting, drain
   connections (30s timeout), commit, exit.

2. **Connection limits** — 1000 max concurrent, refuse beyond.

3. **Idle timeouts** — 300s read timeout closes hung clients.

4. **Read optimization** — Single lock acquisition across entire
   read (was per-block).

## Remaining gaps

### P1: Security (deferred by Heiko)

1. **AUTH_SYS only.** UIDs are client-asserted. Suitable for trusted
   internal networks only. For untrusted: Kerberos (3-4 weeks) or
   stunnel + IP ACLs (days).

2. **No encryption.** Cleartext on the wire. Use stunnel or WireGuard
   for transport security.

### P2: Performance (partial)

1. **No benchmarks vs. baseline.** Unknown if 2x or 20x slower than
   ext4/tmpfs. Need: fio comparison.

2. **No readahead.** Sequential reads are per-RPC. Client-side caching
   mitigates, but server-side prefetch would help.

### P3: Features (not started)

1. **No quotas.** One user can fill the filesystem.
2. **No ACLs.** Only POSIX mode bits.
3. **No extended attributes** (user xattrs).
4. **No NFSv4 delegations** (client caching).

## Test coverage

- **166 Rust tests** across 22 test binaries.
- **7 pynfs** v4.0 conformance tests.
- Coverage: core ops, errors, attrs, I/O, state, concurrency,
  replication, referrals, backup, lease, checksums.

## Recommendation

**Ready for trusted internal deployment** with the following
operational requirements:

1. Run behind a firewall (AUTH_SYS is spoofable).
2. Use stunnel/WireGuard for encryption if crossing untrusted networks.
3. Monitor `/metrics` and `/healthz`.
4. Set up `cownfs-backup` cron (daily full, hourly incremental via
   replication).
5. Use `--node-id` with a unique ID per primary; set `--lease-ttl`
   to 30s (default).

**Not ready for:**
- Untrusted clients (needs Kerberos)
- Multi-tenant with quotas (needs P3)
- Single-file high-bandwidth (needs pNFS, deprecated)

The architecture is sound. Remaining work is hardening for
specific deployment scenarios, not redesign.
