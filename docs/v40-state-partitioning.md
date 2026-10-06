# NFSv4.0 State Partitioning for 100M+ Clients

**Status:** Design — not yet implemented.
**Date:** 2026-10-06.
**Scope:** How `cownfs-server`'s NFSv4.0 client/open/lock state scales from
~1K clients per server to a 100M-client fleet. Companion to
`docs/horizontal-scaling-plan.md` (data path) — this doc is the state path.

## 1. Current state model (as of `main`)

`crates/cownfs-nfs/src/state.rs`: a single `StateManager` behind one
`Arc<Mutex<…>>` per server process, all in memory:

- `clients: HashMap<u64, ClientRecord>` — verifier, name, confirmed flag,
  lease expiry (90s lease).
- `opens: HashMap<(clientid, owner), OpenRecord>` — file ino, stateid,
  share access/deny, seqid.
- `locks: HashMap<(clientid, owner), LockRecord>` — file ino, byte range,
  lock type, seqid.
- `clientid` / stateid counter: local monotonic `u64`s, restart at 1.
- Lease expiry reaps all of a client's state. No persistence, no grace
  period, no reclaim handling — a restart loses everything and clients
  get `STALE_CLIENTID`.

This is correct for a single server and a few thousand clients. It fails
at fleet scale in four ways (§2).

## 2. Why it doesn't scale

**Memory.** Roughly 100 B per client + 200 B per open + 200 B per lock.
1M clients × 10 opens ≈ 2 GB per server — survivable. 10M × 50 ≈ 100 GB —
not. Memory grows with clients × state, unbounded per server.

**Lock contention.** Every stateful op (OPEN, CLOSE, LOCK, LOCKU, RENEW,
SETCLIENTID) takes the single global state mutex. At tens of thousands of
stateful ops/sec the mutex serializes the very path S7 is trying to
parallelize.

**Restart = total amnesia.** All state is lost. Every client must
re-establish from scratch simultaneously — a reclaim storm with no
server-side grace handling. Worse, share reservations and byte-range locks
are silently dropped, which is a correctness hole, not just a
performance cliff.

**ID aliasing on failover.** Clientids restart at 1 on every boot. If a
client fails over from primary A to standby B, B can reissue a clientid
(and stateids) that A had already handed out — the new holder's state can
alias the old holder's in-flight operations.

## 3. Partitioning decision: state follows the shard

**Decision: partition v4.0 state by data shard.** The server that owns the
files owns the state that governs them.

Why not the alternatives:

- **By client (consistent hash to a state tier):** every stateful op gains
  a network hop, and share-reservation / lock-conflict checks still need
  the data server to agree — you've built a distributed transaction on the
  data path to avoid one.
- **By file (lock state hashed independently of open state):** open state
  (per client) and lock state (per file) land in different places; reclaim
  becomes a scatter-gather across the fleet.

The key insight: NFSv4.0 state is only meaningful relative to the files it
governs. Share reservations and byte-range locks are coherence problems
among contenders *for the same file*. Colocating the decider with the data
gives single-node atomicity for every stateful op — no distributed
transactions, no consensus, no 2PC anywhere on the data path.

Consequences, all acceptable:

- A client touching N shards holds N clientids (one per shard server).
  This is already how referrals work today.
- Failover is per-shard, matching the per-shard data failover.
- There is never cross-shard state, because cross-shard mutation doesn't
  exist (`XDEV`).

## 4. Within a shard

### 4.1 Shard the StateManager itself

Split the single mutex into N buckets (e.g. 64) by `hash(clientid)`:
per-bucket `Mutex` guarding that bucket's clients/opens/locks. Every
stateful op touches exactly one bucket. Lease reaping becomes per-bucket
(sweep one bucket per tick, not the world).

Memory is bounded by **admission control**: cap clients per shard server
(configurable, e.g. 200K). At the cap, the server returns `NFS4ERR_DELAY`
for new SETCLIENTID and, for existing clients doing fresh mounts, issues a
referral (`fs_locations`) to a less-loaded shard server. Scale by adding
shards — the same answer as the data path.

### 4.2 Server-qualified IDs (failover without aliasing)

- **clientid:** high 32 bits = server id (unique per shard primary,
  assigned at cluster init and persisted in the image header), low 32 bits
  = local counter. A standby never reissues a live id.
- **stateid `other`:** embed server id + boot generation (12 bytes are
  plenty: 4 server id, 4 boot gen, 4 counter). A stateid from a dead
  primary fails clean with `STALE_STATEID` instead of aliasing a new
  holder's state.

No protocol change: both fields are opaque bytes on the wire. This is a
prerequisite for any failover story and is shippable alone.

### 4.3 State change log (the failover story)

