# Scale-out with NFSv4.0 only (no pNFS)

Date: 2026-10-02

You don't need v4.1/pNFS to scale out. NFSv4.0 has three mechanisms
that compose into a full scale-out story. We have two of three.

## 1. Read replicas (DONE)

`cownfs-replicate send/receive` copies block-level diffs to a replica.
The replica serves with `--read-only`.

- Scales **reads** linearly: N replicas = N× read throughput.
- Replication is async (eventual consistency). Fine for read-heavy
  workloads, caches, static assets.
- Crash-safe as of the superblock-last ordering fix.

**Use when:** Read-heavy, tolerance for replication lag.

## 2. Subtree sharding (DONE, manual)

`cownfs-shard` does static longest-prefix routing: `/shard0/...` goes
to server A, `/shard1/...` to server B. Each shard is an independent
cownfs image.

- Scales **writes** and **capacity**: each shard is independent.
- Today it's manual: the client mounts each shard separately
  (`mount serverA:/shard0 /mnt/a`, etc.).
- Cross-shard rename = EXDEV (expected, like separate filesystems).

**Use when:** You can partition the namespace by top-level directory
and don't mind multiple mounts.

## 3. Referrals: transparent sharding (NOT DONE)

This is the missing piece that makes sharding transparent with pure
v4.0. RFC 7530 §6.4 and the `fs_locations` attribute (FATTR4_FS_LOCATIONS).

### How it works

1. The **referral server** holds a directory (e.g., `/data`) that is
   actually owned by another server.
2. When the client does LOOKUP(`/data`), the referral server returns
   `NFS4ERR_MOVED` with the `fs_locations` attribute listing the real
   servers (`shard2.example.com:/data`).
3. The client **transparently** reconnects to the new server and
   retries. The application never sees this.

It's like an HTTP 302 redirect, but for NFS. The client handles it.

### What we'd need to implement

- **FATTR4_FS_LOCATIONS** (attr 32): `fs_location4` list — hostname,
  port, and path for each replica/shard.
- **NFS4ERR_MOVED** (error 87): Return this from LOOKUP when the
  directory is a referral.
- **Referral config**: A table mapping directory inodes → list of
  (host, path). Could be a simple config file or a special xattr.

### Architecture with referrals

```
                    ┌─────────────┐
                    │  Referral   │
                    │  Server     │  (holds namespace skeleton)
                    │  (v4.0)     │
                    └──────┬──────┘
                           │ LOOKUP /data → MOVED + fs_locations
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌─────────┐  ┌─────────┐  ┌─────────┐
        │ Shard 0 │  │ Shard 1 │  │ Shard 2 │
        │ /a/...  │  │ /b/...  │  │ /c/...  │
        └─────────┘  └─────────┘  └─────────┘
```

The client mounts only the referral server. All shard navigation
is transparent. This is how NetApp, Isilon, etc. did scale-out before
pNFS existed.

### vs. pNFS

| | v4.0 Referrals | v4.1 pNFS |
|---|---|---|
| Granularity | Directory subtree | File byte-range |
| Client support | All v4.0 clients | Needs pNFS client |
| Implementation | fs_locations + MOVED | Layouts + DS + backchannel |
| Write scaling | Yes (per-shard) | Yes (striped) |
| Single-file bandwidth | No (one server) | Yes (parallel DS) |

Referrals don't stripe a single file across servers (that's what
pNFS layouts do). But for most workloads — home dirs, project dirs,
per-user shards — directory-granularity sharding is sufficient.

## Recommendation

**Implement referrals (mechanism 3).** It's ~200 lines:
1. Add FATTR4_FS_LOCATIONS constant and encoding.
2. Add a referral table (config file: path → [(host, port, remote_path)]).
3. In LOOKUP, if the directory is a referral, return MOVED + locations.
4. Test with the Linux client (which follows referrals).

This gives you transparent scale-out with **zero client changes** and
**no v4.1**. The pNFS work stays experimental.

Combined with read replicas, you get:
- **Read scale**: Replicas per shard
- **Write scale**: Shards via referrals
- **Capacity scale**: Shards are independent images

That's a complete scale-out story on v4.0 alone.
