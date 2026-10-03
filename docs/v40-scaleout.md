# Scale-out with NFSv4.0 only (no pNFS)

Date: 2026-10-02 (updated: referrals implemented)

You don't need v4.1/pNFS to scale out. NFSv4.0 has three mechanisms
that compose into a full scale-out story. All three are implemented.

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

## 3. Referrals: transparent sharding (DONE)

Transparent sharding via RFC 7530 §6.4 `fs_locations` (FATTR4_FS_LOCATIONS).

### How it works

1. The **referral server** holds a directory (e.g., `/data`) that is
   actually owned by another server.
2. When the client does LOOKUP(`/data`), the referral server returns
   `NFS4ERR_MOVED` with the `fs_locations` attribute listing the real
   servers (`shard2.example.com:/data`).
3. The client **transparently** reconnects to the new server and
   retries. The application never sees this.

It's like an HTTP 302 redirect, but for NFS. The client handles it.

### What was implemented

- **FATTR4_FS_LOCATIONS** (attr 32): `fs_location4` list — hostname,
  port, and path for each replica/shard. (`crates/cownfs-nfs/src/nfs4.rs`)
- **NFS4ERR_MOVED** (error 87): Returned from LOOKUP when the
  directory is a referral. (`crates/cownfs-nfs/src/server.rs`)
- **Referral config**: `crates/cownfs-nfs/src/referrals.rs`. Text file
  format: `<inode> <server>[:port] <path>` per line.
- **Server flag**: `cownfs-server --referrals <file>`.
- **Tests**: `p24_referrals` (MOVED + fs_locations encoding).

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
1. Add FATTR4_FS_LOCATIONS constant and encoding. ✓ DONE
2. Add a referral table (config file: path → [(host, port, remote_path)]). ✓ DONE
3. In LOOKUP, if the directory is a referral, return MOVED + locations. ✓ DONE
4. Test with the Linux client (which follows referrals).

This gives you transparent scale-out with **zero client changes** and
**no v4.1**. The pNFS work stays experimental.

Combined with read replicas, you get:
- **Read scale**: Replicas per shard
- **Write scale**: Shards via referrals
- **Capacity scale**: Shards are independent images

That's a complete scale-out story on v4.0 alone.

## Deployment guide

### Single shard with read replicas

```bash
# Primary
cownfs-mkfs /data/primary.img
cownfs-server /data/primary.img 0.0.0.0:2049

# Replica 1 (on another host)
cownfs-replicate receive /data/replica.img 0.0.0.0:2050
cownfs-server --read-only /data/replica.img 0.0.0.0:2049

# Periodic sync (cron every 60s)
cownfs-replicate send /data/primary.img replica-host:2050
```

### Sharded namespace with referrals

```bash
# Shard servers (each independent)
cownfs-mkfs /data/shard0.img && cownfs-server /data/shard0.img 10.0.0.10:2049 &
cownfs-mkfs /data/shard1.img && cownfs-server /data/shard1.img 10.0.0.11:2049 &

# Referral server (namespace skeleton)
cownfs-mkfs /data/referral.img
cownfs-server /data/referral.img 10.0.0.1:2049 &
# Create the shard mountpoints, get their inodes:
#   mkdir /mnt/shard0; mkdir /mnt/shard1  (via NFS or cownfs CLI)
#   <get inodes via stat>

# referrals.conf:
# <shard0-ino> 10.0.0.10 /data
# <shard1-ino> 10.0.0.11 /data

# Restart referral server with config:
cownfs-server --referrals referrals.conf /data/referral.img 10.0.0.1:2049
```

Client mounts only `10.0.0.1:/`. LOOKUP on `/shard0` returns MOVED;
the client follows to `10.0.0.10:/data` transparently.

### Combined: sharded + replicated

Each shard gets its own replicas. The referral targets point to a
load balancer or list multiple replicas in `fs_locations` (client
picks one).

```
Referral server → Shard 0 → Replica 0a, 0b
                → Shard 1 → Replica 1a, 1b
```

## Tooling

- `cownfs-shard`: Static sharding CLI (route, list, init, check).
- `cownfs-replicate`: Block-level replication (send/receive/drive).
- `cownfs-server --referrals`: Referral server mode.
- `cownfs-cluster` (planned): One-command topology setup.
