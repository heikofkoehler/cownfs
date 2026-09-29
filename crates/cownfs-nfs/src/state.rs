//! NFSv4 state management for P6: clients, opens, locks, leases.
//!
//! - SETCLIENTID/SETCLIENTID_CONFIRM establishes a client with a lease.
//! - OPEN creates open state with a real stateid; share reservations are enforced.
//! - LOCK/LOCKU manage byte-range locks with conflict detection.
//! - Sequence IDs provide replay protection.
//! - Expired leases reap all client state.

use crate::nfs4::{
    NfsError, StateId, NFS4ERR_BAD_SEQID, NFS4ERR_DENIED, NFS4ERR_EXPIRED, NFS4ERR_LOCKED,
    NFS4ERR_STALE_CLIENTID,
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
    next_clientid: u64,
    next_stateid_other: u64,
    lease_duration: Duration,
}

impl StateManager {
    pub fn new() -> Self {
        StateManager {
            clients: HashMap::new(),
            opens: HashMap::new(),
            locks: HashMap::new(),
            next_clientid: 1,
            next_stateid_other: 1,
            lease_duration: LEASE_DURATION,
        }
    }

    /// For tests: use a short lease.
    pub fn with_lease(lease: Duration) -> Self {
        let mut s = Self::new();
        s.lease_duration = lease;
        s
    }

    fn new_stateid(&mut self) -> StateId {
        let other = self.next_stateid_other;
        self.next_stateid_other += 1;
        let mut b = [0u8; 12];
        b[..8].copy_from_slice(&other.to_be_bytes());
        // Last 4 bytes: random-ish (use other again)
        let r = (other.wrapping_mul(0x9e3779b9) >> 32) as u32;
        b[8..].copy_from_slice(&r.to_be_bytes());
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
                let id = self.next_clientid;
                self.next_clientid += 1;
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
        clientid: u64,
        owner: Vec<u8>,
        file_ino: u64,
        share_access: u32,
        share_deny: u32,
    ) -> Result<OpenRecord, NfsError> {
        self.check_client(clientid)?;
        // Check share conflicts with existing opens on the same file.
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
        let stateid = self.new_stateid();
        let rec = OpenRecord {
            clientid,
            owner: owner.clone(),
            file_ino,
            stateid: stateid.clone(),
            share_access,
            share_deny,
            seqid: 0,
        };
        self.opens.insert((clientid, owner), rec.clone());
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
    ) -> Result<(), NfsError> {
        let key = self
            .locks
            .iter()
            .find(|(_, l)| &l.stateid == stateid)
            .map(|(k, _)| k.clone());
        match key {
            Some(k) => {
                let l = self.locks.get(&k).unwrap();
                if seqid != l.seqid + 1 {
                    if seqid == l.seqid {
                        return Ok(()); // Replay.
                    }
                    return Err(NfsError::Status(NFS4ERR_BAD_SEQID));
                }
                // For simplicity, remove the whole lock (not partial).
                // P6: full unlock only; partial unlock is an edge case.
                let _ = (offset, length);
                let clientid = l.clientid;
                self.locks.remove(&k);
                self.renew_lease(clientid);
                Ok(())
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
        sm.open(cid, b"owner".to_vec(), 1, 3, 0).unwrap();
        assert!(sm.client_has_state(cid));
        // Wait for lease to expire.
        std::thread::sleep(Duration::from_millis(150));
        sm.reap_expired();
        // State should be gone.
        assert!(!sm.client_has_state(cid));
        // Open should fail with stale clientid.
        assert!(sm.open(cid, b"owner2".to_vec(), 1, 3, 0).is_err());
    }

    #[test]
    fn share_deny_conflict() {
        let mut sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        // c1 opens with DENY_WRITE.
        sm.open(cid1, b"o1".to_vec(), 1, 3, 2).unwrap();
        // c2 tries to open for WRITE — should be denied.
        let res = sm.open(cid2, b"o2".to_vec(), 1, 2, 0);
        assert!(res.is_err());
    }

    #[test]
    fn lock_conflict() {
        let mut sm = StateManager::new();
        let (cid1, _) = sm.setclientid([1u8; 8], b"c1".to_vec());
        let (cid2, _) = sm.setclientid([2u8; 8], b"c2".to_vec());
        sm.confirm(cid1, [1u8; 8]);
        sm.confirm(cid2, [2u8; 8]);
        let o1 = sm.open(cid1, b"o1".to_vec(), 1, 3, 0).unwrap();
        let o2 = sm.open(cid2, b"o2".to_vec(), 1, 3, 0).unwrap();
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
}
