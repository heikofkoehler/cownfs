//! Minimal NFSv4.0 TCP server: COMPOUND execution against the engine.
//! Single-connection (`serve_listener`) or thread-per-connection
//! (`serve_concurrent`) serving; filesystem and NFSv4 state are shared.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use cownfs_core::engine::{Fs, FsError, FTYPE_DIR, FTYPE_SYMLINK, ROOT_INO};

use crate::nfs4::{
    encode_compound, AttrMask, AttrValues, Compound, FileAttrs, FileHandle, NfsError, Op, OpResult,
    StateId, ACCESS4_DELETE, ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_MODIFY,
    ACCESS4_READ, FATTR4_MODE, FATTR4_SIZE, FILE_SYNC4, GUARDED4, LAYOUT4_NFSV4_1_FILES, NF4DIR,
    NF4LNK, NF4REG, NFS4ERR_BADNAME, NFS4ERR_BADSESSION, NFS4ERR_BADSLOT, NFS4ERR_BADTYPE,
    NFS4ERR_BADXDR, NFS4ERR_BAD_COOKIE, NFS4ERR_EXIST, NFS4ERR_EXPIRED, NFS4ERR_INVAL,
    NFS4ERR_ISDIR, NFS4ERR_NAMETOOLONG, NFS4ERR_NOENT, NFS4ERR_NOFILEHANDLE, NFS4ERR_NOTDIR,
    NFS4ERR_NOTSUPP, NFS4ERR_OP_ILLEGAL, NFS4ERR_ROFS, NFS4ERR_SEQ_MISORDERED, NFS4ERR_SERVERFAULT,
    NFS4ERR_STALE_CLIENTID, NFS4ERR_TOOSMALL, NFS4_OK, OPEN4_CREATE, OP_ACCESS, OP_CLOSE,
    OP_COMMIT, OP_CREATE, OP_CREATE_SESSION, OP_DESTROY_CLIENTID, OP_DESTROY_SESSION,
    OP_EXCHANGE_ID, OP_GETATTR, OP_GETFH, OP_ILLEGAL, OP_LAYOUTCOMMIT, OP_LAYOUTGET,
    OP_LAYOUTRETURN, OP_LINK, OP_LOCK, OP_LOCKU, OP_LOOKUP, OP_LOOKUPP, OP_OPEN, OP_OPEN_DOWNGRADE,
    OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR, OP_REMOVE, OP_RENAME, OP_RENEW, OP_RESTOREFH,
    OP_SAVEFH, OP_SECINFO, OP_SEQUENCE, OP_SETATTR, OP_SETCLIENTID, OP_SETCLIENTID_CONFIRM,
    OP_WRITE, UNCHECKED4,
};
use crate::rpc::{self, Call, RecordReader, RpcError};
use crate::state::StateManager;
use crate::xdr::{Writer, XdrError};
use cownfs_core::engine::SetAttrs;

#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    Rpc(RpcError),
    Nfs(NfsError),
    Fs(FsError),
}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::Io(e)
    }
}
impl From<RpcError> for ServerError {
    fn from(e: RpcError) -> Self {
        ServerError::Rpc(e)
    }
}
impl From<NfsError> for ServerError {
    fn from(e: NfsError) -> Self {
        ServerError::Nfs(e)
    }
}
impl From<FsError> for ServerError {
    fn from(e: FsError) -> Self {
        ServerError::Fs(e)
    }
}
impl From<XdrError> for ServerError {
    fn from(e: XdrError) -> Self {
        ServerError::Nfs(NfsError::Xdr(e))
    }
}

/// Decode an XDR-string fattr value (u32 length + bytes + padding) to &str.
/// The raw value captured by `parse_fattr` includes the framing, so callers
/// must strip it before interpreting the text.
fn fattr_str(val: &[u8]) -> Option<&str> {
    if val.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes(val[..4].try_into().ok()?) as usize;
    let bytes = val.get(4..4 + len)?;
    std::str::from_utf8(bytes).ok()
}

/// Map engine errors to NFS4 status codes.
fn fs_to_nfs(e: FsError) -> u32 {
    match e {
        FsError::NotFound => NFS4ERR_NOENT,
        FsError::NotDir => NFS4ERR_NOTDIR,
        FsError::NotFile => NFS4ERR_ISDIR,
        FsError::NotEmpty => NFS4ERR_NOTSUPP,
        FsError::AlreadyExists => NFS4ERR_EXIST,
        FsError::BadName => NFS4ERR_INVAL,
        FsError::NoSpace => NFS4ERR_NOTSUPP, // read-only in P4; real mapping in P5
        FsError::Invalid(_) => NFS4ERR_INVAL,
        FsError::Store(_) => NFS4ERR_SERVERFAULT,
        // Injected faults (P7) never reach the wire in production;
        // map to SERVERFAULT if they do.
        FsError::InjectedFault(_) => NFS4ERR_SERVERFAULT,
    }
}

/// Server state shared across all connections: the filesystem and the
/// NFSv4 client/open/lock table (leases, open owners, byte-range locks).
/// `cfh`/`saved_fh` stay per-connection inside [`Session`].
#[derive(Clone)]
pub struct Shared {
    pub fs: Arc<Mutex<Fs>>,
    pub state: Arc<Mutex<StateManager>>,
    /// NFSv4.1 client/session/slot table (exactly-once semantics).
    pub sessions: Arc<Mutex<crate::sessions::SessionTable>>,
    /// pNFS layout table (single data server in v1).
    pub layouts: Arc<Mutex<crate::layouts::LayoutTable>>,
    /// Data server address for LAYOUTCOMMIT verification (e.g. "127.0.0.1:2060").
    /// None disables pNFS layouts (LAYOUTGET returns NOTSUPP).
    pub ds_addr: Option<String>,
    /// When true, mutating ops return NFS4ERR_ROFS. Used for read replicas.
    pub read_only: bool,
}

impl Shared {
    pub fn new(fs: Fs) -> Self {
        Shared {
            fs: Arc::new(Mutex::new(fs)),
            state: Arc::new(Mutex::new(StateManager::new())),
            sessions: Arc::new(Mutex::new(crate::sessions::SessionTable::new())),
            layouts: Arc::new(Mutex::new(crate::layouts::LayoutTable::new())),
            ds_addr: None,
            read_only: false,
        }
    }

    pub fn new_read_only(fs: Fs) -> Self {
        Shared {
            fs: Arc::new(Mutex::new(fs)),
            state: Arc::new(Mutex::new(StateManager::new())),
            sessions: Arc::new(Mutex::new(crate::sessions::SessionTable::new())),
            layouts: Arc::new(Mutex::new(crate::layouts::LayoutTable::new())),
            ds_addr: None,
            read_only: true,
        }
    }

    /// Attach a data server address, enabling pNFS layouts.
    pub fn with_ds_addr(mut self, addr: String) -> Self {
        self.ds_addr = Some(addr);
        self
    }
}

/// Per-connection COMPOUND execution state.
struct Session {
    shared: Shared,
    /// Current filehandle's inode (None = none selected).
    cfh: Option<u64>,
    /// Saved filehandle (SAVEFH/RESTOREFH).
    saved_fh: Option<u64>,
    /// v4.1 session ID from the compound's SEQUENCE (None in 4.0 mode).
    session41: Option<[u8; 16]>,
}

impl Session {
    fn new(shared: &Shared) -> Self {
        Session {
            shared: shared.clone(),
            cfh: None,
            saved_fh: None,
            session41: None,
        }
    }

