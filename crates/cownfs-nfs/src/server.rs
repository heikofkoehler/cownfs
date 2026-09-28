//! Minimal NFSv4.0 TCP server: one thread per connection, read-only
//! COMPOUND execution against the engine (P4).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use cownfs_core::engine::{Fs, FsError, FTYPE_DIR, FTYPE_SYMLINK, ROOT_INO};

use crate::nfs4::{
    encode_compound, AttrMask, AttrValues, Compound, FileAttrs, FileHandle, NfsError, Op, OpResult,
    StateId, ACCESS4_EXECUTE, ACCESS4_EXTEND, ACCESS4_LOOKUP, ACCESS4_READ, FATTR4_MODE,
    FATTR4_SIZE, FILE_SYNC4, GUARDED4, NF4DIR, NF4LNK, NF4REG, NFS4ERR_INVAL, NFS4ERR_ISDIR,
    NFS4ERR_NOENT, NFS4ERR_NOTDIR, NFS4ERR_NOTSUPP, NFS4ERR_SERVERFAULT, NFS4_OK, OPEN4_CREATE,
    OP_ACCESS, OP_COMMIT, OP_CREATE, OP_GETATTR, OP_GETFH, OP_LINK, OP_LOOKUP, OP_LOOKUPP, OP_OPEN,
    OP_PUTFH, OP_PUTROOTFH, OP_READ, OP_READDIR, OP_REMOVE, OP_RENAME, OP_RESTOREFH, OP_SAVEFH,
    OP_SETATTR, OP_WRITE, UNCHECKED4,
};
use crate::rpc::{self, Call, RecordReader, RpcError};
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

/// Map engine errors to NFS4 status codes.
fn fs_to_nfs(e: FsError) -> u32 {
    match e {
        FsError::NotFound => NFS4ERR_NOENT,
        FsError::NotDir => NFS4ERR_NOTDIR,
        FsError::NotFile => NFS4ERR_ISDIR,
        FsError::NotEmpty => NFS4ERR_NOTSUPP,
        FsError::AlreadyExists => NFS4ERR_NOTSUPP,
        FsError::BadName => NFS4ERR_INVAL,
        FsError::NoSpace => NFS4ERR_NOTSUPP, // read-only in P4; real mapping in P5
        FsError::Invalid(_) => NFS4ERR_INVAL,
        FsError::Store(_) => NFS4ERR_SERVERFAULT,
    }
}

/// Per-connection COMPOUND execution state.
struct Session<'f> {
    fs: &'f mut Fs,
    /// Current filehandle's inode (None = none selected).
    cfh: Option<u64>,
    /// Saved filehandle (SAVEFH/RESTOREFH).
    saved_fh: Option<u64>,
}

impl<'f> Session<'f> {
    fn new(fs: &'f mut Fs) -> Self {
        Session {
            fs,
            cfh: None,
            saved_fh: None,
        }
    }

    fn check_fh(&self, fh: &FileHandle) -> Result<u64, u32> {
        if fh.fs_uuid != self.fs.uuid() {
            return Err(NFS4ERR_INVAL);
        }
        Ok(fh.inode)
    }

