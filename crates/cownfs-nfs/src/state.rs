//! NFSv4 state management for P6: clients, opens, locks, leases.
//!
//! - SETCLIENTID/SETCLIENTID_CONFIRM establishes a client with a lease.
//! - OPEN creates open state with a real stateid; share reservations are enforced.
//! - LOCK/LOCKU manage byte-range locks with conflict detection.
//! - Sequence IDs provide replay protection.
//! - Expired leases reap all client state.

use crate::nfs4::{
    NfsError, StateId, NFS4ERR_BAD_SEQID, NFS4ERR_DENIED, NFS4ERR_EXPIRED, NFS4ERR_GRACE,
    NFS4ERR_INVAL, NFS4ERR_LOCKED, NFS4ERR_NO_GRACE, NFS4ERR_STALE_CLIENTID,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Lease duration. Short for testability; RFC 7530 suggests 90s.
pub const LEASE_DURATION: Duration = Duration::from_secs(90);

/// Share access bits.
pub const OPEN4_SHARE_ACCESS_READ: u32 = 1;
pub const OPEN4_SHARE_ACCESS_WRITE: u32 = 2;
pub const OPEN4_SHARE_ACCESS_BOTH: u32 = 3;

/// Share deny bits.
pub const OPEN4_SHARE_DENY_NONE: u32 = 0;
pub const OPEN4_SHARE_DENY_READ: u32 = 1;
pub const OPEN4_SHARE_DENY_WRITE: u32 = 2;
pub const OPEN4_SHARE_DENY_BOTH: u32 = 3;

#[derive(Debug)]
pub struct ClientRecord {
    pub verifier: [u8; 8],
    pub name: Vec<u8>,
    pub confirmed: bool,
    pub lease_expiry: Instant,
}

#[derive(Debug, Clone)]
pub struct OpenRecord {
    pub clientid: u64,
    pub owner: Vec<u8>,
    pub file_ino: u64,
    pub stateid: StateId,
    pub share_access: u32,
    pub share_deny: u32,
    /// Last seqid used (for CLOSE replay).
    pub seqid: u32,
}

#[derive(Debug, Clone)]
pub struct LockRecord {
    pub clientid: u64,
    pub owner: Vec<u8>,
    pub file_ino: u64,
    pub stateid: StateId,
    pub offset: u64,
    pub length: u64, // u64::MAX means to EOF
    pub locktype: u32,
    /// Last seqid used (for LOCK/LOCKU replay).
    pub seqid: u32,
}

/// Number of state buckets. Must be a power of two. Every per-client map
/// is keyed (in part) by clientid, so any operation on a single client's
/// state touches exactly one bucket and needs only that bucket's lock.
const NUM_BUCKETS: usize = 64;

/// State mutation log record (v40-state-partitioning §4.3, step 4).
/// Every state *mutation* appends one of these to the in-memory log;
/// a standby tails the log and applies the records to stay warm.
/// Lease renewals are deliberately not logged.
#[derive(Debug, Clone)]
pub enum StateLogRecord {
    ClientConfirmed {
        clientid: u64,
        verifier: [u8; 8],
        name: Vec<u8>,
    },
    ClientExpired {
        clientid: u64,
    },
    Open {
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
        stateid: StateId,
        seqid: u32,
    },
    Close {
        stateid: StateId,
    },
    OpenDowngrade {
        stateid: StateId,
        share_access: u32,
        share_deny: u32,
        seqid: u32,
    },
    Lock {
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        offset: u64,
        length: u64,
        locktype: u32,
        stateid: StateId,
        seqid: u32,
    },
    Unlock {
        stateid: StateId,
        offset: u64,
        length: u64,
        seqid: u32,
    },
}

/// Maximum log records retained in memory. A standby that falls more than
/// this far behind must resync (not implemented in v1; it errors).
const MAX_LOG_RECORDS: usize = 1_000_000;

impl StateLogRecord {
    /// Discriminant for the wire format.
    fn tag(&self) -> u8 {
        match self {
            StateLogRecord::ClientConfirmed { .. } => 1,
            StateLogRecord::ClientExpired { .. } => 2,
            StateLogRecord::Open { .. } => 3,
            StateLogRecord::Close { .. } => 4,
            StateLogRecord::OpenDowngrade { .. } => 5,
            StateLogRecord::Lock { .. } => 6,
            StateLogRecord::Unlock { .. } => 7,
        }
    }

    /// Encode to a byte vector (simple binary format for the tail stream).
    pub fn encode(&self) -> Vec<u8> {
        fn u64b(out: &mut Vec<u8>, v: u64) {
            out.extend_from_slice(&v.to_be_bytes());
        }
        fn u32b(out: &mut Vec<u8>, v: u32) {
            out.extend_from_slice(&v.to_be_bytes());
        }
        fn bytes(out: &mut Vec<u8>, b: &[u8]) {
            u32b(out, b.len() as u32);
            out.extend_from_slice(b);
        }
        fn sid(out: &mut Vec<u8>, s: &StateId) {
            out.extend_from_slice(&s.seqid.to_be_bytes());
            out.extend_from_slice(&s.other);
        }
        let mut out = Vec::new();
        out.push(self.tag());
        match self {
            StateLogRecord::ClientConfirmed {
                clientid,
                verifier,
                name,
            } => {
                u64b(&mut out, *clientid);
                out.extend_from_slice(verifier);
                bytes(&mut out, name);
            }
            StateLogRecord::ClientExpired { clientid } => {
                u64b(&mut out, *clientid);
            }
            StateLogRecord::Open {
                clientid,
                owner,
                file_ino,
                share_access,
                share_deny,
                stateid,
                seqid,
            } => {
                u64b(&mut out, *clientid);
                bytes(&mut out, owner);
                u64b(&mut out, *file_ino);
                u32b(&mut out, *share_access);
                u32b(&mut out, *share_deny);
                sid(&mut out, stateid);
                u32b(&mut out, *seqid);
            }
            StateLogRecord::Close { stateid } => {
                sid(&mut out, stateid);
            }
            StateLogRecord::OpenDowngrade {
                stateid,
                share_access,
                share_deny,
                seqid,
            } => {
                sid(&mut out, stateid);
                u32b(&mut out, *share_access);
                u32b(&mut out, *share_deny);
                u32b(&mut out, *seqid);
            }
            StateLogRecord::Lock {
                clientid,
                owner,
                file_ino,
                offset,
                length,
                locktype,
                stateid,
                seqid,
            } => {
                u64b(&mut out, *clientid);
                bytes(&mut out, owner);
                u64b(&mut out, *file_ino);
                u64b(&mut out, *offset);
                u64b(&mut out, *length);
                u32b(&mut out, *locktype);
                sid(&mut out, stateid);
                u32b(&mut out, *seqid);
            }
            StateLogRecord::Unlock {
                stateid,
                offset,
                length,
                seqid,
            } => {
                sid(&mut out, stateid);
                u64b(&mut out, *offset);
                u64b(&mut out, *length);
                u32b(&mut out, *seqid);
            }
        }
        out
    }

    /// Decode from a byte slice. Returns (record, bytes_consumed).
    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.is_empty() {
            return None;
        }
        let tag = buf[0];
        let mut pos = 1usize;
        let mut u64b = |buf: &[u8], pos: &mut usize| -> Option<u64> {
            if buf.len() < *pos + 8 {
                return None;
            }
            let v = u64::from_be_bytes(buf[*pos..*pos + 8].try_into().ok()?);
            *pos += 8;
            Some(v)
        };
        let mut u32b = |buf: &[u8], pos: &mut usize| -> Option<u32> {
            if buf.len() < *pos + 4 {
                return None;
            }
            let v = u32::from_be_bytes(buf[*pos..*pos + 4].try_into().ok()?);
            *pos += 4;
            Some(v)
        };
        let mut bytes = |buf: &[u8], pos: &mut usize| -> Option<Vec<u8>> {
            let len = u32b(buf, pos)? as usize;
            if buf.len() < *pos + len {
                return None;
            }
            let v = buf[*pos..*pos + len].to_vec();
            *pos += len;
            Some(v)
        };
        let mut sid = |buf: &[u8], pos: &mut usize| -> Option<StateId> {
            let seqid = u32b(buf, pos)?;
            if buf.len() < *pos + 12 {
                return None;
            }
            let mut other = [0u8; 12];
            other.copy_from_slice(&buf[*pos..*pos + 12]);
            *pos += 12;
            Some(StateId { seqid, other })
        };
        let rec = match tag {
            1 => {
                let clientid = u64b(buf, &mut pos)?;
                if buf.len() < pos + 8 {
                    return None;
                }
                let mut verifier = [0u8; 8];
                verifier.copy_from_slice(&buf[pos..pos + 8]);
                pos += 8;
                let name = bytes(buf, &mut pos)?;
                StateLogRecord::ClientConfirmed {
                    clientid,
                    verifier,
                    name,
                }
            }
            2 => {
                let clientid = u64b(buf, &mut pos)?;
                StateLogRecord::ClientExpired { clientid }
            }
            3 => {
                let clientid = u64b(buf, &mut pos)?;
                let owner = bytes(buf, &mut pos)?;
                let file_ino = u64b(buf, &mut pos)?;
                let share_access = u32b(buf, &mut pos)?;
                let share_deny = u32b(buf, &mut pos)?;
                let stateid = sid(buf, &mut pos)?;
                let seqid = u32b(buf, &mut pos)?;
                StateLogRecord::Open {
                    clientid,
                    owner,
                    file_ino,
                    share_access,
                    share_deny,
                    stateid,
                    seqid,
                }
            }
            4 => {
                let stateid = sid(buf, &mut pos)?;
                StateLogRecord::Close { stateid }
            }
            5 => {
                let stateid = sid(buf, &mut pos)?;
                let share_access = u32b(buf, &mut pos)?;
                let share_deny = u32b(buf, &mut pos)?;
                let seqid = u32b(buf, &mut pos)?;
                StateLogRecord::OpenDowngrade {
                    stateid,
                    share_access,
                    share_deny,
                    seqid,
                }
            }
            6 => {
                let clientid = u64b(buf, &mut pos)?;
                let owner = bytes(buf, &mut pos)?;
                let file_ino = u64b(buf, &mut pos)?;
                let offset = u64b(buf, &mut pos)?;
                let length = u64b(buf, &mut pos)?;
                let locktype = u32b(buf, &mut pos)?;
                let stateid = sid(buf, &mut pos)?;
                let seqid = u32b(buf, &mut pos)?;
                StateLogRecord::Lock {
                    clientid,
                    owner,
                    file_ino,
                    offset,
                    length,
                    locktype,
                    stateid,
                    seqid,
                }
            }
            7 => {
                let stateid = sid(buf, &mut pos)?;
                let offset = u64b(buf, &mut pos)?;
                let length = u64b(buf, &mut pos)?;
                let seqid = u32b(buf, &mut pos)?;
                StateLogRecord::Unlock {
                    stateid,
                    offset,
                    length,
                    seqid,
                }
            }
            _ => return None,
        };
        Some((rec, pos))
    }
}

