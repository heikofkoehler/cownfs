//! NFSv4 state management for P6: clients, opens, locks, leases.
//!
//! - SETCLIENTID/SETCLIENTID_CONFIRM establishes a client with a lease.
//! - OPEN creates open state with a real stateid; share reservations are enforced.
//! - LOCK/LOCKU manage byte-range locks with conflict detection.
//! - Sequence IDs provide replay protection.
//! - Expired leases reap all client state.

use crate::nfs4::{
    NfsError, StateId, NFS4ERR_BAD_SEQID, NFS4ERR_DENIED, NFS4ERR_EXPIRED, NFS4ERR_INVAL,
    NFS4ERR_LOCKED, NFS4ERR_STALE_CLIENTID,
};
use std::collections::HashMap;
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

pub struct StateManager {
    clients: HashMap<u64, ClientRecord>,
    opens: HashMap<(u64, Vec<u8>), OpenRecord>,
    locks: HashMap<(u64, Vec<u8>), LockRecord>,
    /// Unique per shard primary (see docs/v40-state-partitioning.md §4.2).
    /// Occupies the high 32 bits of every clientid this server issues, so
    /// two servers never issue the same clientid and a standby never
    /// reissues a dead primary's ids.
    server_id: u32,
    /// Random per process boot; embedded in every stateid's `other` field
    /// so a stateid minted before a restart can never alias one minted
    /// after (defense for the future state-log/WAL replay path).
    boot_gen: u32,
    /// Low 32 bits of the next clientid. Never 0 (0 clientid = invalid).
    next_client_seq: u32,
    next_stateid_seq: u32,
    lease_duration: Duration,
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
        StateManager {
            clients: HashMap::new(),
            opens: HashMap::new(),
            locks: HashMap::new(),
            server_id,
            boot_gen: random_boot_gen(),
            next_client_seq: 1,
            next_stateid_seq: 1,
            lease_duration: LEASE_DURATION,
        }
    }

    /// Change the server id. Must be called before the server starts
    /// accepting clients (no issued ids may exist yet).
    pub fn set_server_id(&mut self, server_id: u32) {
        assert!(
            self.clients.is_empty(),
            "set_server_id after clients were registered"
        );
        self.server_id = server_id;
    }

    /// The server id this manager qualifies ids with.
    pub fn server_id(&self) -> u32 {
        self.server_id
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
        Self::stateid_server_id(stateid) == self.server_id
            && Self::stateid_boot_gen(stateid) == self.boot_gen
    }

    /// For tests: use a short lease.
    pub fn with_lease(lease: Duration) -> Self {
        let mut s = Self::new();
        s.lease_duration = lease;
        s
    }

    fn new_stateid(&mut self) -> StateId {
        let seq = self.next_stateid_seq;
        // Skip 0 on wrap; 0 is not special on the wire, this just keeps
        // the sequence dense and avoids reusing the initial value.
        self.next_stateid_seq = seq.wrapping_add(1).max(1);
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&self.server_id.to_be_bytes());
        b[4..8].copy_from_slice(&self.boot_gen.to_be_bytes());
        b[8..12].copy_from_slice(&seq.to_be_bytes());
        StateId { seqid: 0, other: b }
    }

    /// Reap expired clients and their state. Call before stateful ops.
    pub fn reap_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.lease_expiry < now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.clients.remove(&id);
            self.opens.retain(|(cid, _), _| *cid != id);
            self.locks.retain(|(cid, _), _| *cid != id);
        }
    }

    fn check_client(&mut self, clientid: u64) -> Result<(), NfsError> {
        self.reap_expired();
        match self.clients.get(&clientid) {
            Some(c) if c.confirmed => Ok(()),
            Some(_) => Err(NfsError::Status(NFS4ERR_STALE_CLIENTID)),
            None => Err(NfsError::Status(NFS4ERR_STALE_CLIENTID)),
        }
    }

    fn renew_lease(&mut self, clientid: u64) {
        if let Some(c) = self.clients.get_mut(&clientid) {
            c.lease_expiry = Instant::now() + self.lease_duration;
        }
    }

    /// SETCLIENTID: register or update a client. Returns (clientid, confirmed).
    pub fn setclientid(&mut self, verifier: [u8; 8], name: Vec<u8>) -> (u64, bool) {
        self.reap_expired();
        // Look for existing client with same name.
        let existing = self
            .clients
            .iter()
            .find(|(_, c)| c.name == name)
            .map(|(id, _)| *id);
        match existing {
            Some(id) => {
                let c = self.clients.get_mut(&id).unwrap();
                if c.verifier == verifier {
                    // Same client, restart. Keep id, mark unconfirmed.
                    c.confirmed = false;
                    c.lease_expiry = Instant::now() + self.lease_duration;
                    (id, false)
                } else {
                    // Different verifier: new incarnation, drop old state.
                    self.opens.retain(|(cid, _), _| *cid != id);
                    self.locks.retain(|(cid, _), _| *cid != id);
                    let c = self.clients.get_mut(&id).unwrap();
                    c.verifier = verifier;
                    c.confirmed = false;
                    c.lease_expiry = Instant::now() + self.lease_duration;
                    (id, false)
                }
            }
            None => {
                // Server-qualified clientid: high 32 bits = server id, low
                // 32 bits = local sequence. Two servers never issue the
                // same clientid; low bits never 0 (0 = invalid clientid).
                let seq = self.next_client_seq;
                self.next_client_seq = seq.wrapping_add(1).max(1);
                let id = ((self.server_id as u64) << 32) | (seq as u64);
                self.clients.insert(
                    id,
                    ClientRecord {
                        verifier,
                        name,
                        confirmed: false,
                        lease_expiry: Instant::now() + self.lease_duration,
                    },
                );
                (id, false)
            }
        }
    }

    /// SETCLIENTID_CONFIRM: confirm a client. Returns true if confirmed.
    pub fn confirm(&mut self, clientid: u64, verifier: [u8; 8]) -> bool {
        match self.clients.get_mut(&clientid) {
            Some(c) if c.verifier == verifier => {
                c.confirmed = true;
                c.lease_expiry = Instant::now() + self.lease_duration;
                true
            }
            _ => false,
        }
    }

    /// RENEW: renew the lease. Returns true if client exists.
    pub fn renew(&mut self, clientid: u64) -> bool {
        self.reap_expired();
        match self.clients.get_mut(&clientid) {
            Some(c) => {
                c.lease_expiry = Instant::now() + self.lease_duration;
                true
            }
            None => false,
        }
    }

    /// OPEN: create open state. Checks share reservations.
    /// Returns the OpenRecord or an NFS error.
    pub fn open(
        &mut self,
        seqid: u32,
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
    ) -> Result<OpenRecord, NfsError> {
        self.check_client(clientid)?;
        // Check share conflicts with existing opens on the same file
        // (excluding our own opens from the same client).
        for (_, o) in self.opens.iter() {
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
        let key = (clientid, owner.clone());
        // If the same open_owner reopens the same file, merge share modes
        // (upgrade) instead of creating a duplicate. Accept any seqid for
        // the upgrade to be lenient with client seqid tracking.
        if let Some(existing) = self.opens.get_mut(&key) {
            if existing.file_ino == file_ino {
                existing.share_access |= share_access;
                existing.share_deny |= share_deny;
                existing.seqid = seqid;
                existing.stateid.seqid += 1;
                let rec = existing.clone();
                self.renew_lease(clientid);
                return Ok(rec);
            }
        }
        let stateid = self.new_stateid();
        let rec = OpenRecord {
            clientid,
            owner: owner.clone(),
            file_ino,
            stateid: stateid.clone(),
            share_access,
            share_deny,
            seqid,
        };
        self.opens.insert(key, rec.clone());
        self.renew_lease(clientid);
        Ok(rec)
    }

    /// CLOSE: release open state. Validates seqid for replay.
    pub fn close(&mut self, stateid: &StateId, seqid: u32) -> Result<(), NfsError> {
        // Find the open by stateid.
        let key = self
            .opens
            .iter()
            .find(|(_, o)| &o.stateid == stateid)
            .map(|(k, _)| k.clone());
        match key {
            Some(k) => {
                let o = self.opens.get(&k).unwrap();
                if seqid != o.seqid + 1 {
                    // Replay or bad seqid.
                    if seqid == o.seqid {
                        return Ok(()); // Replay: already closed, return success.
                    }
                    return Err(NfsError::Status(NFS4ERR_BAD_SEQID));
                }
                let clientid = o.clientid;
                self.opens.remove(&k);
                // Also remove locks held by this open's owner? No, locks are
                // separate. But if no opens remain for the file, keep locks
                // (they're independent in NFSv4).
                self.renew_lease(clientid);
                Ok(())
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// Find an open by stateid.
    pub fn find_open(&self, stateid: &StateId) -> Option<&OpenRecord> {
        self.opens.values().find(|o| &o.stateid == stateid)
    }

    /// OPEN_DOWNGRADE: reduce share_access/share_deny. Validates seqid.
    pub fn open_downgrade(
        &mut self,
        stateid: &StateId,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
    ) -> Result<StateId, NfsError> {
        let key = self
            .opens
            .iter()
            .find(|(_, o)| &o.stateid == stateid)
            .map(|(k, _)| k.clone());
        match key {
            Some(k) => {
                let (sid, clientid) = {
                    let o = self.opens.get_mut(&k).unwrap();
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
                    (o.stateid.clone(), o.clientid)
                };
                self.renew_lease(clientid);
                Ok(sid)
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// LOCK: acquire a byte-range lock. Checks conflicts.
    pub fn lock(
        &mut self,
        clientid: u64,
        lock_owner: Vec<u8>,
        file_ino: u64,
        locktype: u32,
        offset: u64,
        length: u64,
        open_stateid: Option<&StateId>,
    ) -> Result<LockRecord, NfsError> {
        self.check_client(clientid)?;
        // Validate open_stateid if provided (new lock owner).
        if let Some(ost) = open_stateid {
            if self.find_open(ost).is_none() {
                return Err(NfsError::Status(NFS4ERR_EXPIRED));
            }
        }
        // Check for conflicts with existing locks on the same file.
        for (_, l) in self.locks.iter() {
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
        let stateid = self.new_stateid();
        let rec = LockRecord {
            clientid,
            owner: lock_owner.clone(),
            file_ino,
            stateid: stateid.clone(),
            offset,
            length,
            locktype,
            seqid: 0,
        };
        self.locks.insert((clientid, lock_owner), rec.clone());
        self.renew_lease(clientid);
        Ok(rec)
    }

    /// LOCKU: release a byte-range lock.
    pub fn unlock(
        &mut self,
        stateid: &StateId,
        seqid: u32,
        offset: u64,
        length: u64,
    ) -> Result<StateId, NfsError> {
        let key = self
            .locks
            .iter()
            .find(|(_, l)| &l.stateid == stateid)
            .map(|(k, _)| k.clone());
        match key {
            Some(k) => {
                let (sid, clientid) = {
                    let l = self.locks.get(&k).unwrap();
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
                    (sid, l.clientid)
                };
                self.locks.remove(&k);
                self.renew_lease(clientid);
                Ok(sid)
            }
            None => Err(NfsError::Status(NFS4ERR_EXPIRED)),
        }
    }

    /// Check if a client has any state (for testing).
    pub fn client_has_state(&self, clientid: u64) -> bool {
        self.opens.keys().any(|(cid, _)| *cid == clientid)
            || self.locks.keys().any(|(cid, _)| *cid == clientid)
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
        let mut sm = StateManager::with_lease(Duration::from_millis(100));
        let (cid, _) = sm.setclientid([1u8; 8], b"test".to_vec());
        assert!(sm.confirm(cid, [1u8; 8]));
        // Create an open.
        sm.open(1, cid, b"owner".to_vec(), 1, 3, 0).unwrap();
        assert!(sm.client_has_state(cid));
        // Wait for lease to expire.
        std::thread::sleep(Duration::from_millis(150));
        sm.reap_expired();
        // State should be gone.
        assert!(!sm.client_has_state(cid));
        // Open should fail with stale clientid.
        assert!(sm.open(1, cid, b"owner2".to_vec(), 1, 3, 0).is_err());
    }

    #[test]
    fn share_deny_conflict() {
        let mut sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        // c1 opens with DENY_WRITE.
        sm.open(1, cid1, b"o1".to_vec(), 1, 3, 2).unwrap();
        // c2 tries to open for WRITE — should be denied.
        let res = sm.open(1, cid2, b"o2".to_vec(), 1, 2, 0);
        assert!(res.is_err());
    }

    #[test]
    fn lock_conflict() {
        let mut sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        let o1 = sm.open(1, cid1, b"o1".to_vec(), 1, 3, 0).unwrap();
        let o2 = sm.open(1, cid2, b"o2".to_vec(), 1, 3, 0).unwrap();
        // c1 locks [0,1000) WRITE.
        sm.lock(cid1, b"l1".to_vec(), 1, 2, 0, 1000, Some(&o1.stateid))
            .unwrap();
        // c2 tries overlapping [500,1500) — should fail.
        let res = sm.lock(cid2, b"l2".to_vec(), 1, 2, 500, 1000, Some(&o2.stateid));
        assert!(res.is_err());
        // c2 locks non-overlapping [2000,3000) — should succeed.
        sm.lock(cid2, b"l2".to_vec(), 1, 2, 2000, 1000, Some(&o2.stateid))
            .unwrap();
    }

    // --- Server-qualified ids (docs/v40-state-partitioning.md §4.2) ---

    #[test]
    fn server_id_qualifies_clientids() {
        let mut sm = StateManager::with_server_id(0x1234_5678);
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
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
        let mut sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
        assert_eq!(cid1, 1);
        assert_eq!(cid2, 2);
    }

    #[test]
    fn two_servers_never_issue_same_clientid() {
        // The failover hazard: a standby must never reissue the dead
        // primary's clientids.
        let mut primary = StateManager::with_server_id(7);
        let mut standby = StateManager::with_server_id(8);
        let mut seen = std::collections::HashSet::new();
        for i in 0..100 {
            let (c1, _) = primary.setclientid([1u8; 8], format!("p{i}").into_bytes());
            let (c2, _) = standby.setclientid([1u8; 8], format!("s{i}").into_bytes());
            assert!(seen.insert(c1), "primary reissued clientid {c1:#x}");
            assert!(
                seen.insert(c2),
                "standby aliased primary's clientid {c2:#x}"
            );
        }
    }

    #[test]
    fn stateid_carries_server_id_and_boot_gen() {
        let mut sm = StateManager::with_server_id(42);
        let (cid, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        sm.confirm(cid, [1u8; 8]);
        let o = sm.open(1, cid, b"o1".to_vec(), 1, 3, 0).unwrap();
        assert_eq!(StateManager::stateid_server_id(&o.stateid), 42);
        assert!(sm.owns_stateid(&o.stateid));
        // A stateid from another server is not ours.
        let mut other = StateManager::with_server_id(43);
        assert!(!other.owns_stateid(&o.stateid));
    }

    #[test]
    fn stateid_does_not_alias_across_boots() {
        // Two boots of the same server id get different boot generations,
        // so a pre-restart stateid can never equal a post-restart one.
        // (With overwhelming probability; /dev/urandom-backed.)
        let mut boot1 = StateManager::with_server_id(9);
        let mut boot2 = StateManager::with_server_id(9);
        let (cid1, _) = boot1.setclientid([1u8; 8], b"c".to_vec());
        let (cid2, _) = boot2.setclientid([1u8; 8], b"c".to_vec());
        boot1.confirm(cid1, [1u8; 8]);
        boot2.confirm(cid2, [1u8; 8]);
        let o1 = boot1.open(1, cid1, b"o".to_vec(), 1, 3, 0).unwrap();
        let o2 = boot2.open(1, cid2, b"o".to_vec(), 1, 3, 0).unwrap();
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
        let mut sm = StateManager::new();
        sm.set_server_id(5);
        assert_eq!(sm.server_id(), 5);
        let (cid, _) = sm.setclientid([1u8; 8], b"c".to_vec());
        assert_eq!(StateManager::clientid_server_id(cid), 5);
        // Changing the id after ids were issued would break the
        // no-aliasing invariant: must panic.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sm.set_server_id(6);
        }));
        assert!(r.is_err());
    }
}