    fn run(&mut self, compound: &Compound) -> Vec<OpResult> {
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
                        fs_uuid: self.fs.uuid(),
                        inode: ino,
                    }
                    .encode(&mut w);
                    OpResult::ok(OP_GETFH, w.into_bytes())
                }
                None => OpResult::err(OP_GETFH, NFS4ERR_INVAL),
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
                flags,
                opentype,
                createmode,
                createattrs,
                claim_type,
                filename,
            } => self.op_open(
                *flags,
                *opentype,
                *createmode,
                createattrs,
                *claim_type,
                filename,
            ),
            Op::Create {
                ftype,
                linkdata,
                name,
                attrs,
            } => self.op_create(*ftype, linkdata, name, attrs),
            Op::Remove(name) => self.op_remove(name),
            Op::Rename { old, new } => self.op_rename(old, new),
            Op::Link(name) => self.op_link(name),
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
            Op::SetAttr { attrs } => self.op_setattr(attrs),
            Op::Write {
                offset,
                stable,
                data,
            } => self.op_write(*offset, *stable, data),
            Op::Commit { offset, count } => self.op_commit(*offset, *count),
        }
    }

    fn current(&self) -> Result<u64, OpResult> {
        self.cfh.ok_or(OpResult::err(OP_PUTFH, NFS4ERR_INVAL))
    }

    fn op_lookup(&mut self, name: &[u8], opnum: u32) -> OpResult {
        let ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(opnum, NFS4ERR_INVAL),
        };
        // "." and ".." are handled by LOOKUPP; treat "." as self.
        let result = if name == b"." {
            Ok(Some((ino, 0)))
        } else if name == b".." {
            return self.op_lookupp_inner(opnum);
        } else {
            self.fs.lookup(ino, name)
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
            None => return OpResult::err(opnum, NFS4ERR_INVAL),
        };
        if ino == ROOT_INO {
            // Root's parent is itself.
            self.cfh = Some(ROOT_INO);
            return OpResult::ok(opnum, Vec::new());
        }
        match self.fs.lookup(ino, b"..") {
            Ok(Some((parent, _))) => {
                self.cfh = Some(parent);
                OpResult::ok(opnum, Vec::new())
            }
            Ok(None) => OpResult::err(opnum, NFS4ERR_NOENT),
            Err(e) => OpResult::err(opnum, fs_to_nfs(e)),
        }
    }

    fn op_getattr(&self, mask: &AttrMask) -> OpResult {
        let ino = match self.current() {
            Ok(i) => i,
            Err(r) => return r,
        };
        let inode = match self.fs.getattr(ino) {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_GETATTR, fs_to_nfs(e)),
        };
        let ftype = match inode.ftype {
            FTYPE_DIR => NF4DIR,
            FTYPE_SYMLINK => NF4LNK,
            _ => NF4REG,
        };
        let vals = AttrValues::encode(
            mask,
            &FileAttrs {
                ftype,
                size: inode.size,
                fileid: ino,
                mode: inode.mode,
                nlink: inode.nlink,
                uid: inode.uid,
                gid: inode.gid,
                atime: inode.atime,
                mtime: inode.mtime,
                ctime: inode.ctime,
            },
        );
        let mut w = Writer::new();
        vals.encode_result(&mut w);
        OpResult::ok(OP_GETATTR, w.into_bytes())
    }

    fn op_readdir(&self, cookie: u64, _dircount: u32, maxcount: u32, mask: &AttrMask) -> OpResult {
        let ino = match self.current() {
            Ok(i) => i,
            Err(r) => return r,
        };
        let entries = match self.fs.readdir(ino) {
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
        for (idx, (name, child_ino, typ)) in entries.iter().enumerate().skip(start) {
            let ftype = match *typ {
                FTYPE_DIR => NF4DIR,
                FTYPE_SYMLINK => NF4LNK,
                _ => NF4REG,
            };
            let inode = match self.fs.getattr(*child_ino) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let vals = AttrValues::encode(
                mask,
                &FileAttrs {
                    ftype,
                    size: inode.size,
                    fileid: *child_ino,
                    mode: inode.mode,
                    nlink: inode.nlink,
                    uid: inode.uid,
                    gid: inode.gid,
                    atime: inode.atime,
                    mtime: inode.mtime,
                    ctime: inode.ctime,
                },
            );
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
        let ino = match self.current() {
            Ok(i) => i,
            Err(r) => return r,
        };
        // READ on a directory or symlink target: symlinks are read via
        // READ in v4.0 (no READLINK needed for the P4 gate, but support it).
        let data = match self.fs.read(ino, offset, count.min(1024 * 1024) as usize) {
            Ok(d) => d,
            Err(e) => return OpResult::err(OP_READ, fs_to_nfs(e)),
        };
        let inode = match self.fs.getattr(ino) {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_READ, fs_to_nfs(e)),
        };
        let eof = offset + data.len() as u64 >= inode.size;
        let mut w = Writer::new();
        w.bool(eof);
        w.opaque(&data);
        OpResult::ok(OP_READ, w.into_bytes())
    }

    /// Extract mode/uid/gid from createattrs (fattr4).
    fn parse_createattrs(attrs: &[(u32, Vec<u8>)]) -> (u32, u32, u32) {
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
                    if let Ok(s) = std::str::from_utf8(val) {
                        uid = s.parse().unwrap_or(0);
                    }
                }
                37 => {
                    // FATTR4_OWNER_GROUP
                    if let Ok(s) = std::str::from_utf8(val) {
                        gid = s.parse().unwrap_or(0);
                    }
                }
                _ => {}
            }
        }
        (mode, uid, gid)
    }

    fn op_open(
        &mut self,
        _flags: u32,
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
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_OPEN, NFS4ERR_INVAL),
        };
        // Check the cfh is a directory.
        match self.fs.getattr(dir_ino) {
            Ok(inode) if inode.ftype == FTYPE_DIR => {}
            Ok(_) => return OpResult::err(OP_OPEN, NFS4ERR_NOTDIR),
            Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
        }

        let existing = match self.fs.lookup(dir_ino, filename) {
            Ok(o) => o,
            Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
        };

        let file_ino = match (existing, opentype == OPEN4_CREATE) {
            (Some((ino, _)), _) => ino,
            (None, true) => {
                // Create the file.
                if createmode != UNCHECKED4 && createmode != GUARDED4 {
                    return OpResult::err(OP_OPEN, NFS4ERR_NOTSUPP);
                }
                let (mode, uid, gid) = Self::parse_createattrs(createattrs);
                match self.fs.create(dir_ino, filename, mode, uid, gid) {
                    Ok(ino) => ino,
                    Err(e) => return OpResult::err(OP_OPEN, fs_to_nfs(e)),
                }
            }
            (None, false) => return OpResult::err(OP_OPEN, NFS4ERR_NOENT),
        };

        self.cfh = Some(file_ino);
        // P5: dummy stateid; P6 implements real state.
        let mut w = Writer::new();
        StateId {
            seqid: 0,
            other: [0u8; 12],
        }
        .encode(&mut w);
        // changeid
        w.u64(0);
        // rflags
        w.u32(0);
        // attrset (no delegation attrs)
        w.u32(0);
        // delegation type: OPEN_DELEGATE_NONE
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
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_CREATE, NFS4ERR_INVAL),
        };
        let (mode, uid, gid) = Self::parse_createattrs(attrs);
        let result = match ftype {
            NF4DIR => self.fs.mkdir(dir_ino, name, mode, uid, gid).map(|_| ()),
            NF4LNK => self
                .fs
                .symlink(dir_ino, name, linkdata, uid, gid)
                .map(|_| ()),
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
        let dir_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_REMOVE, NFS4ERR_INVAL),
        };
        // Try unlink first; if it's a dir, use rmdir.
        let ent = match self.fs.lookup(dir_ino, name) {
            Ok(Some((_, typ))) => typ,
            Ok(None) => return OpResult::err(OP_REMOVE, NFS4ERR_NOENT),
            Err(e) => return OpResult::err(OP_REMOVE, fs_to_nfs(e)),
        };
        let result = if ent == FTYPE_DIR {
            self.fs.rmdir(dir_ino, name)
        } else {
            self.fs.unlink(dir_ino, name)
        };
        match result {
            Ok(()) => {
                let mut w = Writer::new();
                w.u64(0); // cinfo before
                w.u64(0); // cinfo after
                OpResult::ok(OP_REMOVE, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_REMOVE, fs_to_nfs(e)),
        }
    }

    fn op_rename(&mut self, old: &[u8], new: &[u8]) -> OpResult {
        // cfh = source dir, saved_fh = dest dir (via SAVEFH).
        let src_dir = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_RENAME, NFS4ERR_INVAL),
        };
        let dst_dir = self.saved_fh.unwrap_or(src_dir);
        match self.fs.rename(src_dir, old, dst_dir, new) {
            Ok(()) => {
                let mut w = Writer::new();
                w.u64(0);
                w.u64(0);
                w.u64(0);
                w.u64(0);
                OpResult::ok(OP_RENAME, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_RENAME, fs_to_nfs(e)),
        }
    }

    fn op_link(&mut self, name: &[u8]) -> OpResult {
        // cfh = existing file, saved_fh = dest dir.
        let file_ino = match self.cfh {
            Some(i) => i,
            None => return OpResult::err(OP_LINK, NFS4ERR_INVAL),
        };
        let dir_ino = match self.saved_fh {
            Some(i) => i,
            None => return OpResult::err(OP_LINK, NFS4ERR_INVAL),
        };
        match self.fs.link(file_ino, dir_ino, name) {
            Ok(()) => {
                let mut w = Writer::new();
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
            None => return OpResult::err(OP_SETATTR, NFS4ERR_INVAL),
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
                    if let Ok(s) = std::str::from_utf8(val) {
                        sa.uid = Some(s.parse().unwrap_or(0));
                    }
                }
                37 => {
                    if let Ok(s) = std::str::from_utf8(val) {
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
        match self.fs.setattr(ino, &sa) {
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
            None => return OpResult::err(OP_WRITE, NFS4ERR_INVAL),
        };
        if let Err(e) = self.fs.write(ino, offset, data) {
            return OpResult::err(OP_WRITE, fs_to_nfs(e));
        }
        // Stable-write semantics: FILE_SYNC4 must hit stable storage
        // before we reply. P5: commit the transaction.
        let committed = if stable == FILE_SYNC4 {
            match self.fs.commit() {
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
        match self.fs.commit() {
            Ok(()) => {
                let mut w = Writer::new();
                w.opaque_fixed(&[0u8; 8]); // verifier
                OpResult::ok(OP_COMMIT, w.into_bytes())
            }
            Err(e) => OpResult::err(OP_COMMIT, fs_to_nfs(e)),
        }
    }

    fn op_access(&self, access: u32) -> OpResult {
        let ino = match self.current() {
            Ok(i) => i,
            Err(r) => return r,
        };
        let inode = match self.fs.getattr(ino) {
            Ok(i) => i,
            Err(e) => return OpResult::err(OP_ACCESS, fs_to_nfs(e)),
        };
        // Read-only server: grant read/lookup/execute, deny modify/extend/delete.
        let mut supported: u32 = ACCESS4_READ | ACCESS4_LOOKUP | ACCESS4_EXECUTE;
        let mut granted: u32 = 0;
        if inode.ftype == FTYPE_DIR {
            supported |= ACCESS4_LOOKUP;
        } else {
            // Regular files: no extend/modify/delete on a read-only mount.
            let _ = ACCESS4_EXTEND;
        }
        for bit in [ACCESS4_READ, ACCESS4_LOOKUP, ACCESS4_EXECUTE] {
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
fn serve_connection(stream: TcpStream, fs: &mut Fs) -> Result<(), ServerError> {
    let mut stream = stream;
    let mut rr = RecordReader::new();
    let mut session = Session::new(fs);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        rr.feed(&buf[..n]);
        while let Some(record) = rr.next_record()? {
            let reply = handle_record(&record, &mut session);
            stream.write_all(&reply)?;
        }
    }
}

fn handle_record(record: &[u8], session: &mut Session) -> Vec<u8> {
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
                    (overall, encode_compound(c.tag, overall, &res))
                }
                Err(NfsError::BadOp(_)) => {
                    (NFS4ERR_NOTSUPP, encode_compound(b"", NFS4ERR_NOTSUPP, &[]))
                }
                Err(_) => (
                    NFS4ERR_SERVERFAULT,
                    encode_compound(b"", NFS4ERR_SERVERFAULT, &[]),
                ),
            };
            let _ = overall;
            rpc::encode_reply(call.xid, &payload)
        }
        _ => rpc::encode_reply(call.xid, &[]),
    };
    rpc::frame_record(&results)
}

/// Run the server on `addr` (e.g. "127.0.0.1:2049") with one thread per
/// connection. Returns when the listener errors.
pub fn serve(addr: &str, fs: &mut Fs) -> Result<(), ServerError> {
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        match stream {
            // Single-threaded in P4: one connection at a time.
            Ok(s) => {
                let _ = serve_connection(s, fs);
            }
            Err(e) => return Err(ServerError::Io(e)),
        }
    }
    Ok(())
}

// Reference the unused import to keep the build clean in P4.
#[allow(dead_code)]
fn _mode_attr() -> u32 {
    FATTR4_MODE
}