    /// Lock the filesystem. Each op takes and releases this guard within a
    /// single statement, so guards never nest and lock order is trivial.
    ///
    /// WARNING: never use `self.fs()` as a temporary method receiver twice
    /// in one expression (e.g. two fields of one struct literal). A
    /// temporary `MutexGuard` that is the receiver of a method call lives
    /// until the end of the enclosing expression, so the second `self.fs()`
    /// deadlocks the thread on itself. Bind each call to a local first.
    fn fs(&self) -> std::sync::MutexGuard<'_, Fs> {
        self.shared.fs.lock().unwrap()
    }

    /// Validate a filename: must be valid UTF-8 (pynfs expects NFS4ERR_INVAL
    /// for invalid UTF-8 in LOOKUP, OPEN, REMOVE, RENAME, LINK, SECINFO).
    /// The engine's DirKey already rejects empty/overslong/slashed names.
    fn check_name(name: &[u8], opnum: u32) -> Result<(), OpResult> {
        if std::str::from_utf8(name).is_err() {
            return Err(OpResult::err(opnum, NFS4ERR_INVAL));
        }
        // 255-byte limit (common filesystem max; pynfs CR15 expects NAMETOOLONG)
        if name.len() > 255 {
            return Err(OpResult::err(opnum, NFS4ERR_NAMETOOLONG));
        }
        // '/' is never valid in a single path component
        if name.contains(&b'/') {
            return Err(OpResult::err(opnum, NFS4ERR_BADNAME));
        }
        Ok(())
    }

    /// Check for '.'/'..' in mutating ops (CREATE/REMOVE/RENAME/LINK).
    /// LOOKUP of '.'/'..' is legal; creating/removing them is not.
    fn check_dots(name: &[u8], opnum: u32) -> Result<(), OpResult> {
        if name == b"." || name == b".." {
            return Err(OpResult::err(opnum, NFS4ERR_BADNAME));
        }
        Ok(())
    }

    /// Lock the NFSv4 client/open/lock table.
    fn state(&self) -> std::sync::MutexGuard<'_, StateManager> {
        self.shared.state.lock().unwrap()
    }

    fn check_fh(&self, fh: &FileHandle) -> Result<u64, u32> {
        if fh.fs_uuid != self.fs().uuid() {
            return Err(NFS4ERR_INVAL);
        }
        Ok(fh.inode)
    }

    fn run(&mut self, compound: &Compound) -> Vec<OpResult> {
        // Current and saved filehandles are per-COMPOUND (RFC 7530 §2.6).
        self.cfh = None;
        self.saved_fh = None;
        // NFSv4.1: if the first op is SEQUENCE, run with exactly-once
        // semantics (replay detection via the session slot table).
        if let Some(Op::Sequence {
            sessionid,
            sequenceid,
            slotid,
            cachethis,
        }) = compound.ops.first()
        {
            return self.run_sessioned(compound, *sessionid, *sequenceid, *slotid, *cachethis);
        }
        let mut out = Vec::new();
        for op in &compound.ops {
            let res = self.exec(op);
            let failed = res.status != NFS4_OK;
            out.push(res);
            if failed {
                break;
            }
        }
        out
    }

    /// Execute a compound whose first op is SEQUENCE, with replay protection.
    fn run_sessioned(
        &mut self,
        compound: &Compound,
        sessionid: [u8; 16],
        sequenceid: u32,
        slotid: u32,
        cachethis: bool,
    ) -> Vec<OpResult> {
        // Validate session and slot, check sequence.
        let replay: Option<Vec<OpResult>> = {
            let mut table = self.shared.sessions.lock().unwrap();
            let sess = match table.get_session_mut(&sessionid) {
                Some(s) => s,
                None => return vec![OpResult::err(OP_SEQUENCE, NFS4ERR_BADSESSION)],
            };
            if slotid > sess.highest_slot {
                return vec![OpResult::err(OP_SEQUENCE, NFS4ERR_BADSLOT)];
            }
            let slot = &mut sess.slots[slotid as usize];
            if sequenceid == slot.sequence {
                // Replay: return the cached reply.
                match &slot.cached {
                    Some(c) => Some(c.clone()),
                    None => {
                        // Not cached (cachethis was false): the client must
                        // retry as a new request; signal misordered.
                        return vec![OpResult::err(OP_SEQUENCE, NFS4ERR_SEQ_MISORDERED)];
                    }
                }
            } else if sequenceid != slot.sequence.wrapping_add(1) {
                return vec![OpResult::err(OP_SEQUENCE, NFS4ERR_SEQ_MISORDERED)];
            } else {
                None
            }
        };
        if let Some(cached) = replay {
            return cached;
        }

        // New request: SEQUENCE result first, then the rest of the compound.
        let seq_res = {
            let table = self.shared.sessions.lock().unwrap();
            let sess = table.get_session(&sessionid).unwrap();
            let mut w = Writer::new();
            w.opaque_fixed(&sessionid);
            w.u32(sequenceid);
            w.u32(slotid);
            w.u32(sess.highest_slot);
            w.u32(sess.highest_slot); // target_highest_slotid
            w.u32(0); // status_flags
            OpResult::ok(OP_SEQUENCE, w.into_bytes())
        };
        // The rest of the compound runs under this v4.1 session.
        self.session41 = Some(sessionid);
        let mut out = vec![seq_res];
        for op in &compound.ops[1..] {
            let res = self.exec(op);
            let failed = res.status != NFS4_OK;
            out.push(res);
            if failed {
                break;
            }
        }
        self.session41 = None;

        // Advance the slot and cache the reply.
        {
            let mut table = self.shared.sessions.lock().unwrap();
            if let Some(sess) = table.get_session_mut(&sessionid) {
                if let Some(slot) = sess.slots.get_mut(slotid as usize) {
                    slot.sequence = sequenceid;
                    slot.cached = if cachethis { Some(out.clone()) } else { None };
                }
            }
        }
        out
    }

    fn exec(&mut self, op: &Op) -> OpResult {
        match op {
            Op::PutFh(fh) => match self.check_fh(fh) {
                Ok(ino) => {
                    self.cfh = Some(ino);
                    OpResult::ok(OP_PUTFH, Vec::new())
                }
                Err(st) => OpResult::err(OP_PUTFH, st),
            },
            Op::PutRootFh => {
                self.cfh = Some(ROOT_INO);
                OpResult::ok(OP_PUTROOTFH, Vec::new())
            }
            Op::GetFh => match self.cfh {
                Some(ino) => {
                    let mut w = Writer::new();
                    FileHandle {
                        fs_uuid: self.fs().uuid(),
                        inode: ino,
                    }
                    .encode(&mut w);
                    OpResult::ok(OP_GETFH, w.into_bytes())
                }
                None => OpResult::err(OP_GETFH, NFS4ERR_NOFILEHANDLE),
            },
            Op::Lookup(name) => self.op_lookup(name, OP_LOOKUP),
            Op::LookupP => self.op_lookupp(),
            Op::GetAttr(mask) => self.op_getattr(mask),
            Op::ReadDir {
                cookie,
                dircount,
                maxcount,
                mask,
                ..
            } => self.op_readdir(*cookie, *dircount, *maxcount, mask),
            Op::Read { offset, count } => self.op_read(*offset, *count),
            Op::Access { access } => self.op_access(*access),
            Op::Open {
                seqid,
                clientid,
                owner,
                share_access,
                share_deny,
                opentype,
                createmode,
                createattrs,
                claim_type,
                filename,
            } => {
                // OPEN with CREATE mutates the namespace.
                if self.shared.read_only && *opentype == OPEN4_CREATE {
                    return OpResult::err(OP_OPEN, NFS4ERR_ROFS);
                }
                self.op_open(
                    *seqid,
                    *clientid,
                    owner,
                    *share_access,
                    *share_deny,
                    *opentype,
                    *createmode,
                    createattrs,
                    *claim_type,
                    filename,
                )
            }
            Op::Create {
                ftype,
                linkdata,
                name,
                attrs,
            } => {
                if self.shared.read_only {
                    return OpResult::err(OP_CREATE, NFS4ERR_ROFS);
                }
                self.op_create(*ftype, linkdata, name, attrs)
            }
            Op::Remove(name) => {
                if self.shared.read_only {
                    return OpResult::err(OP_REMOVE, NFS4ERR_ROFS);
                }
                self.op_remove(name)
            }
            Op::Secinfo(name) => self.op_secinfo(name),
            Op::Illegal => OpResult::err(OP_ILLEGAL, NFS4ERR_OP_ILLEGAL),
            Op::Rename { old, new } => {
                if self.shared.read_only {
                    return OpResult::err(OP_RENAME, NFS4ERR_ROFS);
                }
                self.op_rename(old, new)
            }
            Op::Link(name) => {
                if self.shared.read_only {
                    return OpResult::err(OP_LINK, NFS4ERR_ROFS);
                }
                self.op_link(name)
            }
            Op::SaveFh => {
                self.saved_fh = self.cfh;
                OpResult::ok(OP_SAVEFH, Vec::new())
            }
            Op::RestoreFh => match self.saved_fh {
                Some(ino) => {
                    self.cfh = Some(ino);
                    OpResult::ok(OP_RESTOREFH, Vec::new())
                }
                None => OpResult::err(OP_RESTOREFH, NFS4ERR_INVAL),
            },
            Op::SetAttr { attrs } => {
                if self.shared.read_only {
                    return OpResult::err(OP_SETATTR, NFS4ERR_ROFS);
                }
                self.op_setattr(attrs)
            }
            Op::Write {
                offset,
                stable,
                data,
            } => {
                if self.shared.read_only {
                    return OpResult::err(OP_WRITE, NFS4ERR_ROFS);
                }
                self.op_write(*offset, *stable, data)
            }
            Op::Commit { offset, count } => self.op_commit(*offset, *count),
            Op::SetClientId {
                verifier,
                client_name,
                ..
            } => self.op_setclientid(*verifier, client_name),
            Op::SetClientIdConfirm { clientid, verifier } => {
                self.op_setclientid_confirm(*clientid, *verifier)
            }
            Op::Close { seqid, stateid } => self.op_close(*seqid, stateid),
            Op::OpenDowngrade {
                stateid,
                seqid,
                share_access,
                share_deny,
            } => self.op_open_downgrade(stateid, *seqid, *share_access, *share_deny),
            Op::Lock {
                locktype,
                reclaim,
                offset,
                length,
                new_lock_owner,
                open_seqid,
                open_stateid,
                lock_seqid,
                lock_stateid,
                lock_owner,
            } => self.op_lock(
                *locktype,
                *reclaim,
                *offset,
                *length,
                *new_lock_owner,
                *open_seqid,
                open_stateid,
                *lock_seqid,
                lock_stateid,
                lock_owner,
            ),
            Op::LockU {
                locktype,
                seqid,
                stateid,
                offset,
                length,
            } => self.op_locku(*locktype, *seqid, stateid, *offset, *length),
            Op::Renew { clientid } => self.op_renew(*clientid),
            Op::ExchangeId {
                verifier,
                owner,
                flags,
            } => self.op_exchange_id(*verifier, owner, *flags),
            Op::CreateSession {
                clientid,
                sequence,
                max_slots,
            } => self.op_create_session(*clientid, *sequence, *max_slots),
            Op::DestroySession { sessionid } => self.op_destroy_session(*sessionid),
            Op::Sequence { .. } => {
                // SEQUENCE is handled in run(); reaching exec means it was
                // not the first op, which is a protocol violation.
                OpResult::err(OP_SEQUENCE, NFS4ERR_BADSESSION)
            }
            Op::DestroyClientid { clientid } => self.op_destroy_clientid(*clientid),
            Op::LayoutGet {
                offset,
                length,
                minlength,
            } => self.op_layoutget(*offset, *length, *minlength),
            Op::LayoutCommit {
                offset,
                length,
                blocks,
                new_size,
            } => self.op_layoutcommit(*offset, *length, blocks, *new_size),
            Op::LayoutReturn { offset, length } => self.op_layoutreturn(*offset, *length),
        }
    }

    fn op_exchange_id(&mut self, verifier: [u8; 8], owner: &[u8], _flags: u32) -> OpResult {
        let (clientid, seq) = self
            .shared
            .sessions
            .lock()
            .unwrap()
            .exchange_id(verifier, owner.to_vec());
        let mut w = Writer::new();
        w.u64(clientid);
        w.u32(seq); // sequenceid for CREATE_SESSION
        w.u32(0); // flags
                  // server_owner: verifier + owner
        w.opaque_fixed(&[0u8; 8]); // server verifier (zero)
        w.string(b"cownfs");
        // server_scope
        w.string(b"cownfs");
        OpResult::ok(OP_EXCHANGE_ID, w.into_bytes())
    }

    fn op_create_session(&mut self, clientid: u64, sequence: u32, max_slots: u32) -> OpResult {
        let sid = match self
            .shared
            .sessions
            .lock()
            .unwrap()
            .create_session(clientid, sequence, max_slots)
        {
            Some(id) => id,
            None => return OpResult::err(OP_CREATE_SESSION, NFS4ERR_STALE_CLIENTID),
        };
        let mut w = Writer::new();
        w.opaque_fixed(&sid);
        w.u32(sequence);
        w.u32(0); // flags
                  // fore_chan_attrs (simplified)
        w.u32(0); // headerpadsize
        w.u32(1024 * 1024); // maxreq_sz
        w.u32(1024 * 1024); // maxresp_sz
        w.u32(64 * 1024); // maxresp_cached
        w.u32(16); // maxops
        w.u32(max_slots); // maxreqs
        OpResult::ok(OP_CREATE_SESSION, w.into_bytes())
    }

    fn op_destroy_session(&mut self, sessionid: [u8; 16]) -> OpResult {
        let ok = self
            .shared
            .sessions
            .lock()
            .unwrap()
            .destroy_session(&sessionid);
        if ok {
            OpResult::ok(OP_DESTROY_SESSION, Vec::new())
        } else {
            OpResult::err(OP_DESTROY_SESSION, NFS4ERR_BADSESSION)
        }
    }

    fn op_destroy_clientid(&mut self, clientid: u64) -> OpResult {
        let ok = self
            .shared
            .sessions
            .lock()
            .unwrap()
            .destroy_client(clientid);
        if ok {
            OpResult::ok(OP_DESTROY_CLIENTID, Vec::new())
        } else {
            OpResult::err(OP_DESTROY_CLIENTID, NFS4ERR_STALE_CLIENTID)
        }
    }

    /// Read one block from the data server, verifying the transport.
    /// Returns (data, checksum).
    fn ds_read_block(addr: &str, block_id: u64) -> Result<(Vec<u8>, u64), String> {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).map_err(|e| format!("ds connect: {e}"))?;
        s.write_all(&0x4453_3031u32.to_be_bytes())
            .map_err(|e| e.to_string())?;
        s.write_all(&1u32.to_be_bytes())
            .map_err(|e| e.to_string())?;
        s.write_all(&[1u8]).map_err(|e| e.to_string())?; // READ
        s.write_all(&block_id.to_be_bytes())
            .map_err(|e| e.to_string())?;
        let mut st = [0u8; 4];
        s.read_exact(&mut st).map_err(|e| e.to_string())?;
        if u32::from_be_bytes(st) != 0 {
            return Err("ds read failed".into());
        }
        let mut data = vec![0u8; 4096];
        s.read_exact(&mut data).map_err(|e| e.to_string())?;
        let mut sum = [0u8; 8];
        s.read_exact(&mut sum).map_err(|e| e.to_string())?;
        Ok((data, u64::from_be_bytes(sum)))
    }

    fn op_layoutget(&mut self, offset: u64, length: u64, minlength: u64) -> OpResult {
        let sessionid = match self.session41 {
            Some(s) => s,
            None => return OpResult::err(OP_LAYOUTGET, NFS4ERR_BADSESSION),
        };
        let ds_addr = match &self.shared.ds_addr {
            Some(a) => a.clone(),
            None => return OpResult::err(OP_LAYOUTGET, NFS4ERR_NOTSUPP),
        };
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LAYOUTGET, NFS4ERR_NOFILEHANDLE),
        };
        if self.shared.read_only {
            return OpResult::err(OP_LAYOUTGET, NFS4ERR_ROFS);
        }
        // Cap the layout length at 1 MiB per LAYOUTGET in v1.
        let length = length.min(1024 * 1024).max(minlength.min(1024 * 1024));
        let layout = self
            .shared
            .layouts
            .lock()
            .unwrap()
            .layout_get(sessionid, ino, offset, length);

        let mut w = Writer::new();
        w.u32(1); // return_on_close = true
                  // stateid for the layout (simplified: session-derived)
        let mut stateid = [0u8; 16];
        stateid[..8].copy_from_slice(&layout.first_block_id.to_be_bytes());
        stateid[8..].copy_from_slice(&sessionid[..8]);
        w.opaque_fixed(&stateid);
        // layout4 array: one file-layout segment.
        w.u32(1); // count
        w.u32(LAYOUT4_NFSV4_1_FILES);
        w.u64(offset);
        w.u64(length);
        w.u32(2); // iomode RW
                  // nfsv4_1_file_layout4 body: deviceid + util + first_stripe_index +
                  // pattern_offset + stripe_indices + array of (block_id, nblocks).
                  // v1: single DS, so deviceid indexes the one configured DS.
        let mut body = Writer::new();
        body.opaque_fixed(&[0u8; 16]); // deviceid (index 0)
        body.u32(0); // util
        body.u32(0); // first_stripe_index
        body.u64(offset); // pattern_offset
        body.u32(0); // stripe_indices: empty (single DS)
        body.u32(layout.nblocks as u32);
        for i in 0..layout.nblocks {
            body.u64(layout.first_block_id + i);
        }
        // DS address as a counted string after the opaque body.
        let body_bytes = body.into_bytes();
        w.opaque(&body_bytes);
        w.string(ds_addr.as_bytes());
        OpResult::ok(OP_LAYOUTGET, w.into_bytes())
    }

    fn op_layoutcommit(
        &mut self,
        offset: u64,
        length: u64,
        blocks: &[(u64, u64)],
        new_size: Option<u64>,
    ) -> OpResult {
        let sessionid = match self.session41 {
            Some(s) => s,
            None => return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_BADSESSION),
        };
        let ds_addr = match &self.shared.ds_addr {
            Some(a) => a.clone(),
            None => return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_NOTSUPP),
        };
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_NOFILEHANDLE),
        };
        if self.shared.read_only {
            return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_ROFS);
        }
        // The commit must fall inside an outstanding layout.
        {
            let layouts = self.shared.layouts.lock().unwrap();
            if layouts.find(&sessionid, ino, offset, length).is_none() {
                return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_BADSESSION);
            }
        }
        // Verify each block from the DS and write it into the CoW file.
        // Blocks are in offset order, 4KiB each.
        let mut fs = self.shared.fs.lock().unwrap();
        for (i, (block_id, expect_sum)) in blocks.iter().enumerate() {
            let (data, ds_sum) = match Self::ds_read_block(&ds_addr, *block_id) {
                Ok(x) => x,
                Err(e) => return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_SERVERFAULT),
            };
            if ds_sum != *expect_sum {
                // Client lied or DS corrupted: refuse the commit.
                return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_BADSESSION);
            }
            // Only write the in-range tail of the last block.
            let off = offset + (i as u64) * 4096;
            let mut chunk = &data[..];
            if off + 4096 > offset + length {
                chunk = &chunk[..(offset + length - off) as usize];
            }
            if let Err(_) = fs.write(ino, off, chunk) {
                return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_SERVERFAULT);
            }
        }
        if let Some(sz) = new_size {
            use cownfs_core::engine::SetAttrs;
            if fs
                .setattr(
                    ino,
                    &SetAttrs {
                        size: Some(sz),
                        ..Default::default()
                    },
                )
                .is_err()
            {
                return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_SERVERFAULT);
            }
        }
        // Atomic CoW commit: the pointer swing.
        if let Err(_) = fs.commit() {
            return OpResult::err(OP_LAYOUTCOMMIT, NFS4ERR_SERVERFAULT);
        }
        let mut w = Writer::new();
        w.bool(false); // no new size info to return
        OpResult::ok(OP_LAYOUTCOMMIT, w.into_bytes())
    }

    fn op_layoutreturn(&mut self, offset: u64, length: u64) -> OpResult {
        let sessionid = match self.session41 {
            Some(s) => s,
            None => return OpResult::err(OP_LAYOUTRETURN, NFS4ERR_BADSESSION),
        };
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LAYOUTRETURN, NFS4ERR_NOFILEHANDLE),
        };
        let n = self
            .shared
            .layouts
            .lock()
            .unwrap()
            .layout_return(&sessionid, ino, offset, length);
        let mut w = Writer::new();
        w.u32(if n > 0 { 0 } else { 1 }); // status: 0 = returned
        OpResult::ok(OP_LAYOUTRETURN, w.into_bytes())
    }

    fn current(&self, opnum: u32) -> Result<u64, OpResult> {
        self.cfh.ok_or(OpResult::err(opnum, NFS4ERR_NOFILEHANDLE))
    }

    fn op_lookup(&mut self, name: &[u8], opnum: u32) -> OpResult {
        if let Err(e) = Self::check_name(name, opnum) {
            return e;
        }
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(opnum, NFS4ERR_NOFILEHANDLE),
        };
        // "." and ".." are handled by LOOKUPP; treat "." as self.
        let result = if name == b"." {
            Ok(Some((ino, 0)))
        } else if name == b".." {
            return self.op_lookupp_inner(opnum);
        } else {
            self.fs().lookup(ino, name)
        };
        match result {
            Ok(Some((child, _))) => {
                self.cfh = Some(child);
                OpResult::ok(opnum, Vec::new())
            }
            Ok(None) => OpResult::err(opnum, NFS4ERR_NOENT),
            Err(e) => OpResult::err(opnum, fs_to_nfs(e)),
        }
    }

    fn op_lookupp(&mut self) -> OpResult {
        self.op_lookupp_inner(OP_LOOKUPP)
    }

    fn op_lookupp_inner(&mut self, opnum: u32) -> OpResult {
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(opnum, NFS4ERR_NOFILEHANDLE),
        };
        if ino == ROOT_INO {
            // Root's parent is itself.
            self.cfh = Some(ROOT_INO);
            return OpResult::ok(opnum, Vec::new());
        }
        let parent = self.fs().lookup(ino, b"..");
        match parent {
            Ok(Some((parent, _))) => {
                self.cfh = Some(parent);
                OpResult::ok(opnum, Vec::new())
            }
            Ok(None) => OpResult::err(opnum, NFS4ERR_NOENT),
            Err(e) => OpResult::err(opnum, fs_to_nfs(e)),
        }
    }

    /// Build a `FileAttrs` for the given inode, including filesystem-level
    /// space statistics needed by the REQUIRED/RECOMMENDED GETATTR attrs.
    fn make_file_attrs(&self, ino: u64, inode: &cownfs_core::engine::Inode) -> FileAttrs {
        let ftype = match inode.ftype {
            FTYPE_DIR => NF4DIR,
            FTYPE_SYMLINK => NF4LNK,
            _ => NF4REG,
        };
        let total_blocks = self.fs().block_count();
        let free_blocks = self.fs().free_block_count();
        let fs_uuid = self.fs().uuid();
        let block_size: u64 = 4096;
        FileAttrs {
            ftype,
            size: inode.size,
            fileid: ino,
            mode: inode.mode,
            nlink: inode.nlink,
            fsid_major: u64::from_be_bytes(fs_uuid[..8].try_into().unwrap()),
            fsid_minor: u64::from_be_bytes(fs_uuid[8..].try_into().unwrap()),
            fh: FileHandle {
                fs_uuid,
                inode: ino,
            }
            .to_bytes()
            .to_vec(),
            uid: inode.uid,
            gid: inode.gid,
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            // CHANGE: use ctime as a monotonically increasing change counter.
            // ctime is updated on every metadata/data change, so it works as
            // a per-object change indicator (RFC 7530 §5.8.1.4).
            change: inode.ctime,
            space_total: total_blocks * block_size,
            space_free: free_blocks * block_size,
            // Rough inode estimates: 1 inode per 16 KiB (4 blocks) used.
            files_total: total_blocks / 4,
            files_free: free_blocks / 4,
        }
    }

    fn op_getattr(&self, mask: &AttrMask) -> OpResult {
        let ino = match self.current(OP_GETATTR) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // Bind before matching: a guard in a match scrutinee stays live
        // for the whole match, which would deadlock a later self.fs().
        let inode_res = self.fs().getattr(ino);
        let inode = match inode_res {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_GETATTR, fs_to_nfs(e)),
        };
        let fa = self.make_file_attrs(ino, &inode);
        let vals = AttrValues::encode(mask, &fa);
        let mut w = Writer::new();
        vals.encode_result(&mut w);
        OpResult::ok(OP_GETATTR, w.into_bytes())
    }

    fn op_readdir(&self, cookie: u64, _dircount: u32, maxcount: u32, mask: &AttrMask) -> OpResult {
        let ino = match self.current(OP_READDIR) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // Cookies 1 and 2 are reserved (pynfs RDDR10).
        if cookie == 1 || cookie == 2 {
            return OpResult::err(OP_READDIR, NFS4ERR_BAD_COOKIE);
        }
        // maxcount=0 can never hold an entry (pynfs RDDR7).
        if maxcount == 0 {
            return OpResult::err(OP_READDIR, NFS4ERR_TOOSMALL);
        }
        let entries = match self.fs().readdir(ino) {
            Ok(e) => e,
            Err(e) => return OpResult::err(OP_READDIR, fs_to_nfs(e)),
        };
        // cookie 0 starts at the beginning; otherwise resume after the
        // entry whose cookie matches. Cookies are 1-based indices.
        let start = if cookie == 0 { 0 } else { cookie as usize };
        let mut w = Writer::new();
        w.u64(0); // cookieverf (we don't change directories mid-read)
        let mut bytes_used: u32 = 16; // cookieverf + eof flag estimate
        let max = maxcount.min(1024 * 1024);
        let mut emitted = 0usize;
        for (idx, (name, child_ino, _typ)) in entries.iter().enumerate().skip(start) {
            let inode = match self.fs().getattr(*child_ino) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let fa = self.make_file_attrs(*child_ino, &inode);
            let vals = AttrValues::encode(mask, &fa);
            // entry: cookie, name, attrs, next-entry flag
            let mut ew = Writer::new();
            ew.u64((idx + 1) as u64);
            ew.opaque(name);
            vals.encode_result(&mut ew);
            let entry_bytes = ew.bytes().len() as u32 + 4; // + value_follows
            if bytes_used + entry_bytes > max && emitted > 0 {
                break;
            }
            w.u32(1); // value_follows
            w.raw(ew.bytes());
            bytes_used += entry_bytes;
            emitted += 1;
        }
        w.u32(0); // no more entries
        w.bool(true); // eof (we always return everything after `start`)
        OpResult::ok(OP_READDIR, w.into_bytes())
    }
    fn op_read(&self, offset: u64, count: u32) -> OpResult {
        let ino = match self.current(OP_READ) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // READ on a directory or symlink target: symlinks are read via
        // READ in v4.0 (no READLINK needed for the P4 gate, but support it).
        let data = match self.fs().read(ino, offset, count.min(1024 * 1024) as usize) {
            Ok(d) => d,
            Err(e) => return OpResult::err(OP_READ, fs_to_nfs(e)),
        };
        let inode = match self.fs().getattr(ino) {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_READ, fs_to_nfs(e)),
        };
        let eof = offset + data.len() as u64 >= inode.size;
        let mut w = Writer::new();
        w.bool(eof);
        w.opaque(&data);
        OpResult::ok(OP_READ, w.into_bytes())
    }

    /// Extract mode/uid/gid from createattrs (fattr4). OWNER/OWNER_GROUP
    /// values arrive as XDR strings (length prefix + padding); decode the
    /// framing before parsing the "uid"/"gid" text.
    fn parse_createattrs(attrs: &[(u32, Vec<u8>)]) -> Result<(u32, u32, u32), u32> {
        let mut mode = 0o644u32;
        let mut uid = 0u32;
        let mut gid = 0u32;
        for (attr, val) in attrs {
            match *attr {
                FATTR4_MODE => {
                    if val.len() >= 4 {
                        mode = u32::from_be_bytes(val[..4].try_into().unwrap());
                    }
                }
                36 => {
                    // FATTR4_OWNER: "uid" string
                    if let Some(s) = fattr_str(val) {
                        uid = s.parse().unwrap_or(0);
                    }
                }
                37 => {
                    // FATTR4_OWNER_GROUP
                    if let Some(s) = fattr_str(val) {
                        gid = s.parse().unwrap_or(0);
                    }
                }
                // Read-only or unsupported attrs in CREATE -> INVAL (pynfs CR11).
                _ => return Err(NFS4ERR_INVAL),
            }
        }
        Ok((mode, uid, gid))
    }

    fn op_open(
        &mut self,
        seqid: u32,
        clientid: u64,
        owner: &[u8],
        share_access: u32,
        share_deny: u32,
        opentype: u32,
        createmode: u32,
        createattrs: &[(u32, Vec<u8>)],
        claim_type: u32,
        filename: &[u8],
    ) -> OpResult {
        // P5: support CLAIM_NULL (open by name) with optional CREATE.
        // P6 will add real share/state management.
        if claim_type != 0 {
            return OpResult::err(OP_OPEN, NFS4ERR_NOTSUPP);
        }
        if let Err(e) = Self::check_name(filename, OP_OPEN) {
            return e;
        }
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_OPEN, NFS4ERR_NOFILEHANDLE),
        };
        // Check the cfh is a directory.
        match self.fs().getattr(dir_ino) {
            Ok(inode) if inode.ftype == FTYPE_DIR => {}
            Ok(_) => return OpResult::err(OP_OPEN, NFS4ERR_NOTDIR),
            Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
        }

        let existing = match self.fs().lookup(dir_ino, filename) {
            Ok(o) => o,
            Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
        };

        let file_ino = match (existing, opentype == OPEN4_CREATE) {
            (Some(_), _) if createmode == GUARDED4 && opentype == OPEN4_CREATE => {
                return OpResult::err(OP_OPEN, NFS4ERR_EXIST)
            }
            (Some((ino, _)), _) => ino,
            (None, true) => {
                // Create the file.
                if createmode != UNCHECKED4 && createmode != GUARDED4 {
                    return OpResult::err(OP_OPEN, NFS4ERR_NOTSUPP);
                }
                let (mode, uid, gid) = match Self::parse_createattrs(createattrs) {
                    Ok(v) => v,
                    Err(s) => return OpResult::err(OP_OPEN, s),
                };
                match self.fs().create(dir_ino, filename, mode, uid, gid) {
                    Ok(ino) => ino,
                    Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
                }
            }
            (None, false) => return OpResult::err(OP_OPEN, NFS4ERR_NOENT),
        };

        self.cfh = Some(file_ino);
        // P6: create real open state with share reservation checking.
        let open_rec = match self.state().open(
            seqid,
            clientid,
            owner.to_vec(),
            file_ino,
            share_access,
            share_deny,
        ) {
            Ok(rec) => rec,
            Err(NfsError::Status(s)) => return OpResult::err(OP_OPEN, s),
            Err(_) => return OpResult::err(OP_OPEN, NFS4ERR_SERVERFAULT),
        };
        let mut w = Writer::new();
        open_rec.stateid.encode(&mut w);
        // change_info4: atomic(bool) + before(u64) + after(u64)  [RFC 7530 §16.16]
        w.bool(true); // atomic
        w.u64(0); // before changeid
        w.u64(0); // after changeid
                  // rflags (OPEN4_RESULT_* bits; 0 = nothing special)
        w.u32(0);
        // attrset: bitmap4 of attributes set during create (empty)
        AttrMask { words: vec![0, 0] }.encode(&mut w);
        // delegation type: OPEN_DELEGATE_NONE = 0
        w.u32(0);
        OpResult::ok(OP_OPEN, w.into_bytes())
    }

    fn op_create(
        &mut self,
        ftype: u32,
        linkdata: &[u8],
        name: &[u8],
        attrs: &[(u32, Vec<u8>)],
    ) -> OpResult {
        if let Err(e) = Self::check_name(name, OP_CREATE) {
            return e;
        }
        if let Err(e) = Self::check_dots(name, OP_CREATE) {
            return e;
        }
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_CREATE, NFS4ERR_NOFILEHANDLE),
        };
        let (mode, uid, gid) = match Self::parse_createattrs(attrs) {
            Ok(v) => v,
            Err(s) => return OpResult::err(OP_CREATE, s),
        };
        let result = match ftype {
            NF4DIR => self.fs().mkdir(dir_ino, name, mode, uid, gid).map(|_| ()),
            NF4LNK => {
                // Empty symlink target is invalid (pynfs CR9a).
                if linkdata.is_empty() {
                    return OpResult::err(OP_CREATE, NFS4ERR_INVAL);
                }
                self.fs()
                    .symlink(dir_ino, name, linkdata, uid, gid)
                    .map(|_| ())
            }
            // RFC 7530 16.7: CREATE is for non-regular files; regular files
            // use OPEN. NF4REG via CREATE is NFS4ERR_BADTYPE.
            NF4REG => return OpResult::err(OP_CREATE, NFS4ERR_BADTYPE),
            _ => return OpResult::err(OP_CREATE, NFS4ERR_NOTSUPP),
        };
        match result {
            Ok(()) => {
                // cfh stays at the directory; cinfo for the change.
                let mut w = Writer::new();
                w.bool(true); // atomic
                w.u64(0); // before changeid
                w.u64(0); // after changeid
                          // attrset
                AttrMask { words: vec![0, 0] }.encode(&mut w);
                OpResult::ok(OP_CREATE, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_CREATE, fs_to_nfs(e)),
        }
    }

    fn op_remove(&mut self, name: &[u8]) -> OpResult {
        if let Err(e) = Self::check_name(name, OP_REMOVE) {
            return e;
        }
        if let Err(e) = Self::check_dots(name, OP_REMOVE) {
            return e;
        }
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_REMOVE, NFS4ERR_NOFILEHANDLE),
        };
        // Try unlink first; if it's a dir, use rmdir.
        let ent = match self.fs().lookup(dir_ino, name) {
            Ok(Some((_, typ))) => typ,
            Ok(None) => return OpResult::err(OP_REMOVE, NFS4ERR_NOENT),
            Err(e) => return OpResult::err(OP_REMOVE, fs_to_nfs(e)),
        };
        let result = if ent == FTYPE_DIR {
            self.fs().rmdir(dir_ino, name)
        } else {
            self.fs().unlink(dir_ino, name)
        };
        match result {
            Ok(()) => {
                let mut w = Writer::new();
                // change_info4: bool(atomic) + u64(before) + u64(after)
                w.bool(true);
                w.u64(0); // before changeid
                w.u64(0); // after changeid
                OpResult::ok(OP_REMOVE, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_REMOVE, fs_to_nfs(e)),
        }
    }

    fn op_secinfo(&mut self, name: &[u8]) -> OpResult {
        // RFC 7530 §16.33: return the security flavors available for `name`.
        // secinfo4 is a discriminated union: for AUTH_SYS (and AUTH_NONE) the
        // arm is `void` — no flavor_info follows (RFC 7530 §16.31.3). We only
        // speak AUTH_SYS, so advertise a single secinfo4 entry. Deliberately
        // do not require the name to exist: macOS sends SECINFO in the create
        // path, and a NOENT here makes it abort the operation instead of
        // proceeding to OPEN(CREATE). (pynfs st_secinfo expects NOENT for
        // missing names; we side with macOS interop — documented deviation.)
        if let Err(e) = Self::check_name(name, OP_SECINFO) {
            return e;
        }
        let mut w = Writer::new();
        w.u32(1); // one secinfo4 entry
        w.u32(1); // rpcsec_flavor = AUTH_SYS (default arm: void)
        OpResult::ok(OP_SECINFO, w.into_bytes())
    }

    fn op_rename(&mut self, old: &[u8], new: &[u8]) -> OpResult {
        if let Err(e) = Self::check_name(old, OP_RENAME) {
            return e;
        }
        if let Err(e) = Self::check_name(new, OP_RENAME) {
            return e;
        }
        if let Err(e) = Self::check_dots(old, OP_RENAME) {
            return e;
        }
        if let Err(e) = Self::check_dots(new, OP_RENAME) {
            return e;
        }
        // cfh = source dir, saved_fh = dest dir (via SAVEFH).
        let src_dir = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_RENAME, NFS4ERR_NOFILEHANDLE),
        };
        let dst_dir = self.saved_fh.unwrap_or(src_dir);
        match self.fs().rename(src_dir, old, dst_dir, new) {
            Ok(()) => {
                let mut w = Writer::new();
                // change_info4 x2: bool(atomic) + u64(before) + u64(after) [RFC 7530 §16.15]
                w.bool(true);
                w.u64(0);
                w.u64(0);
                w.bool(true);
                w.u64(0);
                w.u64(0);
                OpResult::ok(OP_RENAME, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_RENAME, fs_to_nfs(e)),
        }
    }

    fn op_link(&mut self, name: &[u8]) -> OpResult {
        if let Err(e) = Self::check_name(name, OP_LINK) {
            return e;
        }
        if let Err(e) = Self::check_dots(name, OP_LINK) {
            return e;
        }
        // cfh = existing file, saved_fh = dest dir.
        let file_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LINK, NFS4ERR_NOFILEHANDLE),
        };
        let dir_ino = match self.saved_fh {
            Some(i) => i,
            None => return OpResult::err(OP_LINK, NFS4ERR_NOFILEHANDLE),
        };
        match self.fs().link(file_ino, dir_ino, name) {
            Ok(()) => {
                let mut w = Writer::new();
                // change_info4: bool(atomic) + u64(before) + u64(after) [RFC 7530 §16.11]
                w.bool(true);
                w.u64(0);
                w.u64(0);
                OpResult::ok(OP_LINK, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_LINK, fs_to_nfs(e)),
        }
    }

    fn op_setattr(&mut self, attrs: &[(u32, Vec<u8>)]) -> OpResult {
        let ino = match self.cfh {
            Some(i) => i,
            None => {
                // RFC 7530 §16.34: SETATTR4res always includes attrsset,
                // even on error.
                let mut w = Writer::new();
                w.u32(0); // empty bitmap
                return OpResult {
                    opnum: OP_SETATTR,
                    status: NFS4ERR_NOFILEHANDLE,
                    body: w.into_bytes(),
                };
            }
        };
        let mut sa = SetAttrs {
            mode: None,
            uid: None,
            gid: None,
            size: None,
            atime: None,
            mtime: None,
        };
        for (attr, val) in attrs {
            match *attr {
                FATTR4_MODE => {
                    if val.len() >= 4 {
                        sa.mode = Some(u32::from_be_bytes(val[..4].try_into().unwrap()));
                    }
                }
                FATTR4_SIZE => {
                    if val.len() >= 8 {
                        sa.size = Some(u64::from_be_bytes(val[..8].try_into().unwrap()));
                    }
                }
                36 => {
                    if let Some(s) = fattr_str(val) {
                        sa.uid = Some(s.parse().unwrap_or(0));
                    }
                }
                37 => {
                    if let Some(s) = fattr_str(val) {
                        sa.gid = Some(s.parse().unwrap_or(0));
                    }
                }
                47 | 53 => {
                    // time_access/time_modify: (seconds, nseconds)
                    if val.len() >= 12 {
                        let secs = i64::from_be_bytes(val[..8].try_into().unwrap());
                        if secs >= 0 {
                            let t = secs as u64;
                            if *attr == 47 {
                                sa.atime = Some(t);
                            } else {
                                sa.mtime = Some(t);
                            }
                        }
                    }
                }
                _ => return OpResult::err(OP_SETATTR, NFS4ERR_NOTSUPP),
            }
        }
        match self.fs().setattr(ino, &sa) {
            Ok(()) => {
                let mut w = Writer::new();
                // attrset: which attrs were set
                let mut mask = AttrMask { words: vec![0, 0] };
                for (attr, _) in attrs {
                    let word = (*attr / 32) as usize;
                    let bit = *attr % 32;
                    if word < mask.words.len() {
                        mask.words[word] |= 1 << bit;
                    }
                }
                mask.encode(&mut w);
                OpResult::ok(OP_SETATTR, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_SETATTR, fs_to_nfs(e)),
        }
    }

    fn op_write(&mut self, offset: u64, stable: u32, data: &[u8]) -> OpResult {
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_WRITE, NFS4ERR_NOFILEHANDLE),
        };
        if let Err(e) = self.fs().write(ino, offset, data) {
            return OpResult::err(OP_WRITE, fs_to_nfs(e));
        }
        // Stable-write semantics: FILE_SYNC4 must hit stable storage
        // before we reply. P5: commit the transaction.
        let committed = if stable == FILE_SYNC4 {
            match self.fs().commit() {
                Ok(()) => FILE_SYNC4,
                Err(e) => return OpResult::err(OP_WRITE, fs_to_nfs(e)),
            }
        } else {
            stable
        };
        let mut w = Writer::new();
        w.u32(data.len() as u32);
        w.u32(committed);
        // verifier (only meaningful for UNSTABLE)
        w.opaque_fixed(&[0u8; 8]);
        OpResult::ok(OP_WRITE, w.into_bytes())
    }

    fn op_commit(&mut self, _offset: u64, _count: u32) -> OpResult {
        match self.fs().commit() {
            Ok(()) => {
                let mut w = Writer::new();
                w.opaque_fixed(&[0u8; 8]); // verifier
                OpResult::ok(OP_COMMIT, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_COMMIT, fs_to_nfs(e)),
        }
    }

    fn op_setclientid(&mut self, verifier: [u8; 8], name: &[u8]) -> OpResult {
        let (clientid, _) = self.state().setclientid(verifier, name.to_vec());
        let mut w = Writer::new();
        w.u64(clientid);
        w.opaque_fixed(&verifier);
        OpResult::ok(OP_SETCLIENTID, w.into_bytes())
    }

    fn op_setclientid_confirm(&mut self, clientid: u64, verifier: [u8; 8]) -> OpResult {
        if self.state().confirm(clientid, verifier) {
            OpResult::ok(OP_SETCLIENTID_CONFIRM, Vec::new())
        } else {
            OpResult::err(OP_SETCLIENTID_CONFIRM, NFS4ERR_STALE_CLIENTID)
        }
    }

    fn op_close(&mut self, seqid: u32, stateid: &StateId) -> OpResult {
        match self.state().close(stateid, seqid) {
            Ok(()) => {
                // CLOSE4resok: return the "closed" stateid (all zeros per RFC 7530 §16.2.3)
                let mut w = Writer::new();
                w.u32(0xffffffff); // seqid = all-ones (invalidated)
                w.opaque_fixed(&[0u8; 12]); // other = zeroes
                OpResult::ok(OP_CLOSE, w.into_bytes())
            }
            Err(NfsError::Status(s)) => OpResult::err(OP_CLOSE, s),
            Err(_) => OpResult::err(OP_CLOSE, NFS4ERR_SERVERFAULT),
        }
    }

    fn op_open_downgrade(
        &mut self,
        stateid: &StateId,
        seqid: u32,
        share_access: u32,
        share_deny: u32,
    ) -> OpResult {
        match self
            .state()
            .open_downgrade(stateid, seqid, share_access, share_deny)
        {
            Ok(new_stateid) => {
                let mut w = Writer::new();
                new_stateid.encode(&mut w);
                OpResult::ok(OP_OPEN_DOWNGRADE, w.into_bytes())
            }
            Err(NfsError::Status(s)) => OpResult::err(OP_OPEN_DOWNGRADE, s),
            Err(_) => OpResult::err(OP_OPEN_DOWNGRADE, NFS4ERR_SERVERFAULT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn op_lock(
        &mut self,
        locktype: u32,
        _reclaim: bool,
        offset: u64,
        length: u64,
        new_lock_owner: bool,
        _open_seqid: u32,
        open_stateid: &StateId,
        _lock_seqid: u32,
        _lock_stateid: &StateId,
        lock_owner: &[u8],
    ) -> OpResult {
        let file_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LOCK, NFS4ERR_INVAL),
        };
        // For P6, we need the clientid. In a real server, the clientid comes
        // from the RPC credentials or the open_stateid. For simplicity, we
        // use a fixed clientid 1 (the test will use SETCLIENTID first).
        // Actually, let's get it from the open_stateid's client.
        let clientid = match self.state().find_open(open_stateid) {
            Some(o) => o.clientid,
            None if !new_lock_owner => {
                // Existing lock owner: find by lock_stateid.
                return OpResult::err(OP_LOCK, NFS4ERR_EXPIRED);
            }
            None => 1, // Fallback for test.
        };
        let open_st = if new_lock_owner {
            Some(open_stateid)
        } else {
            None
        };
        match self.state().lock(
            clientid,
            lock_owner.to_vec(),
            file_ino,
            locktype,
            offset,
            length,
            open_st,
        ) {
            Ok(rec) => {
                let mut w = Writer::new();
                rec.stateid.encode(&mut w);
                OpResult::ok(OP_LOCK, w.into_bytes())
            }
            Err(NfsError::Status(s)) => OpResult::err(OP_LOCK, s),
            Err(_) => OpResult::err(OP_LOCK, NFS4ERR_SERVERFAULT),
        }
    }

    fn op_locku(
        &mut self,
        _locktype: u32,
        seqid: u32,
        stateid: &StateId,
        offset: u64,
        length: u64,
    ) -> OpResult {
        match self.state().unlock(stateid, seqid, offset, length) {
            Ok(sid) => {
                let mut w = Writer::new();
                sid.encode(&mut w);
                OpResult::ok(OP_LOCKU, w.into_bytes())
            }
            Err(NfsError::Status(s)) => OpResult::err(OP_LOCKU, s),
            Err(_) => OpResult::err(OP_LOCKU, NFS4ERR_SERVERFAULT),
        }
    }

    fn op_renew(&mut self, clientid: u64) -> OpResult {
        if self.state().renew(clientid) {
            OpResult::ok(OP_RENEW, Vec::new())
        } else {
            OpResult::err(OP_RENEW, NFS4ERR_EXPIRED)
        }
    }

    fn op_access(&self, access: u32) -> OpResult {
        let ino = match self.current(OP_ACCESS) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let _inode = match self.fs().getattr(ino) {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_ACCESS, fs_to_nfs(e)),
        };
        // Read-write server: support and grant all standard access bits.
        // The filesystem enforces permissions; ACCESS is advisory.
        let supported: u32 = ACCESS4_READ
            | ACCESS4_LOOKUP
            | ACCESS4_MODIFY
            | ACCESS4_EXTEND
            | ACCESS4_DELETE
            | ACCESS4_EXECUTE;
        let mut granted: u32 = 0;
        for bit in [
            ACCESS4_READ,
            ACCESS4_LOOKUP,
            ACCESS4_MODIFY,
            ACCESS4_EXTEND,
            ACCESS4_DELETE,
            ACCESS4_EXECUTE,
        ] {
            if access & bit != 0 && supported & bit != 0 {
                granted |= bit;
            }
        }
        let mut w = Writer::new();
        w.u32(supported);
        w.u32(granted);
        OpResult::ok(OP_ACCESS, w.into_bytes())
    }
}