/// In-memory state mutation log.
struct StateLog {
    /// (sequence number, record), in order. Sequence numbers start at 1.
    records: std::collections::VecDeque<(u64, StateLogRecord)>,
    next_seq: u64,
}

impl StateLog {
    fn new() -> Self {
        StateLog {
            records: std::collections::VecDeque::new(),
            next_seq: 1,
        }
    }

    /// Append a record, returning its sequence number.
    fn append(&mut self, record: StateLogRecord) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.records.push_back((seq, record));
        while self.records.len() > MAX_LOG_RECORDS {
            self.records.pop_front();
        }
        seq
    }

    /// Oldest retained sequence number, or next_seq if empty.
    fn oldest_seq(&self) -> u64 {
        self.records
            .front()
            .map(|(s, _)| *s)
            .unwrap_or(self.next_seq)
    }
}

/// Per-bucket state: all client/open/lock records for the clientids that
/// hash to this bucket.
struct Bucket {
    clients: HashMap<u64, ClientRecord>,
    opens: HashMap<(u64, Vec<u8>), OpenRecord>,
    locks: HashMap<(u64, Vec<u8>), LockRecord>,
}

impl Bucket {
    fn new() -> Self {
        Bucket {
            clients: HashMap::new(),
            opens: HashMap::new(),
            locks: HashMap::new(),
        }
    }
}

pub struct StateManager {
    buckets: [Mutex<Bucket>; NUM_BUCKETS],
    /// Client name -> clientid, for SETCLIENTID re-establishment lookup.
    /// Lock ordering: names -> bucket. Never acquire in the reverse order.
    names: Mutex<HashMap<Vec<u8>, u64>>,
    /// Serializes the conflict-check+insert of open() and lock(), which must
    /// observe a cross-bucket-consistent view of same-file state to keep
    /// share/lock conflict detection exact. Lock ordering: conflict ->
    /// bucket. Never acquire in the reverse order.
    conflict: Mutex<()>,
    /// Post-restart grace period (RFC 7530 §8.4). While `Some(deadline)` is
    /// in the future, the server accepts reclaim (CLAIM_PREVIOUS / reclaim
    /// locks) and rejects new state establishment with NFS4ERR_GRACE.
    /// Entered at server startup; `None` once expired or never entered.
    /// Only ever locked alone (never nested inside bucket/names locks).
    grace_until: Mutex<Option<Instant>>,
    /// Unique per shard primary (see docs/v40-state-partitioning.md §4.2).
    /// Occupies the high 32 bits of every clientid this server issues, so
    /// two servers never issue the same clientid and a standby never
    /// reissues a dead primary's ids.
    server_id: AtomicU32,
    /// Random per process boot; embedded in every stateid's `other` field
    /// so a stateid minted before a restart can never alias one minted
    /// after (defense for the future state-log/WAL replay path).
    /// A standby adopts the primary's (server_id, boot_gen) when tailing
    /// (§4.3 step 4) so it owns the primary's stateids on failover.
    boot_gen: AtomicU32,
    /// Low 32 bits of issued clientids. Never 0 (0 clientid = invalid).
    next_client_seq: AtomicU32,
    next_stateid_seq: AtomicU32,
    lease_duration: Duration,
    /// State mutation log (§4.3 step 4). Appended on every mutation;
    /// tailed by the standby. Locked alone, never nested.
    log: Mutex<StateLog>,
    /// Admission control (§4.1 step 5): max registered clients. 0 = unlimited.
    /// At the cap, new SETCLIENTID gets NFS4ERR_DELAY; re-establishment of
    /// known clients is always allowed.
    max_clients: std::sync::atomic::AtomicUsize,
    /// Persistent state WAL (§4.4 step 6). Appended on every log_append;
    /// replayed on restart instead of reclaiming. Locked alone, never nested.
    wal: Mutex<Option<crate::state_wal::StateWal>>,
}

/// Generate a random boot generation from OS entropy, falling back to a
/// time+pid mix if /dev/urandom is unavailable.
fn random_boot_gen() -> u32 {
    let mut b = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok()
    {
        return u32::from_ne_bytes(b);
    }
    // Fallback: nanos since epoch mixed with pid. Only needs uniqueness
    // across boots of one server, not cryptographic strength.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let mut x = nanos ^ pid.wrapping_mul(0x9e3779b97f4a7c15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 32;
    x as u32
}

