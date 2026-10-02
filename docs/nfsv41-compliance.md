# NFSv4.1 Compliance Assessment

Date: 2026-10-02
Scope: cownfs server's NFSv4.1 (RFC 5661) implementation
Status: **Prototype — not interoperable with real v4.1 clients**

## What works (tested against our own client helpers)

### Session operations
- **EXCHANGE_ID** (42): Basic client ID establishment. Returns static
  server owner/scope. Does not negotiate security flavors or handle
  EXCHANGE_ID flags (EXCHGID4_FLAG_*).
- **CREATE_SESSION** (43): Creates session with slot table. Returns
  simplified channel attributes (fixed 1MiB max sizes, no backchannel
  attrs negotiation).
- **SEQUENCE** (44): Per-slot sequence tracking with replay cache.
  Returns BADSLOT, BADSESSION, SEQ_MISORDERED correctly.
- **DESTROY_SESSION** (47), **DESTROY_CLIENTID** (57): Basic teardown.

### Layout operations (pNFS file layouts)
- **LAYOUTGET** (50): Returns a layout segment for a file range.
- **LAYOUTCOMMIT** (51): Verifies DS block checksums, writes through
  the CoW filesystem path.
- **LAYOUTRETURN** (52): Releases layout ranges.

## Critical gaps

### 1. Missing operations (not implemented)
| Op | Code | Purpose |
|----|------|---------|
| BIND_CONN_TO_SESSION | 41 | Session trunking (multi-conn) |
| BACKCHANNEL_CTL | 45 | Backchannel setup |
| GETDEVICEINFO | 48 | DS discovery |
| GETDEVICELIST | 49 | List DS devices |
| RECLAIM_COMPLETE | 58 | End of recovery |
| LAYOUTERROR | 59 | Client reports layout I/O error |
| LAYOUTSTATS | 60 | Client reports layout stats |

Without GETDEVICEINFO, a real client cannot discover the data server.
Our LAYOUTGET embeds the DS address as a custom string — a hack that
no real client understands.

### 2. Layout wire format is non-compliant
RFC 5661 §13 defines `nfsv4_1_file_layout4` with:
- `deviceid4` referencing a device from GETDEVICEINFO
- `nfl_util` flags
- Stripe indices into the device list

Our implementation:
- Uses a custom body with raw block IDs (`first_block_id + i`)
- Appends the DS address as a counted string (not in the RFC)
- Device ID is 16 zero bytes (not a real device)

A real pNFS client (Linux, etc.) would fail to parse this.

### 3. No backchannel (server-to-client callbacks)
RFC 5661 §2.10 requires the server to initiate callbacks for:
- `CB_LAYOUTRECALL`: Recall a layout (we have the state machine,
  but no wire delivery)
- `CB_RECALL`: Recall delegations (N/A, no delegations)
- `CB_NOTIFY`: Device notifications

Our CREATE_SESSION skips `back_chan_attrs`. The server cannot
asynchronously notify the client.

### 4. No session trunking
Each session is bound to a single TCP connection. BIND_CONN_TO_SESSION
is unimplemented. Real clients use trunking for performance and
failover.

### 5. No recovery protocol
- No grace period after server restart
- No RECLAIM_COMPLETE handling
- Client crash recovery relies on session timeout (not implemented)

### 6. Simplified EXCHANGE_ID/CREATE_SESSION
- No `EXCHGID4_FLAG_*` handling (e.g., SUPP_MOVED_REFER, SUPP_MOVED_MIGR)
- No security negotiation (we only do AUTH_SYS)
- Channel attributes are hardcoded, not negotiated
- No `ca_maxoperations` enforcement

### 7. Layout stateid is synthetic
We construct a layout stateid from block IDs + session ID. RFC 5661
requires a proper stateid with seqid for revocation. Our LAYOUTCOMMIT
does not validate the stateid seqid.

## What would it take to be compliant?

### Phase A: Wire format (2-3 days)
1. Implement GETDEVICEINFO/GETDEVICELIST with a real device table.
2. Rewrite LAYOUTGET to emit RFC-compliant `nfsv4_1_file_layout4`:
   - Real device IDs
   - Stripe indices (even for single DS)
   - Remove the custom DS address string
3. Add LAYOUTERROR/LAYOUTSTATS (client feedback).

### Phase B: Backchannel (1 week)
1. Parse `back_chan_attrs` in CREATE_SESSION.
2. Implement CB_COMPOUND sender (server initiates RPC on the client
   connection, or opens a callback connection).
3. Implement CB_LAYOUTRECALL encoding and delivery.
4. Wire the existing `LayoutTable::recall()` to trigger callbacks.

### Phase C: Sessions (3-5 days)
1. BIND_CONN_TO_SESSION for trunking.
2. BACKCHANNEL_CTL.
3. Proper EXCHANGE_ID flags and channel negotiation.
4. RECLAIM_COMPLETE and grace period.

### Phase D: Validation (1 week)
1. Run pynfs NFSv4.1 test suite (689 tests).
2. Test against Linux pNFS client (requires kernel mount).
3. Fix failures iteratively.

**Total estimate: 3-4 weeks for minimal interoperability.**

## Recommendation

The current v4.1 code is a **functional prototype** that proves the
architecture (MDS + DS separation, CoW write-through). It is not
interoperable.

Options:
1. **Keep as prototype**: Document as experimental, focus on v4.0
   correctness (which is the production path).
2. **Invest in compliance**: 3-4 weeks to Phase D, requires a Linux
   client for validation.
3. **Hybrid**: Do Phase A (wire format) so the protocol is at least
   parseable by real clients, defer B/C/D.

The v4.0 implementation is the priority — it's what Heiko mounts on
his Mac. The v4.1/pNFS work should stay experimental until v4.0 is
rock-solid.
