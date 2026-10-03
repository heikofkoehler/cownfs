# cownfs Security Decisions

This document records the deliberate security tradeoffs in cownfs.
It is not a hardening guide — it is an honest accounting of what the
system does and does not protect against.

## 1. AUTH_SYS Only (No Kerberos)

**Decision:** cownfs implements AUTH_SYS and AUTH_NONE only. There is no
RPCSEC_GSS / Kerberos support, and none is planned.

**Why:** cownfs is designed for trusted local networks (homelab, single-team
dev infrastructure, CI). AUTH_SYS is what every NFS client speaks by default,
and it requires zero configuration. Adding Kerberos would mean KDC setup,
keytabs, and clock synchronization — a heavy operational burden for the
target use case.

**Risks (read these before deploying):**
- AUTH_SYS provides **no authentication**. The client asserts a UID/GID in
  the RPC credential, and the server trusts it. Any client on the network
  can claim to be UID 0.
- UID spoofing is trivial: `mount` with a crafted AUTH_SYS credential gives
  full access to any file, regardless of the server's UID-based quotas or
  ownership checks.
- There is no integrity protection. RPCs can be tampered with in transit.

**When Kerberos would be needed:** Multi-user environments where clients
are not fully trusted, any deployment where UID spoofing would be a
security boundary violation, or compliance regimes requiring authenticated
access.

**Not recommended for:** Untrusted networks, multi-tenant environments,
or any deployment where clients are not under the same administrative
control as the server.

**RFC 7530 reference:** §2.2 (RPCSEC_GSS) is intentionally not implemented.
The server advertises AUTH_SYS via SECINFO (op 33).

## 2. No Transport Encryption

**Decision:** NFS traffic between client and server is unencrypted. There
is no TLS, and none is planned for the NFS protocol itself.

**Threat model:**
- **Trusted local network** (the intended deployment): Switch-level
  security is assumed. An attacker with network tap access is already
  inside the trust boundary.
- **Untrusted network** (internet, shared cloud VPC, hostile LAN):
  **Do not run cownfs directly.** All file data, metadata, and AUTH_SYS
  credentials traverse the wire in cleartext.

**Recommendation for untrusted networks:** Tunnel the NFS traffic through
an encrypted transport. Options:

### stunnel (TLS tunnel)

Server side (`/etc/stunnel/cownfs.conf`):
```
[cownfs]
accept = 0.0.0.0:2050
connect = 127.0.0.1:2049
cert = /etc/stunnel/server.pem
```

Client side:
```
[cownfs]
client = yes
accept = 127.0.0.1:2049
connect = <server>:2050
CAfile = /etc/stunnel/ca.pem
verifyChain = yes
```

Then mount via the local stunnel endpoint:
```
mount -t nfs -o vers=4.0,tcp,port=2049 127.0.0.1:/ /mnt/cow
```

### Alternatives
- **WireGuard:** Point-to-point VPN; lowest overhead. Preferred for
  site-to-site or host-to-host.
- **IPsec:** If the infrastructure already provides it.
- **SSH tunnel:** Works in a pinch (`ssh -L 2049:localhost:2049`);
  not recommended for production due to TCP-over-TCP meltdown.

**What encryption does NOT fix:** AUTH_SYS UID spoofing. Even over an
encrypted tunnel, a malicious client can still claim any UID. Encryption
protects against eavesdropping and tampering, not against malicious
clients.

## 3. No ACLs, No Delegations

### NFSv4 ACLs (RFC 7530 §5.11)

**Not implemented.** cownfs uses POSIX mode bits (owner/group/other,
rwx) exclusively. The `FATTR4_ACL` attribute is not supported; attempts
to set it return `NFS4ERR_ATTRNOTSUPP`.

**What this means:**
- No fine-grained access control lists.
- No inheritance, no named users/groups in ACLs.
- Permission checks are the traditional Unix mode-bit checks.

**Why:** POSIX mode bits cover the target use case. Full NFSv4 ACL
semantics (ALLOW/DENY ACEs, inheritance flags, audit alarms) are complex
and rarely used in practice outside Windows-interop scenarios.

### Delegations (RFC 7530 §9)

**Not implemented.** The server never grants open delegations or file
delegations. All `OPEN` operations complete without delegation state.

**What this means:**
- Clients cannot cache file data aggressively; every read goes to the
  server (subject to normal client attribute caching).
- No `CB_RECALL` callbacks. The server never needs to recall state from
  clients.
- Slightly higher latency for repeated reads of the same file, but simpler
  and more predictable server behavior.

**Why:** Delegations add significant protocol complexity (recall races,
client crash handling) for a performance optimization that matters most
for single-client workloads. cownfs targets shared, multi-client use
where delegations provide less benefit.

### Other intentionally unimplemented RFC 7530 features

| Feature | Section | Status |
|---|---|---|
| RPCSEC_GSS / Kerberos | §2.2 | Not implemented (see §1) |
| NFSv4 ACLs | §5.11 | Not implemented (see above) |
| Delegations | §9 | Not implemented (see above) |
| pNFS layouts | §12 | Not implemented |
| NFSv4.1+ sessions | §2.10 | 4.0 only |

## 4. Quotas

Per-UID block quotas are enforced (`NFS4ERR_DQUOT` on exceed). Quotas
are accounting-based, not reservations — a UID can exceed its quota
only if the check races, which the engine prevents via the txg.

**Caveat:** Quotas are keyed by the file owner's UID, which for AUTH_SYS
is client-asserted (see §1). Quotas are a resource-management feature,
not a security boundary.

## 5. Deferred: A3 Full In-Place Overwrite

**Context:** The CoW engine currently does in-place overwrites only for
blocks allocated in the current (open) transaction group. Blocks from
prior generations with refcount 1 that are not pinned by snapshots
* could* be safely overwritten in place, but the current code CoW-clones
them instead.

**Why deferred:** The full optimization requires a write-ahead log to
remain crash-safe: overwriting a block that the last committed superblock
points to would corrupt the previous generation on crash. The current-txg
subset is crash-safe without a log (uncommitted data is simply lost on
crash, which is the defined semantics).

**Impact of deferral:** Slightly higher write amplification for
overwrite-heavy workloads on snapshotted files. Not a correctness issue.

## Summary

| Threat | Protected? |
|---|---|
| UID spoofing by network client | **No** (AUTH_SYS) |
| Eavesdropping on LAN | **No** (no encryption) |
| Malicious client exceeding quota | Partial (quotas enforced, but UID is spoofable) |
| Crash consistency | **Yes** (CoW, ping-pong superblock) |
| Bit rot (bitmap) | **Yes** (CRC32C sidecars, generation fallback) |
| Unauthorized file access via mode bits | **Yes** (POSIX permissions enforced) |
| Fine-grained ACL bypass | N/A (ACLs not implemented) |

**Bottom line:** cownfs is a correct, crash-safe filesystem for trusted
networks. It is not a hardened multi-tenant storage system. Deploy
accordingly.
