# Production Readiness Assessment

Date: 2026-10-02
Status: **Prototype — not production-ready**

## What's solid

- **On-disk format**: CoW B-tree, ping-pong superblock, CRC32C on
  metadata. Crash during write leaves old generation intact.
- **Replication**: Crash-safe ordering (superblock last), UUID validation.
- **Protocol**: 161 Rust tests, 7 pynfs tests. v4.0 core ops work.
- **Scale-out**: Replicas + referrals on v4.0.

## What's missing for production

### P0: Data safety (must fix before any production use)

1. **No data block checksums.** Metadata has CRCs, but file data blocks
   do not. Bit rot in data is undetectable. Need: checksum per data
   block, verified on read, scrubbed offline.

2. **fsck is offline-only.** No online verification. A corrupted image
   requires downtime. Need: online scrub + incremental verification.

3. **No backup story.** Snapshots exist but there's no `cownfs backup`
   to external storage. Need: snapshot export to S3/file, restore.

4. **Single writer.** No fencing. If two primaries write the same image
   (split-brain), data loss. Need: leader election or STONITH.

### P1: Security (must fix before untrusted clients)

1. **AUTH_SYS only.** UIDs are client-asserted. Any client can spoof any
   UID/GID. Need: Kerberos (RPCSEC_GSS) or at minimum IP-based ACLs.

2. **No encryption.** Data and metadata go over the wire in cleartext.
   Need: TLS wrapper or Kerberos privacy.

3. **No audit logging.** Who deleted that file? Unknown. Need: operation
   log with client identity.

### P1: Operability (must fix before on-call)

1. **No metrics.** No Prometheus endpoint, no operation counters, no
   latency histograms. Blind in production.

2. **No health checks.** Load balancer can't tell if the server is
   healthy. Need: `/healthz` or a NOOP probe.

3. **Logging is stderr only.** No structured logs, no rotation, no
   levels. Need: JSON logs, configurable verbosity.

4. **No config management.** Flags only, no config file. Need: TOML
   config for referrals, replicas, tuning.

### P2: Reliability

1. **No graceful shutdown.** SIGTERM kills mid-operation. Need: drain
   connections, commit, then exit.

2. **Memory unbounded.** No cache limits, no backpressure. A large
   READDIR can OOM. Need: bounded caches, request limits.

3. **No request timeouts.** A hung client holds resources forever.
   Need: idle timeout, op timeout.

### P2: Performance

1. **No benchmarks vs. baseline.** We have microbenchmarks, but no
   comparison to ext4/tmpfs/NFS-Ganesha. Unknown if we're 2x or 20x
   slower.

2. **Single-threaded per connection.** The `serve_concurrent` spawns a
   thread per connection, but each connection is single-threaded.
   Pipelined requests block.

3. **No readahead/prefetch.** Sequential reads do one RPC per block.

### P3: Features

1. **No quotas.** One user can fill the filesystem.
2. **No ACLs.** Only POSIX mode bits.
3. **No extended attributes** (user xattrs).
4. **No NFSv4 delegations** (client caching).

## Effort estimate

| Priority | Work | Estimate |
|----------|------|----------|
| P0 | Data checksums + scrub | 1 week |
| P0 | Backup/restore | 1 week |
| P0 | Fencing/leader election | 2 weeks |
| P1 | Kerberos | 3-4 weeks (or use stunnel) |
| P1 | Metrics + health + logging | 1 week |
| P1 | Audit log | 3 days |
| P2 | Graceful shutdown, limits, timeouts | 1 week |
| P2 | Perf tuning + benchmarks | 2 weeks |
| P3 | Quotas, ACLs, xattrs | 2-3 weeks |

**Total: ~3 months for minimal production readiness** (P0+P1).
Full production (all P2+P3): 5-6 months.

## Recommendation

**Do not run in production yet.** The P0 data safety issues are
blockers:

1. Add data block checksums first (1 week). Without this, you're
   flying blind on bit rot.
2. Build backup/restore (1 week). You need a recovery story before
   you need the filesystem.
3. Then decide on the security model. If it's a trusted internal
   network, IP ACLs + stunnel may suffice (days, not weeks). If
   untrusted, Kerberos is the long pole.

The good news: the architecture (CoW, replication, referrals) is
sound. The work is in hardening, not redesign.