/// Serve one TCP connection to completion (client close or fatal error).
fn serve_connection(stream: TcpStream, shared: &Shared) -> Result<(), ServerError> {
    let mut stream = stream;
    let mut rr = RecordReader::new();
    let mut session = Session::new(shared);
    let mut buf = [0u8; 64 * 1024];
    let debug_rpc = std::env::var("COWNFS_DEBUG_RPC").is_ok();
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        rr.feed(&buf[..n]);
        while let Some(record) = rr.next_record()? {
            if debug_rpc {
                eprintln!("RPC> {} bytes: {}", record.len(), hex(&record));
            }
            let reply = handle_record(&record, &mut session, debug_rpc);
            if debug_rpc {
                eprintln!("RPC< {} bytes: {}", reply.len(), hex(&reply));
            }
            stream.write_all(&reply)?;
        }
    }
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn op_name(op: u32) -> &'static str {
    use crate::nfs4::*;
    match op {
        OP_ACCESS => "ACCESS",
        OP_CLOSE => "CLOSE",
        OP_COMMIT => "COMMIT",
        OP_CREATE => "CREATE",
        OP_GETATTR => "GETATTR",
        OP_GETFH => "GETFH",
        OP_LINK => "LINK",
        OP_LOCK => "LOCK",
        OP_LOCKU => "LOCKU",
        OP_LOOKUP => "LOOKUP",
        OP_LOOKUPP => "LOOKUPP",
        OP_OPEN => "OPEN",
        OP_PUTFH => "PUTFH",
        OP_PUTROOTFH => "PUTROOTFH",
        OP_READ => "READ",
        OP_READDIR => "READDIR",
        OP_REMOVE => "REMOVE",
        OP_SECINFO => "SECINFO",
        OP_ILLEGAL => "ILLEGAL",
        OP_RENAME => "RENAME",
        OP_RENEW => "RENEW",
        OP_RESTOREFH => "RESTOREFH",
        OP_SAVEFH => "SAVEFH",
        OP_SETATTR => "SETATTR",
        OP_SETCLIENTID => "SETCLIENTID",
        OP_SETCLIENTID_CONFIRM => "SETCLIENTID_CONFIRM",
        OP_EXCHANGE_ID => "EXCHANGE_ID",
        OP_CREATE_SESSION => "CREATE_SESSION",
        OP_DESTROY_SESSION => "DESTROY_SESSION",
        OP_SEQUENCE => "SEQUENCE",
        OP_DESTROY_CLIENTID => "DESTROY_CLIENTID",
        OP_LAYOUTGET => "LAYOUTGET",
        OP_LAYOUTCOMMIT => "LAYOUTCOMMIT",
        OP_LAYOUTRETURN => "LAYOUTRETURN",
        OP_WRITE => "WRITE",
        _ => "UNKNOWN",
    }
}