impl StateManager {
    pub fn new() -> Self {
        Self::with_server_id(0)
    }

    /// Create a StateManager that qualifies all issued ids with `server_id`.
    /// Every shard primary (and its standbys) must use a distinct server id;
    /// see docs/v40-state-partitioning.md §4.2.
    pub fn with_server_id(server_id: u32) -> Self {
        Self::with_server_id_and_lease(server_id, LEASE_DURATION)
    }

    fn with_server_id_and_lease(server_id: u32, lease_duration: Duration) -> Self {
        StateManager {
            buckets: std::array::from_fn(|_| Mutex::new(Bucket::new())),
            names: Mutex::new(HashMap::new()),
            conflict: Mutex::new(()),
            grace_until: Mutex::new(None),
            server_id: AtomicU32::new(server_id),
            boot_gen: AtomicU32::new(random_boot_gen()),
            next_client_seq: AtomicU32::new(1),
            next_stateid_seq: AtomicU32::new(1),
            lease_duration,
            log: Mutex::new(StateLog::new()),
            max_clients: std::sync::atomic::AtomicUsize::new(0),
            wal: Mutex::new(None),
        }
    }

    /// Enter the post-restart grace period (RFC 7530 §8.4): until the lease
    /// duration elapses, reclaim is accepted and new state establishment
    /// fails with NFS4ERR_GRACE. Called once at server startup.
    pub fn enter_grace_period(&self) {
        self.enter_grace_period_for(self.lease_duration);
    }

    /// Enter a grace period of custom length (for tests).
    pub fn enter_grace_period_for(&self, d: Duration) {
        *self.grace_until.lock().unwrap() = Some(Instant::now() + d);
    }

    /// Leave the grace period immediately (for tests).
    pub fn exit_grace_period(&self) {
        *self.grace_until.lock().unwrap() = None;
    }

    /// True while the server is in its post-restart grace period.
    pub fn in_grace(&self) -> bool {
        match *self.grace_until.lock().unwrap() {
            Some(t) => t > Instant::now(),
            None => false,
        }
    }

    /// Append a mutation record to the state log. Returns the sequence number.
    /// Called by the primary on every state mutation; the standby tails these.
    /// If a persistent WAL is attached (§4.4 step 6), the record is also
    /// fsynced to the WAL before returning.
    fn log_append(&self, record: StateLogRecord) -> u64 {
        let seq = self.log.lock().unwrap().append(record.clone());
        if let Some(wal) = self.wal.lock().unwrap().as_mut() {
            if let Err(e) = wal.append(seq, &record) {
                eprintln!("state WAL append failed: {e}");
            }
        }
        seq
    }

    /// Read log records with seq >= `from_seq`. Returns (records, oldest_seq).
    /// If `from_seq` is older than the oldest retained record, returns an
    /// error (the tailer has fallen too far behind).
    pub fn log_since(&self, from_seq: u64) -> Result<(Vec<(u64, StateLogRecord)>, u64), NfsError> {
        let log = self.log.lock().unwrap();
        let oldest = log.oldest_seq();
        if from_seq < oldest {
            return Err(NfsError::Status(NFS4ERR_INVAL));
        }
        let out: Vec<_> = log
            .records
            .iter()
            .filter(|(s, _)| *s >= from_seq)
            .cloned()
            .collect();
        Ok((out, oldest))
    }

    /// Current log sequence number (next to be assigned).
    pub fn log_next_seq(&self) -> u64 {
        self.log.lock().unwrap().next_seq
    }

    /// Oldest retained log sequence number (for the tail server).
    pub fn log_oldest_seq(&self) -> u64 {
        self.log.lock().unwrap().oldest_seq()
    }

    /// Apply a tailed log record (standby side). Replicates the primary's
    /// mutation without re-validating: the primary already checked.
    /// Idempotent for the record types we generate.
    pub fn apply_record(&self, record: &StateLogRecord) {
        match record {
            StateLogRecord::ClientConfirmed {
                clientid,
                verifier,
                name,
            } => {
                let idx = Self::bucket_idx(*clientid);
                let mut b = self.buckets[idx].lock().unwrap();
                let c = b.clients.entry(*clientid).or_insert_with(|| ClientRecord {
                    verifier: *verifier,
                    name: name.clone(),
                    confirmed: false,
                    lease_expiry: Instant::now() + self.lease_duration,
                });
                c.confirmed = true;
                c.lease_expiry = Instant::now() + self.lease_duration;
                self.names.lock().unwrap().insert(name.clone(), *clientid);
            }
            StateLogRecord::ClientExpired { clientid } => {
                self.remove_client(*clientid);
            }
            StateLogRecord::Open {
                clientid,
                owner,
                file_ino,
                share_access,
                share_deny,
                stateid,
                seqid,
            } => {
                let idx = Self::bucket_idx(*clientid);
                let mut b = self.buckets[idx].lock().unwrap();
                b.opens.insert(
                    (*clientid, owner.clone()),
                    OpenRecord {
                        clientid: *clientid,
                        owner: owner.clone(),
                        file_ino: *file_ino,
                        stateid: stateid.clone(),
                        share_access: *share_access,
                        share_deny: *share_deny,
                        seqid: *seqid,
                    },
                );
            }
            StateLogRecord::Close { stateid } => {
                // Find and remove the open with this stateid.
                for bucket in &self.buckets {
                    let mut b = bucket.lock().unwrap();
                    if let Some(key) = b
                        .opens
                        .iter()
                        .find(|(_, r)| r.stateid == *stateid)
                        .map(|(k, _)| k.clone())
                    {
                        b.opens.remove(&key);
                        break;
                    }
                }
            }
            StateLogRecord::OpenDowngrade {
                stateid,
                share_access,
                share_deny,
                seqid,
            } => {
                for bucket in &self.buckets {
                    let mut b = bucket.lock().unwrap();
                    if let Some(r) = b.opens.values_mut().find(|r| r.stateid == *stateid) {
                        r.share_access = *share_access;
                        r.share_deny = *share_deny;
                        r.seqid = *seqid;
                        break;
                    }
                }
            }
            StateLogRecord::Lock {
                clientid,
                owner,
                file_ino,
                offset,
                length,
                locktype,
                stateid,
                seqid,
            } => {
                let idx = Self::bucket_idx(*clientid);
                let mut b = self.buckets[idx].lock().unwrap();
                b.locks.insert(
                    (*clientid, owner.clone()),
                    LockRecord {
                        clientid: *clientid,
                        owner: owner.clone(),
                        file_ino: *file_ino,
                        stateid: stateid.clone(),
                        offset: *offset,
                        length: *length,
                        locktype: *locktype,
                        seqid: *seqid,
                    },
                );
            }
            StateLogRecord::Unlock {
                stateid,
                offset,
                length,
                ..
            } => {
                for bucket in &self.buckets {
                    let mut b = bucket.lock().unwrap();
                    if let Some(key) = b
                        .locks
                        .iter()
                        .find(|(_, r)| {
                            r.stateid == *stateid && r.offset == *offset && r.length == *length
                        })
                        .map(|(k, _)| k.clone())
                    {
                        b.locks.remove(&key);
                        break;
                    }
                }
            }
        }
    }

    /// Remove a client and all its state (helper for apply/expiry).
    fn remove_client(&self, clientid: u64) {
        let idx = Self::bucket_idx(clientid);
        let mut b = self.buckets[idx].lock().unwrap();
        if let Some(c) = b.clients.remove(&clientid) {
            self.names.lock().unwrap().remove(&c.name);
        }
        b.opens.retain(|(cid, _), _| *cid != clientid);
        b.locks.retain(|(cid, _), _| *cid != clientid);
    }

