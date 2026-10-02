# Sharding runbook (Phase 2)

Static subtree sharding splits the namespace across independent cownfs
images, each served by its own `cownfs-server`. There is no dynamic
rebalancing in v1 — the shard map is static.

## Shard map

A text file mapping path prefixes to shards:

```
[home]
prefix = /home
addr = 127.0.0.1:2049
image = /data/shard-home.img

[data]
prefix = /data
addr = 127.0.0.1:2050
image = /data/shard-data.img
```

Routing is longest-prefix match. `/data/archive` beats `/data`.

## Provisioning

```sh
cownfs-shard init shard-map.txt     # format missing shard images (1 GiB)
cownfs-shard list shard-map.txt     # show the map
cownfs-shard route shard-map.txt /home/user   # which shard serves a path
cownfs-shard check shard-map.txt    # TCP-probe every shard
```

## Serving

One server per shard:

```sh
cownfs-server --image /data/shard-home.img --port 2049 &
cownfs-server --image /data/shard-data.img --port 2050 &
```

(Adjust flags to the actual `cownfs-server` CLI.)

Clients mount each shard separately:

```sh
mount -t nfs -o vers=4.0,port=2049 127.0.0.1:/ /mnt/home
mount -t nfs -o vers=4.0,port=2050 127.0.0.1:/ /mnt/data
```

## Semantics

- Cross-shard RENAME is impossible: shards are separate mounts, so the
  client gets EXDEV (the NFSv4 equivalent, NFS4ERR_XDEV) naturally.
- Each shard replicates independently with `cownfs-replicate`
  (Phase 1). Run one `drive` per shard.
- Read replicas (Phase 1a) work per shard: serve a replicated shard
  image with `cownfs-server --read-only`.

## Adding a shard

1. Add a `[name]` section to the map with a non-overlapping prefix.
2. `cownfs-shard init` to format the image.
3. Start a server for it and mount it on clients.
4. Existing data does not move automatically — copy it over with any
   file-level tool, then delete the source.