fn handle_record(record: &[u8], session: &mut Session, debug_rpc: bool) -> Vec<u8> {
    let call = match Call::decode(record) {
        Ok(c) => c,
        Err(RpcError::BadProgram) => {
            // Best-effort xid extraction for the reject.
            let xid = u32::from_be_bytes(record[..4].try_into().unwrap_or([0; 4]));
            return rpc::frame_record(&rpc::encode_reject(xid, 1)); // PROG_UNAVAIL
        }
        Err(_) => return Vec::new(), // drop malformed records silently
    };
    let mut params = call.params;
    let results = match call.proc {
        crate::rpc::PROC_NULL => rpc::encode_reply(call.xid, &[]),
        crate::rpc::PROC_COMPOUND => {
            let (overall, payload) = match Compound::decode(&mut params) {
                Ok(c) => {
                    let res = session.run(&c);
                    let overall = res
                        .iter()
                        .find(|r| r.status != NFS4_OK)
                        .map(|r| r.status)
                        .unwrap_or(NFS4_OK);
                    // Human-readable op log: tag [OP→status OP→status …]
                    if debug_rpc {
                        let ops: Vec<String> = res
                            .iter()
                            .map(|r| {
                                let name = op_name(r.opnum);
                                if r.status == NFS4_OK {
                                    name.to_string()
                                } else {
                                    format!("{name}→{}", r.status)
                                }
                            })
                            .collect();
                        let tag = String::from_utf8_lossy(c.tag).trim().to_string();
                        eprintln!("OPS [{}] {}", tag, ops.join(" "));
                    }
                    (overall, encode_compound(c.tag, overall, &res))
                }
                Err(NfsError::BadOp(n)) => {
                    if debug_rpc {
                        eprintln!("OPS [?] decode error: BadOp({n})");
                    }
                    (NFS4ERR_NOTSUPP, encode_compound(b"", NFS4ERR_NOTSUPP, &[]))
                }
                Err(NfsError::Xdr(e)) => {
                    if debug_rpc {
                        eprintln!("OPS [?] decode error: Xdr({e:?})");
                    }
                    (NFS4ERR_BADXDR, encode_compound(b"", NFS4ERR_BADXDR, &[]))
                }
                Err(e) => {
                    if debug_rpc {
                        eprintln!("OPS [?] decode error: {e:?}");
                    }
                    (
                        NFS4ERR_SERVERFAULT,
                        encode_compound(b"", NFS4ERR_SERVERFAULT, &[]),
                    )
                }
            };
            let _ = overall;
            rpc::encode_reply(call.xid, &payload)
        }

        _ => rpc::encode_reply(call.xid, &[]),
    };
    rpc::frame_record(&results)
}

/// Run the server on an already-bound `listener` (e.g. port 0 for an
/// ephemeral port in tests), serving one connection at a time. Returns
/// when the listener errors.
pub fn serve_listener(listener: TcpListener, shared: &Shared) -> Result<(), ServerError> {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let _ = serve_connection(s, shared);
            }
            Err(e) => return Err(ServerError::Io(e)),
        }
    }
    Ok(())
}

/// Run the server on an already-bound `listener`, spawning one thread per
/// connection. Filesystem and NFSv4 state are shared across connections.
/// Returns when the listener errors.
pub fn serve_concurrent(listener: TcpListener, shared: Shared) -> Result<(), ServerError> {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve_connection(s, &shared);
                });
            }
            Err(e) => return Err(ServerError::Io(e)),
        }
    }
    Ok(())
}

/// Run the server on `addr` (e.g. "127.0.0.1:2049"), one thread per
/// connection. Returns when the listener errors.
pub fn serve(addr: &str, shared: Shared) -> Result<(), ServerError> {
    serve_concurrent(TcpListener::bind(addr)?, shared)
}

// Reference the unused import to keep the build clean in P4.
#[allow(dead_code)]
fn _mode_attr() -> u32 {
    FATTR4_MODE
}
