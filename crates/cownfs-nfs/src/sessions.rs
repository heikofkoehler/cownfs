//! NFSv4.1 session state (RFC 5661).
//!
//! Sessions give exactly-once semantics: each slot has a sequence number
//! and a cached reply. A SEQUENCE with `sequenceid == slot.sequence`
//! is a replay — return the cached reply instead of re-executing.
//! `sequenceid == slot.sequence + 1` is a new request. Anything else is
//! NFS4ERR_SEQ_MISORDERED.
//!
//! This module owns the client/session/slot tables. The per-compound
//! execution logic lives in `server.rs`.

use std::collections::HashMap;

use crate::nfs4::OpResult;

/// One session slot: the last executed sequence and its cached reply.
pub struct Slot {
    pub sequence: u32,
    pub cached: Option<Vec<OpResult>>,
}

impl Slot {
    fn new() -> Self {
        Slot {
            sequence: 0,
            cached: None,
        }
    }
}

/// An NFSv4.1 session: an ID plus a slot table.
pub struct Session41 {
    pub id: [u8; 16],
    pub client_id: u64,
    pub slots: Vec<Slot>,
    /// Highest slot ID the client may use (== slots.len() - 1).
    pub highest_slot: u32,
}

/// A client established via EXCHANGE_ID.
pub struct Client41 {
    pub id: u64,
    pub verifier: [u8; 8],
    pub owner: Vec<u8>,
    /// Sequence for CREATE_SESSION (increments per attempt).
    pub create_seq: u32,
    pub session_ids: Vec<[u8; 16]>,
}

/// Global v4.1 client/session table, shared across connections
/// (sessions survive connection loss — that's the point of trunking).
pub struct SessionTable {
    next_client_id: u64,
    next_session_seq: u64,
    clients: HashMap<u64, Client41>,
    /// Owner key -> client ID, for EXCHANGE_ID idempotence.
    by_owner: HashMap<Vec<u8>, u64>,
    sessions: HashMap<[u8; 16], Session41>,
}

impl SessionTable {
    pub fn new() -> Self {
        SessionTable {
            next_client_id: 1,
            next_session_seq: 1,
            clients: HashMap::new(),
            by_owner: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    /// EXCHANGE_ID: return the client ID for (owner, verifier).
    /// Same owner + same verifier -> same ID (client restart detection
    /// would use a new verifier; we treat any verifier change as a new
    /// incarnation and drop the old client's sessions).
    pub fn exchange_id(&mut self, verifier: [u8; 8], owner: Vec<u8>) -> (u64, u32) {
        if let Some(&cid) = self.by_owner.get(&owner) {
            let c = self.clients.get_mut(&cid).unwrap();
            if c.verifier == verifier {
                return (cid, c.create_seq);
            }
            // Verifier changed: client restarted. Drop old state.
            for sid in c.session_ids.drain(..) {
                self.sessions.remove(&sid);
            }
            c.verifier = verifier;
            c.create_seq = 0;
            return (cid, 0);
        }
        let cid = self.next_client_id;
        self.next_client_id += 1;
        self.clients.insert(
            cid,
            Client41 {
                id: cid,
                verifier,
                owner: owner.clone(),
                create_seq: 0,
                session_ids: Vec::new(),
            },
        );
        self.by_owner.insert(owner, cid);
        (cid, 0)
    }

    /// CREATE_SESSION: allocate a session with `max_slots` slots.
    /// Returns the session ID, or None if the client/sequence is bad.
    pub fn create_session(
        &mut self,
        clientid: u64,
        sequence: u32,
        max_slots: u32,
    ) -> Option<[u8; 16]> {
        let c = self.clients.get_mut(&clientid)?;
        // Sequence must advance (simple guard against replays).
        if sequence != c.create_seq.wrapping_add(1) && !(c.create_seq == 0 && sequence == 1) {
            // Allow the first CREATE_SESSION (sequence 1) only.
            if c.create_seq != 0 {
                return None;
            }
        }
        c.create_seq = sequence;

        let nslots = max_slots.clamp(1, 64) as usize;
        let seq = self.next_session_seq;
        self.next_session_seq += 1;
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&clientid.to_be_bytes());
        id[8..].copy_from_slice(&seq.to_be_bytes());

        let mut slots = Vec::with_capacity(nslots);
        for _ in 0..nslots {
            slots.push(Slot::new());
        }
        self.sessions.insert(
            id,
            Session41 {
                id,
                client_id: clientid,
                slots,
                highest_slot: (nslots - 1) as u32,
            },
        );
        self.clients
            .get_mut(&clientid)
            .unwrap()
            .session_ids
            .push(id);
        Some(id)
    }

    pub fn get_session(&self, id: &[u8; 16]) -> Option<&Session41> {
        self.sessions.get(id)
    }

    pub fn get_session_mut(&mut self, id: &[u8; 16]) -> Option<&mut Session41> {
        self.sessions.get_mut(id)
    }

    /// DESTROY_SESSION: remove the session and its slot table.
    pub fn destroy_session(&mut self, id: &[u8; 16]) -> bool {
        if let Some(s) = self.sessions.remove(id) {
            if let Some(c) = self.clients.get_mut(&s.client_id) {
                c.session_ids.retain(|x| x != id);
            }
            true
        } else {
            false
        }
    }

    /// DESTROY_CLIENTID: remove the client and all its sessions.
    pub fn destroy_client(&mut self, clientid: u64) -> bool {
        if let Some(c) = self.clients.remove(&clientid) {
            self.by_owner.remove(&c.owner);
            for sid in c.session_ids {
                self.sessions.remove(&sid);
            }
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_id_is_stable() {
        let mut t = SessionTable::new();
        let (a, _) = t.exchange_id([1; 8], b"client-a".to_vec());
        let (b, _) = t.exchange_id([1; 8], b"client-a".to_vec());
        assert_eq!(a, b);
        let (c, _) = t.exchange_id([1; 8], b"client-b".to_vec());
        assert_ne!(a, c);
    }

    #[test]
    fn verifier_change_resets_client() {
        let mut t = SessionTable::new();
        let (a, _) = t.exchange_id([1; 8], b"client-a".to_vec());
        let sid = t.create_session(a, 1, 4).unwrap();
        assert!(t.get_session(&sid).is_some());
        // New verifier: same client ID, sessions gone.
        let (a2, _) = t.exchange_id([2; 8], b"client-a".to_vec());
        assert_eq!(a, a2);
        assert!(t.get_session(&sid).is_none());
    }

    #[test]
    fn session_lifecycle() {
        let mut t = SessionTable::new();
        let (cid, _) = t.exchange_id([1; 8], b"c".to_vec());
        let sid = t.create_session(cid, 1, 8).unwrap();
        let s = t.get_session(&sid).unwrap();
        assert_eq!(s.slots.len(), 8);
        assert_eq!(s.highest_slot, 7);
        assert!(t.destroy_session(&sid));
        assert!(t.get_session(&sid).is_none());
        assert!(t.destroy_client(cid));
    }
}