    /// Bucket index for a clientid. The low 32 bits are a per-server
    /// sequence, so the low 6 bits distribute uniformly.
    #[inline]
    fn bucket_idx(clientid: u64) -> usize {
        (clientid as usize) & (NUM_BUCKETS - 1)
    }

    /// Allocate the next sequence value, skipping 0 (reserved/invalid).
    /// Wraps past u32::MAX only after 4B registrations, at which point no
    /// earlier id can still be live.
    fn alloc_seq(counter: &AtomicU32) -> u32 {
        let mut s = counter.fetch_add(1, Ordering::Relaxed);
        if s == 0 {
            s = counter.fetch_add(1, Ordering::Relaxed);
            if s == 0 {
                s = 1; // Unreachable in practice; keep the invariant.
            }
        }
        s
    }

    /// Change the server id. Must be called before the server starts
    /// accepting clients (no issued ids may exist yet). The store happens
    /// under the names lock, so no concurrent setclientid can interleave
    /// between the emptiness check and the store.
    pub fn set_server_id(&self, server_id: u32) {
        let names = self.names.lock().unwrap();
        let non_empty = self
            .buckets
            .iter()
            .any(|b| !b.lock().unwrap().clients.is_empty());
        if non_empty {
            // Panic without holding locks, so nothing is poisoned.
            drop(names);
            panic!("set_server_id after clients were registered");
        }
        self.server_id.store(server_id, Ordering::Relaxed);
    }

    /// The server id this manager qualifies ids with.
    pub fn server_id(&self) -> u32 {
        self.server_id.load(Ordering::Relaxed)
    }

    /// Adopt a boot generation (standby tailing the primary's log, §4.3).
    /// Must be called before any local state exists.
    pub fn set_boot_gen(&self, boot_gen: u32) {
        self.boot_gen.store(boot_gen, Ordering::Relaxed);
    }

    /// The boot generation this manager stamps into stateids.
    pub fn boot_gen(&self) -> u32 {
        self.boot_gen.load(Ordering::Relaxed)
    }

    /// Extract the issuing server id from a clientid.
    pub fn clientid_server_id(clientid: u64) -> u32 {
        (clientid >> 32) as u32
    }

    /// Extract the issuing server id from a stateid's `other` field.
    pub fn stateid_server_id(stateid: &StateId) -> u32 {
        u32::from_be_bytes(stateid.other[0..4].try_into().unwrap())
    }

    /// Extract the boot generation from a stateid's `other` field.
    pub fn stateid_boot_gen(stateid: &StateId) -> u32 {
        u32::from_be_bytes(stateid.other[4..8].try_into().unwrap())
    }

    /// True if this stateid was minted by this server in this boot.
    /// Used by the future standby/failover path to tell locally-minted
    /// state from a previous incarnation's (which must fail clean with
    /// STALE_STATEID, never alias).
    pub fn owns_stateid(&self, stateid: &StateId) -> bool {
        Self::stateid_server_id(stateid) == self.server_id()
            && Self::stateid_boot_gen(stateid) == self.boot_gen.load(Ordering::Relaxed)
    }

    /// For tests: use a short lease.
    pub fn with_lease(lease: Duration) -> Self {
        Self::with_server_id_and_lease(0, lease)
    }