Every state *mutation* — client confirm, open, close, open-downgrade,
lock, locku — appends a small ordered record (tens of bytes: op type,
clientid, owner hash, ino, range, stateid) to an in-memory log with
sequence numbers. Lease *renewals* are deliberately not logged (they're
derivable from liveness; logging them would be write amplification for
zero benefit).

The shard standby tails the log over a dedicated TCP stream (or piggyback
on the `cownfs-replicate` channel). On primary failure the standby
promotes with warm state: it takes over the virtual IP / updates DNS and
serves immediately. Only the unreplicated tail (sub-second) falls back to
client reclaim — and reclaim remains implemented anyway because the spec
requires it.

Cost: one small sequential append per state mutation. State mutations are
orders of magnitude rarer than data I/O, so this is noise.

### 4.4 Reclaim, for when it still happens

Reclaim is the fallback (spec-required; also the path when no standby
exists). Make it survivable:

- **Bounded grace window** (implement the missing RFC 7530 §8.4 grace
  handling): during grace, reclaim-flagged OPEN/LOCK are prioritized;
  non-reclaim state ops get `NFS4ERR_DELAY`.
- **Server-side shedding, not collapsing:** when the reclaim queue
  exceeds a threshold, keep returning `DELAY` with jitter guidance
  rather than melting. Clients already back off on `DELAY`.
- **Optional phase 2 — persistent state WAL:** append the §4.3 log to a
  reserved image area (small, sequential). On restart *without* a standby,
  replay the WAL instead of reclaiming: the storm becomes a local disk
  read. This is the robust answer for single-server deployments that
  still want fast restart.

### 4.5 Lock manager

Stays local to the shard. Byte-range conflict detection only matters among
contenders for the same file, and all of those rendezvous at the shard
that owns the file — no distributed lock manager, ever. This is the
payoff of partition-by-shard. Lock records ride the same change log as
opens (§4.3), so failover preserves them.

### 4.6 Lease renewals at fleet scale

90s lease × 100M clients ≈ 1.1M renewals/sec fleet-wide at perfect pacing
— but renewals are per shard server, so per-server it's
(clients-per-server)/90. At a 200K client cap that's ~2.2K/s of tiny RPCs:
noise. Keep 90s (RFC's suggestion); consider 5 min later to halve it.
Renewal doubles as the failure detector, so don't extend it past the
point where dead-client state lingers too long.

## 5. Failure modes

1. **Shard primary dies.** Standby with tailed log promotes, takes the
   service address, serves with warm state. Sub-second unlogged tail →
   affected clients reclaim (bounded, per §4.4).
2. **Network partition.** Lease expiry reaps the partitioned clients'
   state on the primary; the per-image fencing epoch (already implemented)
   prevents split-brain writes; on heal, clients re-establish with fresh
   server-qualified ids — no aliasing.
3. **Rolling upgrade.** Drain: stop accepting new state, flush the state
   log to the standby, hand over the address. No reclaim storm by design.
   This is only possible *because* of §4.3 — without it, every deploy is
   a fleet-wide reconnect event.
4. **Referral tier failure.** Referral servers hold almost no state
   (namespace skeleton + shard map). Anycast + client-side `fs_locations`
   caching makes this survivable; a dead referral server just shifts new
   mounts elsewhere.

## 6. What NOT to do

- **No separate state tier.** A hop plus consensus on the data path to
  solve a problem that colocation solves for free.
- **No cross-shard locks.** The operations that would need them are
  already `XDEV`.
- **No per-renewal persistence.** Write amplification with no benefit.
- **No global clientid allocator.** Clientids are per-server in v4.0;
  keep them that way — the server-id prefix gives all the uniqueness
  failover needs.

## 7. Migration path (sequenced, each shippable independently)

1. **Server-qualified clientids/stateids** (§4.2). Small, no protocol
   change, kills the aliasing hazard. Shippable alone.
2. **Bucketed StateManager** (§4.1). Removes the mutex bottleneck; no
   behavior change. Shippable alone.
3. **Grace period + reclaim per RFC** (§4.4, minus WAL). Spec compliance
   that's currently missing; required before any failover story.
4. **State change log + standby tailing** (§4.3). Failover without
   reclaim storms. Depends on 1–3.
5. **Admission control + client migration** (§4.1). Caps per-server
   state; the 100M-client story only closes here.
6. **Optional: persistent state WAL** (§4.4). Fast restart without a
   standby.

## 8. Open questions

1. Is the replicated state log (§4.3) worth its complexity, or is a
   well-managed reclaim (§4.4: bounded grace + DELAY shedding) acceptable
   given per-shard client caps? The log is the only path to
   storm-free rolling upgrades.
2. Target clients per shard server (drives shard count for 100M)?
3. Lease duration: keep 90s or extend toward 5 min?