    fn new_stateid(&self) -> StateId {
        let seq = Self::alloc_seq(&self.next_stateid_seq);
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.server_id().to_be_bytes());
        b[4..8].copy_from_slice(&self.boot_gen.load(Ordering::Relaxed).to_be_bytes());
        b[8..12].copy_from_slice(&seq.to_be_bytes());
        StateId { seqid: 0, other: b }
    }

    /// Reap expired clients across all buckets. (Primarily for tests; the
    /// per-op paths reap lazily per bucket.)
    pub fn reap_expired(&self) {
        self.reap_all();
    }

    /// Reap expired clients in one bucket (and their opens/locks).
    /// Lock ordering: names -> bucket.
    fn reap_bucket(&self, idx: usize) {
        let mut names = self.names.lock().unwrap();
        let mut b = self.buckets[idx].lock().unwrap();
        let now = Instant::now();
        let expired: Vec<u64> = b
            .clients
            .iter()
            .filter(|(_, c)| c.lease_expiry < now)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            if let Some(c) = b.clients.remove(id) {
                names.remove(&c.name);
            }
            b.opens.retain(|(cid, _), _| cid != id);
            b.locks.retain(|(cid, _), _| cid != id);
        }
        // Log expiries after releasing bucket/names locks.
        drop(b);
        drop(names);
        for id in expired {
            self.log_append(StateLogRecord::ClientExpired { clientid: id });
        }
    }

    /// Reap expired clients across all buckets. Used where the old code
    /// did a global reap (SETCLIENTID's name lookup must not see expired
    /// clients in any bucket).
    fn reap_all(&self) {
        for idx in 0..NUM_BUCKETS {
            self.reap_bucket(idx);
        }
    }

    fn check_client(&self, clientid: u64) -> Result<(), NfsError> {
        let idx = Self::bucket_idx(clientid);
        self.reap_bucket(idx);
        let b = self.buckets[idx].lock().unwrap();
        match b.clients.get(&clientid) {
            Some(c) if c.confirmed => Ok(()),
            Some(_) => Err(NfsError::Status(NFS4ERR_STALE_CLIENTID)),
            None => Err(NfsError::Status(NFS4ERR_STALE_CLIENTID)),
        }
    }

    fn renew_lease_in(&self, bucket: &mut Bucket, clientid: u64) {
        if let Some(c) = bucket.clients.get_mut(&clientid) {
            c.lease_expiry = Instant::now() + self.lease_duration;
        }
    }

    /// SETCLIENTID: register or update a client. Returns (clientid, confirmed).
    /// At the admission-control cap, a NEW name gets `Err(NFS4ERR_DELAY)`;
    /// re-establishment of a known client is always allowed.
    pub fn setclientid(&self, verifier: [u8; 8], name: Vec<u8>) -> Result<(u64, bool), u32> {
        self.reap_all();
        // Lock ordering: names -> bucket.
        let mut names = self.names.lock().unwrap();
        if let Some(&id) = names.get(&name) {
            let mut b = self.buckets[Self::bucket_idx(id)].lock().unwrap();
            let same_verifier = b
                .clients
                .get(&id)
                .expect("name index and client map disagree")
                .verifier
                == verifier;
            if same_verifier {
                // Same client, restart. Keep id, mark unconfirmed.
                let c = b.clients.get_mut(&id).unwrap();
                c.confirmed = false;
                c.lease_expiry = Instant::now() + self.lease_duration;
                Ok((id, false))
            } else {
                // Different verifier: new incarnation, drop old state.
                b.opens.retain(|(cid, _), _| *cid != id);
                b.locks.retain(|(cid, _), _| *cid != id);
                let c = b
                    .clients
                    .get_mut(&id)
                    .expect("name index and client map disagree");
                c.verifier = verifier;
                c.confirmed = false;
                c.lease_expiry = Instant::now() + self.lease_duration;
                Ok((id, false))
            }
        } else {
            // Admission control (§4.1): at the cap, refuse NEW clients with
            // DELAY so the client retries (or the LB sends it elsewhere).
            let max = self.max_clients.load(std::sync::atomic::Ordering::Relaxed);
            if max > 0 {
                let count: usize = self
                    .buckets
                    .iter()
                    .map(|bucket| bucket.lock().unwrap().clients.len())
                    .sum();
                if count >= max {
                    return Err(crate::nfs4::NFS4ERR_DELAY);
                }
            }
            // Server-qualified clientid: high 32 bits = server id, low
            // 32 bits = local sequence. Two servers never issue the
            // same clientid; low bits never 0 (0 = invalid clientid).
            let seq = Self::alloc_seq(&self.next_client_seq);
            let id = ((self.server_id() as u64) << 32) | (seq as u64);
            let mut b = self.buckets[Self::bucket_idx(id)].lock().unwrap();
            b.clients.insert(
                id,
                ClientRecord {
                    verifier,
                    name: name.clone(),
                    confirmed: false,
                    lease_expiry: Instant::now() + self.lease_duration,
                },
            );
            names.insert(name, id);
            Ok((id, false))
        }
    }

    /// SETCLIENTID_CONFIRM: confirm a client. Returns true if confirmed.
    pub fn confirm(&self, clientid: u64, verifier: [u8; 8]) -> bool {
        let (ok, name) = {
            let mut b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
            match b.clients.get_mut(&clientid) {
                Some(c) if c.verifier == verifier => {
                    c.confirmed = true;
                    c.lease_expiry = Instant::now() + self.lease_duration;
                    (true, c.name.clone())
                }
                _ => (false, Vec::new()),
            }
        };
        if ok {
            self.log_append(StateLogRecord::ClientConfirmed {
                clientid,
                verifier,
                name,
            });
        }
        ok
    }

    /// RENEW: renew the lease. Returns true if client exists.
    pub fn renew(&self, clientid: u64) -> bool {
        let idx = Self::bucket_idx(clientid);
        self.reap_bucket(idx);
        let mut b = self.buckets[idx].lock().unwrap();
        match b.clients.get_mut(&clientid) {
            Some(c) => {
                c.lease_expiry = Instant::now() + self.lease_duration;
                true
            }
            None => false,
        }
    }

    /// OPEN: create open state. Checks share reservations.
    /// Returns the OpenRecord or an NFS error.
    ///
    /// `reclaim` = CLAIM_PREVIOUS (RFC 7530 §8.4): the client re-establishes
    /// state it held before a server restart. Reclaim is only valid during
    /// the grace period and is granted without share-conflict checks (the
    /// server lost all pre-restart state, so there is nothing to check
    /// against). Non-reclaim opens during grace fail with NFS4ERR_GRACE.
    ///
    /// The share-conflict check scans all buckets under the conflict lock,
    /// so two racing opens observe a consistent view and conflict detection
    /// stays exact (same atomicity the old global lock provided).
    pub fn open(
        &self,
        seqid: u32,
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
        reclaim: bool,
    ) -> Result<OpenRecord, NfsError> {
        self.check_client(clientid)?;
        let rec = if reclaim {
            if !self.in_grace() {
                return Err(NfsError::Status(NFS4ERR_NO_GRACE));
            }
            self.insert_open(seqid, clientid, owner, file_ino, share_access, share_deny)
        } else {
            if self.in_grace() {
                return Err(NfsError::Status(NFS4ERR_GRACE));
            }
            // Serialize conflict-check+insert vs other open()/lock() calls.
            // Lock ordering: conflict -> bucket.
            let _conflict = self.conflict.lock().unwrap();
            // Check share conflicts with existing opens on the same file
            // (excluding our own opens from the same client).
            for bucket in &self.buckets {
                let b = bucket.lock().unwrap();
                for o in b.opens.values() {
                    if o.file_ino != file_ino || o.clientid == clientid {
                        continue;
                    }
                    // If existing denies what we want, or we deny what existing has.
                    let deny_conflict = (o.share_deny & share_access) != 0;
                    let access_conflict = (share_deny & o.share_access) != 0;
                    if deny_conflict || access_conflict {
                        return Err(NfsError::Status(NFS4ERR_DENIED));
                    }
                }
            }
            let mut b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
            self.insert_open_in(
                &mut b,
                seqid,
                clientid,
                owner,
                file_ino,
                share_access,
                share_deny,
            )
        };
        self.log_append(StateLogRecord::Open {
            clientid: rec.clientid,
            owner: rec.owner.clone(),
            file_ino: rec.file_ino,
            share_access: rec.share_access,
            share_deny: rec.share_deny,
            stateid: rec.stateid.clone(),
            seqid: rec.seqid,
        });
        Ok(rec)
    }

    /// Merge-or-insert an open record into an already-locked bucket.
    /// Called with the bucket lock held (and, for non-reclaim opens, the
    /// conflict lock held).
    fn insert_open_in(
        &self,
        b: &mut Bucket,
        seqid: u32,
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
    ) -> OpenRecord {
        let key = (clientid, owner.clone());
        // If the same open_owner reopens the same file, merge share modes
        // (upgrade) instead of creating a duplicate. Accept any seqid for
        // the upgrade to be lenient with client seqid tracking.
        if let Some(existing) = b.opens.get_mut(&key) {
            if existing.file_ino == file_ino {
                existing.share_access |= share_access;
                existing.share_deny |= share_deny;
                existing.seqid = seqid;
                existing.stateid.seqid += 1;
                let rec = existing.clone();
                self.renew_lease_in(b, clientid);
                return rec;
            }
        }
        let stateid = self.new_stateid();
        let rec = OpenRecord {
            clientid,
            owner,
            file_ino,
            stateid,
            share_access,
            share_deny,
            seqid,
        };
        b.opens.insert(key, rec.clone());
        self.renew_lease_in(b, clientid);
        rec
    }

    /// Insert an open record for the reclaim path (no conflict checks; the
    /// caller holds no locks).
    fn insert_open(
        &self,
        seqid: u32,
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
    ) -> OpenRecord {
        let mut b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
        self.insert_open_in(
            &mut b,
            seqid,
            clientid,
            owner,
            file_ino,
            share_access,
            share_deny,
        )
    }

    /// Scan all buckets for the open with this exact stateid.
    /// Returns (bucket_idx, key). The stateid doesn't encode the clientid,
    /// so this is a full scan (as before); callers re-validate under the
    /// bucket lock.
    fn find_open_key(&self, stateid: &StateId) -> Option<(usize, (u64, Vec<u8>))> {
        for (idx, bucket) in self.buckets.iter().enumerate() {
            let b = bucket.lock().unwrap();
            if let Some((k, _)) = b.opens.iter().find(|(_, o)| &o.stateid == stateid) {
                return Some((idx, k.clone()));
            }
        }
        None
    }

    /// CLOSE: release open state. Validates seqid for replay.
    pub fn close(&self, stateid: &StateId, seqid: u32) -> Result<(), NfsError> {
        // Find the open by stateid.
        let found = self.find_open_key(stateid);
        match found {
            Some((idx, k)) => {
                let removed = {
                    let mut b = self.buckets[idx].lock().unwrap();
                    // Re-validate under the bucket lock: may have been closed
                    // concurrently between the scan and now.
                    let o = match b.opens.get(&k) {
                        Some(o) if &o.stateid == stateid => o,
                        _ => return Err(NfsError::Status(NFS4ERR_EXPIRED)),
                    };
                    if seqid != o.seqid + 1 {
                        // Replay or bad seqid.
                        if seqid == o.seqid {
                            return Ok(()); // Replay: already closed, return success.
                        }
                        return Err(NfsError::Status(NFS4ERR_BAD_SEQID));
                    }
                    let clientid = o.clientid;
                    b.opens.remove(&k);
                    // Also remove locks held by this open's owner? No, locks are
                    // separate. But if no opens remain for the file, keep locks
                    // (they're independent in NFSv4).
                    self.renew_lease_in(&mut b, clientid);
                    true
                };
                if removed {
                    self.log_append(StateLogRecord::Close {
                        stateid: stateid.clone(),
                    });
                }
                Ok(())
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// Find an open by stateid. Returns an owned record: with per-bucket
    /// locks there is no single guard that can back a reference.
    pub fn find_open(&self, stateid: &StateId) -> Option<OpenRecord> {
        for bucket in &self.buckets {
            let b = bucket.lock().unwrap();
            if let Some(o) = b.opens.values().find(|o| &o.stateid == stateid) {
                return Some(o.clone());
            }
        }
        None
    }

    /// OPEN_DOWNGRADE: reduce share_access/share_deny. Validates seqid.
    pub fn open_downgrade(
        &self,
        stateid: &StateId,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
    ) -> Result<StateId, NfsError> {
        let found = self.find_open_key(stateid);
        match found {
            Some((idx, k)) => {
                let (sid, downgraded) = {
                    let mut b = self.buckets[idx].lock().unwrap();
                    let o = match b.opens.get_mut(&k) {
                        Some(o) if &o.stateid == stateid => o,
                        _ => return Err(NfsError::Status(NFS4ERR_EXPIRED)),
                    };
                    if seqid != o.seqid + 1 {
                        if seqid == o.seqid {
                            return Ok(o.stateid.clone()); // Replay.
                        }
                        return Err(NfsError::Status(NFS4ERR_BAD_SEQID));
                    }
                    // Downgrade must be a subset of current modes.
                    if (share_access & !o.share_access) != 0 || (share_deny & !o.share_deny) != 0 {
                        return Err(NfsError::Status(NFS4ERR_INVAL));
                    }
                    o.share_access = share_access;
                    o.share_deny = share_deny;
                    o.seqid = seqid;
                    // Bump stateid seqid.
                    o.stateid.seqid += 1;
                    let sid = o.stateid.clone();
                    let clientid = o.clientid;
                    self.renew_lease_in(&mut b, clientid);
                    (sid, true)
                };
                if downgraded {
                    self.log_append(StateLogRecord::OpenDowngrade {
                        stateid: stateid.clone(),
                        share_access,
                        share_deny,
                        seqid,
                    });
                }
                Ok(sid)
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// LOCK: acquire a byte-range lock. Checks conflicts.
    ///
    /// `reclaim` mirrors OPEN's CLAIM_PREVIOUS (RFC 7530 §8.4): only valid
    /// during the grace period, granted without conflict checks and without
    /// open_stateid validation (the server lost the pre-restart stateids).
    /// Non-reclaim locks during grace fail with NFS4ERR_GRACE.
    ///
    /// Like open(), the conflict check scans all buckets under the conflict
    /// lock so racing locks observe a consistent view.
    pub fn lock(
        &self,
        clientid: u64,
        lock_owner: Vec<u8>,
        file_ino: u64,
        locktype: u32,
        offset: u64,
        length: u64,
        open_stateid: Option<&StateId>,
        reclaim: bool,
    ) -> Result<LockRecord, NfsError> {
        self.check_client(clientid)?;
        let rec = if reclaim {
            if !self.in_grace() {
                return Err(NfsError::Status(NFS4ERR_NO_GRACE));
            }
            let mut b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
            let stateid = self.new_stateid();
            let rec = LockRecord {
                clientid,
                owner: lock_owner.clone(),
                file_ino,
                stateid,
                offset,
                length,
                locktype,
                seqid: 0,
            };
            b.locks.insert((clientid, lock_owner), rec.clone());
            self.renew_lease_in(&mut b, clientid);
            rec
        } else {
            if self.in_grace() {
                return Err(NfsError::Status(NFS4ERR_GRACE));
            }
            // Validate open_stateid if provided (new lock owner).
            if let Some(ost) = open_stateid {
                if self.find_open(ost).is_none() {
                    return Err(NfsError::Status(NFS4ERR_EXPIRED));
                }
            }
            let _conflict = self.conflict.lock().unwrap();
            // Check for conflicts with existing locks on the same file.
            for bucket in &self.buckets {
                let b = bucket.lock().unwrap();
                for l in b.locks.values() {
                    if l.file_ino != file_ino {
                        continue;
                    }
                    // Same owner: allow (it's an upgrade/downgrade or overlapping).
                    if l.clientid == clientid && l.owner == lock_owner {
                        continue;
                    }
                    if ranges_overlap(l.offset, l.length, offset, length) {
                        // WRITE lock conflicts with any; READ conflicts with WRITE.
                        let conflict = match (l.locktype, locktype) {
                            (2, _) | (_, 2) => true, // WRITE_LT conflicts
                            _ => false,
                        };
                        if conflict {
                            return Err(NfsError::Status(NFS4ERR_LOCKED));
                        }
                    }
                }
            }
            let mut b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
            let stateid = self.new_stateid();
            let rec = LockRecord {
                clientid,
                owner: lock_owner.clone(),
                file_ino,
                stateid,
                offset,
                length,
                locktype,
                seqid: 0,
            };
            b.locks.insert((clientid, lock_owner), rec.clone());
            self.renew_lease_in(&mut b, clientid);
            rec
        };
        self.log_append(StateLogRecord::Lock {
            clientid: rec.clientid,
            owner: rec.owner.clone(),
            file_ino: rec.file_ino,
            offset: rec.offset,
            length: rec.length,
            locktype: rec.locktype,
            stateid: rec.stateid.clone(),
            seqid: rec.seqid,
        });
        Ok(rec)
    }

    /// Scan all buckets for the lock with this exact stateid.
    fn find_lock_key(&self, stateid: &StateId) -> Option<(usize, (u64, Vec<u8>))> {
        for (idx, bucket) in self.buckets.iter().enumerate() {
            let b = bucket.lock().unwrap();
            if let Some((k, _)) = b.locks.iter().find(|(_, l)| &l.stateid == stateid) {
                return Some((idx, k.clone()));
            }
        }
        None
    }

    /// LOCKU: release a byte-range lock.
    pub fn unlock(
        &self,
        stateid: &StateId,
        seqid: u32,
        offset: u64,
        length: u64,
    ) -> Result<StateId, NfsError> {
        let found = self.find_lock_key(stateid);
        match found {
            Some((idx, k)) => {
                let (sid, unlocked) = {
                    let mut b = self.buckets[idx].lock().unwrap();
                    let l = match b.locks.get(&k) {
                        Some(l) if &l.stateid == stateid => l,
                        _ => return Err(NfsError::Status(NFS4ERR_EXPIRED)),
                    };
                    if seqid != l.seqid + 1 {
                        if seqid == l.seqid {
                            return Ok(l.stateid.clone()); // Replay.
                        }
                        return Err(NfsError::Status(NFS4ERR_BAD_SEQID));
                    }
                    // For simplicity, remove the whole lock (not partial).
                    // P6: full unlock only; partial unlock is an edge case.
                    let _ = (offset, length);
                    let mut sid = l.stateid.clone();
                    sid.seqid += 1;
                    let clientid = l.clientid;
                    b.locks.remove(&k);
                    self.renew_lease_in(&mut b, clientid);
                    (sid, true)
                };
                if unlocked {
                    self.log_append(StateLogRecord::Unlock {
                        stateid: stateid.clone(),
                        offset,
                        length,
                        seqid,
                    });
                }
                Ok(sid)
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// Check if a client has any state (for testing).
    pub fn client_has_state(&self, clientid: u64) -> bool {
        let b = self.buckets[Self::bucket_idx(clientid)].lock().unwrap();
        b.opens.keys().any(|(cid, _)| *cid == clientid)
            || b.locks.keys().any(|(cid, _)| *cid == clientid)
    }

    /// Total number of registered clients (for tests/metrics).
    pub fn client_count(&self) -> usize {
        self.buckets
            .iter()
            .map(|bucket| bucket.lock().unwrap().clients.len())
            .sum()
    }

    /// Set the admission-control cap (§4.1 step 5). 0 = unlimited.
    pub fn set_max_clients(&self, max: usize) {
        self.max_clients
            .store(max, std::sync::atomic::Ordering::Relaxed);
    }

    /// The admission-control cap (0 = unlimited).
    pub fn max_clients(&self) -> usize {
        self.max_clients.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True if the server is at its client cap (admission control).
    pub fn at_client_cap(&self) -> bool {
        let max = self.max_clients();
        max > 0 && self.client_count() >= max
    }

    /// Attach a persistent state WAL (§4.4 step 6). From this point, every
    /// log_append is also fsynced to the WAL.
    pub fn set_wal(&self, wal: crate::state_wal::StateWal) {
        *self.wal.lock().unwrap() = Some(wal);
    }
}

fn ranges_overlap(off1: u64, len1: u64, off2: u64, len2: u64) -> bool {
    let end1 = if len1 == u64::MAX {
        u64::MAX
    } else {
        off1.saturating_add(len1)
    };
    let end2 = if len2 == u64::MAX {
        u64::MAX
    } else {
        off2.saturating_add(len2)
    };
    off1 < end2 && off2 < end1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lease_expiry_reaps_state() {
        let sm = StateManager::with_lease(Duration::from_millis(100));
        let (cid, _) = sm.setclientid([1u8; 8], b"test".to_vec()).unwrap();
        assert!(sm.confirm(cid, [1u8; 8]));
        // Create an open.
        sm.open(1, cid, b"owner".to_vec(), 1, 3, 0, false).unwrap();
        assert!(sm.client_has_state(cid));
        // Wait for lease to expire.
        std::thread::sleep(Duration::from_millis(150));
        sm.reap_expired();
        // State should be gone.
        assert!(!sm.client_has_state(cid));
        // Open should fail with stale clientid.
        assert!(sm.open(1, cid, b"owner2".to_vec(), 1, 3, 0, false).is_err());
    }

    #[test]
    fn share_deny_conflict() {
        let sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        // c1 opens with DENY_WRITE.
        sm.open(1, cid1, b"o1".to_vec(), 1, 3, 2, false).unwrap();
        // c2 tries to open for WRITE — should be denied.
        let res = sm.open(1, cid2, b"o2".to_vec(), 1, 2, 0, false);
        assert!(res.is_err());
    }

    #[test]
    fn lock_conflict() {
        let sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        let o1 = sm.open(1, cid1, b"o1".to_vec(), 1, 3, 0, false).unwrap();
        let o2 = sm.open(1, cid2, b"o2".to_vec(), 1, 3, 0, false).unwrap();
        // c1 locks [0,1000) WRITE.
        sm.lock(
            cid1,
            b"l1".to_vec(),
            1,
            2,
            0,
            1000,
            Some(&o1.stateid),
            false,
        )
        .unwrap();
        // c2 tries overlapping [500,1500) — should fail.
        let res = sm.lock(
            cid2,
            b"l2".to_vec(),
            1,
            2,
            500,
            1000,
            Some(&o2.stateid),
            false,
        );
        assert!(res.is_err());
        // c2 locks non-overlapping [2000,3000) — should succeed.
        sm.lock(
            cid2,
            b"l2".to_vec(),
            1,
            2,
            2000,
            1000,
            Some(&o2.stateid),
            false,
        )
        .unwrap();
    }

    // --- Server-qualified ids (docs/v40-state-partitioning.md §4.2) ---

    #[test]
    fn server_id_qualifies_clientids() {
        let sm = StateManager::with_server_id(0x1234_5678);
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        // High 32 bits carry the server id; low 32 bits are the sequence.
        assert_eq!(StateManager::clientid_server_id(cid1), 0x1234_5678);
        assert_eq!(StateManager::clientid_server_id(cid2), 0x1234_5678);
        assert_eq!(cid1 & 0xffff_ffff, 1);
        assert_eq!(cid2 & 0xffff_ffff, 2);
        assert_eq!(cid1 >> 32, 0x1234_5678);
    }

    #[test]
    fn default_server_id_preserves_legacy_layout() {
        // Server id 0 keeps the old wire shape: clientids are 1, 2, 3...
        let sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        assert_eq!(cid1, 1);
        assert_eq!(cid2, 2);
    }

    #[test]
    fn two_servers_never_issue_same_clientid() {
        // The failover hazard: a standby must never reissue the dead
        // primary's clientids.
        let primary = StateManager::with_server_id(7);
        let standby = StateManager::with_server_id(8);
        let mut seen = std::collections::HashSet::new();
        for i in 0..100 {
            let (c1, _) = primary
                .setclientid([1u8; 8], format!("p{i}").into_bytes())
                .unwrap();
            let (c2, _) = standby
                .setclientid([1u8; 8], format!("s{i}").into_bytes())
                .unwrap();
            assert!(seen.insert(c1), "primary reissued clientid {c1:#x}");
            assert!(
                seen.insert(c2),
                "standby aliased primary's clientid {c2:#x}"
            );
        }
    }

    #[test]
    fn stateid_carries_server_id_and_boot_gen() {
        let sm = StateManager::with_server_id(42);
        let (cid, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        sm.confirm(cid, [1u8; 8]);
        let o = sm.open(1, cid, b"o1".to_vec(), 1, 3, 0, false).unwrap();
        assert_eq!(StateManager::stateid_server_id(&o.stateid), 42);
        assert!(sm.owns_stateid(&o.stateid));
        // A stateid from another server is not ours.
        let other = StateManager::with_server_id(43);
        assert!(!other.owns_stateid(&o.stateid));
    }

    #[test]
    fn stateid_does_not_alias_across_boots() {
        // Two boots of the same server id get different boot generations,
        // so a pre-restart stateid can never equal a post-restart one.
        // (With overwhelming probability; /dev/urandom-backed.)
        let boot1 = StateManager::with_server_id(9);
        let boot2 = StateManager::with_server_id(9);
        let (cid1, _) = boot1.setclientid([1u8; 8], b"c".to_vec()).unwrap();
        let (cid2, _) = boot2.setclientid([1u8; 8], b"c".to_vec()).unwrap();
        boot1.confirm(cid1, [1u8; 8]);
        boot2.confirm(cid2, [1u8; 8]);
        let o1 = boot1.open(1, cid1, b"o".to_vec(), 1, 3, 0, false).unwrap();
        let o2 = boot2.open(1, cid2, b"o".to_vec(), 1, 3, 0, false).unwrap();
        assert_ne!(
            StateManager::stateid_boot_gen(&o1.stateid),
            StateManager::stateid_boot_gen(&o2.stateid),
            "boot generations collided"
        );
        assert_ne!(o1.stateid, o2.stateid);
        assert!(boot1.owns_stateid(&o1.stateid));
        assert!(!boot1.owns_stateid(&o2.stateid));
        assert!(!boot2.owns_stateid(&o1.stateid));
    }

    #[test]
    fn set_server_id_rejected_after_registration() {
        let sm = StateManager::new();
        sm.set_server_id(5);
        assert_eq!(sm.server_id(), 5);
        let (cid, _) = sm.setclientid([1u8; 8], b"c".to_vec()).unwrap();
        assert_eq!(StateManager::clientid_server_id(cid), 5);
        // Changing the id after ids were issued would break the
        // no-aliasing invariant: must panic.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sm.set_server_id(6);
        }));
        assert!(r.is_err());
    }

    // --- Bucketed concurrency (no global state lock) ---

    #[test]
    fn concurrent_disjoint_clients_no_deadlock() {
        use std::sync::{Arc, Barrier};
        let sm = Arc::new(StateManager::with_server_id(1));
        let n = 32usize;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = vec![];
        for i in 0..n {
            let sm = sm.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let v = [i as u8; 8];
                let (cid, _) = sm
                    .setclientid(v, format!("client-{i}").into_bytes())
                    .unwrap();
                assert!(sm.confirm(cid, v));
                // Each client opens/closes its own files: no conflicts expected.
                for j in 0..20u64 {
                    let rec = sm
                        .open(
                            1,
                            cid,
                            format!("owner-{j}").into_bytes(),
                            1000 + j,
                            3,
                            0,
                            false,
                        )
                        .expect("disjoint open failed");
                    sm.close(&rec.stateid, 2).expect("close failed");
                }
                assert!(sm.renew(cid));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(sm.client_count(), n);
    }

    #[test]
    fn concurrent_conflicting_opens_exactly_one_wins() {
        // Two clients racing to OPEN the same file with mutually conflicting
        // share modes: the cross-bucket conflict check must stay exact, so
        // exactly one wins and the other gets DENIED.
        use std::sync::{Arc, Barrier};
        let sm = Arc::new(StateManager::new());
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        // Sanity: the two clients land in different buckets.
        assert_ne!(cid1 & 63, cid2 & 63);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = vec![];
        for cid in [cid1, cid2] {
            let sm = sm.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                // OPEN for WRITE with DENY_WRITE: conflicts with any writer.
                sm.open(1, cid, b"o".to_vec(), 777, 2, 2, false)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let oks = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one conflicting open must win, got {results:?}"
        );
    }

    #[test]
    fn setclientid_reestablish_drops_old_state() {
        // Same name, new verifier: new incarnation keeps the id but drops
        // old opens/locks, and the name index stays consistent.
        let sm = StateManager::with_lease(Duration::from_secs(3600));
        let (cid1, _) = sm.setclientid([1u8; 8], b"cli".to_vec()).unwrap();
        sm.confirm(cid1, [1u8; 8]);
        let rec = sm.open(1, cid1, b"o".to_vec(), 5, 3, 0, false).unwrap();
        assert!(sm.client_has_state(cid1));
        let (cid2, _) = sm.setclientid([2u8; 8], b"cli".to_vec()).unwrap();
        assert_eq!(cid1, cid2);
        assert!(!sm.client_has_state(cid2));
        assert!(sm.find_open(&rec.stateid).is_none());
        assert!(sm.confirm(cid2, [2u8; 8]));
    }

    // --- Grace period / reclaim (RFC 7530 §8.4) ---

    fn confirmed_client(sm: &StateManager, name: &[u8]) -> u64 {
        let (cid, _) = sm.setclientid([9u8; 8], name.to_vec()).unwrap();
        assert!(sm.confirm(cid, [9u8; 8]));
        cid
    }

    #[test]
    fn grace_reclaim_open_accepted() {
        let sm = StateManager::new();
        sm.enter_grace_period();
        assert!(sm.in_grace());
        let cid = confirmed_client(&sm, b"c1");
        // Reclaim during grace: granted.
        let rec = sm.open(1, cid, b"o".to_vec(), 7, 3, 0, true).unwrap();
        assert_eq!(rec.file_ino, 7);
        assert!(sm.client_has_state(cid));
    }

    #[test]
    fn grace_new_open_rejected() {
        let sm = StateManager::new();
        sm.enter_grace_period();
        let cid = confirmed_client(&sm, b"c1");
        let err = sm.open(1, cid, b"o".to_vec(), 7, 3, 0, false).unwrap_err();
        assert_eq!(err, NfsError::Status(NFS4ERR_GRACE));
    }

    #[test]
    fn no_grace_reclaim_open_rejected() {
        let sm = StateManager::new();
        assert!(!sm.in_grace());
        let cid = confirmed_client(&sm, b"c1");
        let err = sm.open(1, cid, b"o".to_vec(), 7, 3, 0, true).unwrap_err();
        assert_eq!(err, NfsError::Status(NFS4ERR_NO_GRACE));
    }

    #[test]
    fn grace_expiry_resumes_normal_opens() {
        let sm = StateManager::new();
        sm.enter_grace_period_for(Duration::from_millis(50));
        let cid = confirmed_client(&sm, b"c1");
        assert!(sm.open(1, cid, b"o".to_vec(), 7, 3, 0, false).is_err());
        std::thread::sleep(Duration::from_millis(80));
        assert!(!sm.in_grace());
        // Grace over: normal open works, reclaim is rejected.
        sm.open(1, cid, b"o".to_vec(), 7, 3, 0, false).unwrap();
        let err = sm.open(1, cid, b"o2".to_vec(), 8, 3, 0, true).unwrap_err();
        assert_eq!(err, NfsError::Status(NFS4ERR_NO_GRACE));
    }

    #[test]
    fn grace_reclaim_skips_share_conflicts() {
        // Post-restart there is nothing to conflict with: two clients
        // reclaiming mutually conflicting opens both succeed.
        let sm = StateManager::new();
        sm.enter_grace_period();
        let cid1 = confirmed_client(&sm, b"c1");
        let cid2 = confirmed_client(&sm, b"c2");
        sm.open(1, cid1, b"o1".to_vec(), 7, 2, 2, true).unwrap();
        sm.open(1, cid2, b"o2".to_vec(), 7, 2, 2, true).unwrap();
    }

    #[test]
    fn grace_lock_reclaim_rules() {
        let sm = StateManager::new();
        sm.enter_grace_period();
        let cid = confirmed_client(&sm, b"c1");
        // Reclaim lock during grace: granted without open_stateid validation
        // (pre-restart stateids are unresolvable).
        sm.lock(cid, b"l".to_vec(), 7, 2, 0, 100, None, true)
            .unwrap();
        // New lock during grace: rejected.
        let err = sm
            .lock(cid, b"l2".to_vec(), 7, 2, 200, 100, None, false)
            .unwrap_err();
        assert_eq!(err, NfsError::Status(NFS4ERR_GRACE));
        sm.exit_grace_period();
        // Reclaim after grace: rejected.
        let err = sm
            .lock(cid, b"l3".to_vec(), 7, 2, 300, 100, None, true)
            .unwrap_err();
        assert_eq!(err, NfsError::Status(NFS4ERR_NO_GRACE));
    }

    #[test]
    fn admission_control_caps_new_clients() {
        use crate::nfs4::NFS4ERR_DELAY;
        use std::time::Duration;
        let sm = StateManager::with_lease(Duration::from_millis(50));
        sm.set_max_clients(2);
        assert!(!sm.at_client_cap());
        // Two fresh registrations succeed.
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec()).unwrap();
        let (_cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec()).unwrap();
        assert!(sm.at_client_cap());
        assert_eq!(sm.client_count(), 2);
        // Third fresh client gets DELAY.
        let err = sm.setclientid([3u8; 8], b"c3".to_vec()).unwrap_err();
        assert_eq!(err, NFS4ERR_DELAY);
        // Re-establishment of a known client is still allowed at the cap.
        let (cid1b, _) = sm.setclientid([9u8; 8], b"c1".to_vec()).unwrap();
        assert_eq!(cid1b, cid1);
        // After leases expire, a new client fits again.
        std::thread::sleep(Duration::from_millis(100));
        let (cid3, _) = sm.setclientid([3u8; 8], b"c3".to_vec()).unwrap();
        assert_ne!(cid3, cid1);
    }

    #[test]
    fn admission_control_unlimited_by_default() {
        let sm = StateManager::new();
        assert_eq!(sm.max_clients(), 0);
        assert!(!sm.at_client_cap());
        for i in 0..100 {
            sm.setclientid([i as u8; 8], format!("c{i}").into_bytes())
                .unwrap();
        }
        assert_eq!(sm.client_count(), 100);
    }
}
