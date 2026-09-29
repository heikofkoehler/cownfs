//! NFSv4.0 (RFC 7530) types and XDR codecs: filehandles, COMPOUND,
//! the read-only operation subset (P4), and attribute encoding.

use crate::xdr::{Reader, Writer, XdrError};

// -- protocol constants ----------------------------------------------------

pub const NFS4_OK: u32 = 0;
pub const NFS4ERR_NOENT: u32 = 2;
pub const NFS4ERR_IO: u32 = 5;
pub const NFS4ERR_ACCESS: u32 = 13;
pub const NFS4ERR_EXIST: u32 = 17;
pub const NFS4ERR_NOTDIR: u32 = 20;
pub const NFS4ERR_ISDIR: u32 = 21;
pub const NFS4ERR_INVAL: u32 = 22;
pub const NFS4ERR_NOSPC: u32 = 28;
pub const NFS4ERR_ROFS: u32 = 30;
pub const NFS4ERR_NOTSUPP: u32 = 10004;
pub const NFS4ERR_SERVERFAULT: u32 = 10006;
pub const NFS4ERR_BAD_STATEID: u32 = 10025;
pub const NFS4ERR_OPENMODE: u32 = 10026;
pub const NFS4ERR_EXPIRED: u32 = 10011;
pub const NFS4ERR_LOCKED: u32 = 10012;
pub const NFS4ERR_DENIED: u32 = 10010;
pub const NFS4ERR_BAD_SEQID: u32 = 10027;
pub const NFS4ERR_STALE_CLIENTID: u32 = 10022;

// Operation numbers.
pub const OP_ACCESS: u32 = 3;
pub const OP_CREATE: u32 = 6;
pub const OP_GETATTR: u32 = 9;
pub const OP_GETFH: u32 = 10;
pub const OP_LOOKUP: u32 = 15;
pub const OP_LOOKUPP: u32 = 16;
pub const OP_COMMIT: u32 = 5;
pub const OP_PUTFH: u32 = 22;
pub const OP_PUTROOTFH: u32 = 24;
pub const OP_READ: u32 = 25;
pub const OP_READDIR: u32 = 26;
pub const OP_REMOVE: u32 = 28;
pub const OP_RENAME: u32 = 29;
pub const OP_LINK: u32 = 11;
pub const OP_RESTOREFH: u32 = 31;
pub const OP_SAVEFH: u32 = 32;
pub const OP_SETATTR: u32 = 34;
pub const OP_WRITE: u32 = 38;
pub const OP_OPEN: u32 = 18;
pub const OP_CLOSE: u32 = 4;
pub const OP_LOCK: u32 = 12;
pub const OP_LOCKU: u32 = 14;
pub const OP_RENEW: u32 = 30;
pub const OP_SETCLIENTID: u32 = 35;
pub const OP_SETCLIENTID_CONFIRM: u32 = 36;

// Attribute numbers (RFC 7530 §5).
pub const FATTR4_TYPE: u32 = 1;
pub const FATTR4_SIZE: u32 = 4;
pub const FATTR4_LINK_SUPPORT: u32 = 5;
pub const FATTR4_FSID: u32 = 8;
pub const FATTR4_FILEHANDLE: u32 = 19;
pub const FATTR4_NUMLINKS: u32 = 35;
pub const FATTR4_FILEID: u32 = 20;
pub const FATTR4_MODE: u32 = 33;
pub const FATTR4_OWNER: u32 = 36;
pub const FATTR4_OWNER_GROUP: u32 = 37;
pub const FATTR4_SPACE_USED: u32 = 45;
pub const FATTR4_TIME_ACCESS: u32 = 47;
pub const FATTR4_TIME_METADATA: u32 = 52;
pub const FATTR4_TIME_MODIFY: u32 = 53;
pub const FATTR4_MOUNTED_ON_FILEID: u32 = 55;

// File types.
pub const NF4REG: u32 = 1;
pub const NF4DIR: u32 = 2;
pub const NF4LNK: u32 = 5;

// Create modes.
pub const UNCHECKED4: u32 = 0;
pub const GUARDED4: u32 = 1;
pub const EXCLUSIVE4: u32 = 2;

// Open flags.
pub const OPEN4_NOCREATE: u32 = 0;
pub const OPEN4_CREATE: u32 = 1;

// Lock types.
pub const READ_LT: u32 = 1;
pub const WRITE_LT: u32 = 2;
pub const READW_LT: u32 = 3;
pub const WRITEW_LT: u32 = 4;

// Stable write modes.
pub const UNSTABLE4: u32 = 0;
pub const DATA_SYNC4: u32 = 1;
pub const FILE_SYNC4: u32 = 2;

// Access bits.
pub const ACCESS4_READ: u32 = 0x0001;
pub const ACCESS4_LOOKUP: u32 = 0x0002;
pub const ACCESS4_MODIFY: u32 = 0x0004;
pub const ACCESS4_EXTEND: u32 = 0x0008;
pub const ACCESS4_DELETE: u32 = 0x0010;
pub const ACCESS4_EXECUTE: u32 = 0x0020;

/// NFSv4 stateid (16 bytes: seqid + 12-byte other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateId {
    pub seqid: u32,
    pub other: [u8; 12],
}

impl StateId {
    pub fn decode(r: &mut Reader) -> Result<Self, NfsError> {
        let seqid = r.u32()?;
        let b = r.opaque_fixed(12)?;
        let mut other = [0u8; 12];
        other.copy_from_slice(b);
        Ok(StateId { seqid, other })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.seqid);
        w.opaque_fixed(&self.other);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NfsError {
    Xdr(XdrError),
    /// Unknown operation number.
    BadOp(u32),
    /// Minor version != 0.
    BadMinor,
    /// NFS status code (for state management errors).
    Status(u32),
}

impl From<XdrError> for NfsError {
    fn from(e: XdrError) -> Self {
        NfsError::Xdr(e)
    }
}

// -- filehandles -----------------------------------------------------------

/// Opaque filehandle layout (32 bytes):
/// `[magic:4][fs_uuid:16][inode:8][gen:4]`.
/// The generation lets future invalidation work; v1 always writes 0.
pub const FH_MAGIC: u32 = 0x434f5746; // "COWF"
pub const FH_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHandle {
    pub fs_uuid: [u8; 16],
    pub inode: u64,
}

impl FileHandle {
    /// Raw FH_LEN-byte encoding (magic + uuid + inode + gen), without the
    /// XDR opaque<> wrapper. Used for FATTR4_FILEHANDLE attr values.
    pub fn to_bytes(&self) -> [u8; FH_LEN] {
        let mut fh = [0u8; FH_LEN];
        fh[0..4].copy_from_slice(&FH_MAGIC.to_be_bytes());
        fh[4..20].copy_from_slice(&self.fs_uuid);
        fh[20..28].copy_from_slice(&self.inode.to_be_bytes());
        // gen = 0
        fh
    }

    pub fn encode(&self, w: &mut Writer) {
        w.opaque(&self.to_bytes());
    }

    pub fn decode(r: &mut Reader) -> Result<Self, NfsError> {
        let b = r.opaque()?;
        if b.len() != FH_LEN {
            return Err(NfsError::Xdr(XdrError::Invalid("filehandle length")));
        }
        if u32::from_be_bytes(b[0..4].try_into().unwrap()) != FH_MAGIC {
            return Err(NfsError::Xdr(XdrError::Invalid("filehandle magic")));
        }
        let mut fs_uuid = [0u8; 16];
        fs_uuid.copy_from_slice(&b[4..20]);
        let inode = u64::from_be_bytes(b[20..28].try_into().unwrap());
        Ok(FileHandle { fs_uuid, inode })
    }
}

// -- COMPOUND --------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Op {
    PutFh(FileHandle),
    PutRootFh,
    GetFh,
    Lookup(Vec<u8>),
    LookupP,
    GetAttr(AttrMask),
    ReadDir {
        cookie: u64,
        cookieverf: [u8; 8],
        dircount: u32,
        maxcount: u32,
        mask: AttrMask,
    },
    Read {
        offset: u64,
        count: u32,
    },
    Access {
        access: u32,
    },
    Open {
        clientid: u64,
        owner: Vec<u8>,
        flags: u32,
        opentype: u32,
        createmode: u32,
        createattrs: Vec<(u32, Vec<u8>)>,
        claim_type: u32,
        filename: Vec<u8>,
    },
    Create {
        ftype: u32,
        linkdata: Vec<u8>,
        name: Vec<u8>,
        attrs: Vec<(u32, Vec<u8>)>,
    },
    Remove(Vec<u8>),
    Rename {
        old: Vec<u8>,
        new: Vec<u8>,
    },
    Link(Vec<u8>),
    SaveFh,
    RestoreFh,
    SetAttr {
        attrs: Vec<(u32, Vec<u8>)>,
    },
    Write {
        offset: u64,
        stable: u32,
        data: Vec<u8>,
    },
    Commit {
        offset: u64,
        count: u32,
    },
    SetClientId {
        verifier: [u8; 8],
        client_name: Vec<u8>,
        callback_prog: u32,
        netid: String,
        addr: String,
    },
    SetClientIdConfirm {
        clientid: u64,
        verifier: [u8; 8],
    },
    Close {
        seqid: u32,
        stateid: StateId,
    },
    Lock {
        locktype: u32,
        reclaim: bool,
        offset: u64,
        length: u64,
        new_lock_owner: bool,
        open_seqid: u32,
        open_stateid: StateId,
        lock_seqid: u32,
        lock_stateid: StateId,
        lock_owner: Vec<u8>,
    },
    LockU {
        locktype: u32,
        seqid: u32,
        stateid: StateId,
        offset: u64,
        length: u64,
    },
    Renew {
        clientid: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttrMask {
    /// Counted array of u32 words (v4.0 uses 2, but accept more).
    pub words: Vec<u32>,
}

impl AttrMask {
    pub fn decode(r: &mut Reader) -> Result<Self, NfsError> {
        let n = r.u32()? as usize;
        if n > 8 {
            return Err(NfsError::Xdr(XdrError::Invalid("attr mask")));
        }
        let mut words = Vec::with_capacity(n);
        for _ in 0..n {
            words.push(r.u32()?);
        }
        Ok(AttrMask { words })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.words.len() as u32);
        for word in &self.words {
            w.u32(*word);
        }
    }

    /// Test whether attribute `attr` is requested.
    pub fn wants(&self, attr: u32) -> bool {
        let word = (attr / 32) as usize;
        let bit = attr % 32;
        self.words
            .get(word)
            .map(|w| w & (1 << bit) != 0)
            .unwrap_or(false)
    }
}

#[derive(Debug)]
pub struct Compound<'a> {
    pub tag: &'a [u8],
    pub ops: Vec<Op>,
}

impl<'a> Compound<'a> {
    pub fn decode(params: &mut Reader<'a>) -> Result<Self, NfsError> {
        let tag = params.string()?;
        let minor = params.u32()?;
        if minor != 0 {
            return Err(NfsError::BadMinor);
        }
        let nops = params.u32()? as usize;
        if nops > 64 {
            return Err(NfsError::Xdr(XdrError::Invalid("too many ops")));
        }
        let mut ops = Vec::with_capacity(nops);
        for _ in 0..nops {
            ops.push(Op::decode(params)?);
        }
        params.end()?;
        Ok(Compound { tag, ops })
    }
}

/// Parse an fattr4 (bitmap + attr values) into (attrnum, raw XDR value) pairs.
pub fn parse_fattr(r: &mut Reader) -> Result<Vec<(u32, Vec<u8>)>, NfsError> {
    let mask = AttrMask::decode(r)?;
    let blob = r.opaque()?;
    let mut br = Reader::new(blob);
    let mut out = Vec::new();
    for (wi, word) in mask.words.iter().enumerate() {
        for bit in 0..32 {
            if word & (1 << bit) == 0 {
                continue;
            }
            let attr = wi as u32 * 32 + bit;
            // Capture the raw bytes for this attr by recording position.
            let before = br.remaining();
            skip_attr_value(&mut br, attr)?;
            let after = br.remaining();
            let len = before - after;
            let start = blob.len() - before;
            out.push((attr, blob[start..start + len].to_vec()));
        }
    }
    Ok(out)
}

/// Skip one attribute value in an attrlist, based on its type.
fn skip_attr_value(br: &mut Reader, attr: u32) -> Result<(), NfsError> {
    match attr {
        FATTR4_TYPE => {
            br.u32()?;
        }
        FATTR4_SIZE | FATTR4_FILEID | FATTR4_SPACE_USED | FATTR4_MOUNTED_ON_FILEID => {
            br.u64()?;
        }
        FATTR4_FSID => {
            br.u64()?;
            br.u64()?;
        }
        FATTR4_NUMLINKS | FATTR4_MODE | FATTR4_LINK_SUPPORT => {
            br.u32()?;
        }
        FATTR4_OWNER | FATTR4_OWNER_GROUP | FATTR4_FILEHANDLE => {
            br.opaque()?;
        }
        FATTR4_TIME_ACCESS | FATTR4_TIME_METADATA | FATTR4_TIME_MODIFY => {
            br.i64()?;
            br.u32()?;
        }
        _ => return Err(NfsError::Xdr(XdrError::Invalid("unsupported attr"))),
    }
    Ok(())
}

impl Op {
    pub fn decode(r: &mut Reader) -> Result<Self, NfsError> {
        let opnum = r.u32()?;
        let op = match opnum {
            OP_PUTFH => Op::PutFh(FileHandle::decode(r)?),
            OP_PUTROOTFH => Op::PutRootFh,
            OP_GETFH => Op::GetFh,
            OP_LOOKUP => Op::Lookup(r.string()?.to_vec()),
            OP_LOOKUPP => Op::LookupP,
            OP_GETATTR => Op::GetAttr(AttrMask::decode(r)?),
            OP_READDIR => Op::ReadDir {
                cookie: r.u64()?,
                cookieverf: {
                    let b = r.opaque_fixed(8)?;
                    let mut v = [0u8; 8];
                    v.copy_from_slice(b);
                    v
                },
                dircount: r.u32()?,
                maxcount: r.u32()?,
                mask: AttrMask::decode(r)?,
            },
            OP_READ => {
                let _seqid = r.u32()?;
                let _other = r.opaque_fixed(12)?;
                Op::Read {
                    offset: r.u64()?,
                    count: r.u32()?,
                }
            }
            OP_ACCESS => Op::Access { access: r.u32()? },
            OP_OPEN => {
                let clientid = r.u64()?;
                let owner = r.opaque()?.to_vec();
                let flags = r.u32()?;
                let opentype = r.u32()?;
                let (createmode, createattrs) = if opentype == OPEN4_CREATE {
                    let mode = r.u32()?;
                    let attrs = match mode {
                        UNCHECKED4 | GUARDED4 => parse_fattr(r)?,
                        EXCLUSIVE4 => {
                            let _ = r.opaque_fixed(8)?; // verifier
                            Vec::new()
                        }
                        _ => return Err(NfsError::Xdr(XdrError::Invalid("createmode"))),
                    };
                    (mode, attrs)
                } else {
                    (0, Vec::new())
                };
                let claim_type = r.u32()?;
                let filename = match claim_type {
                    0 => r.string()?.to_vec(), // CLAIM_NULL
                    1 => Vec::new(),           // CLAIM_PREVIOUS
                    _ => return Err(NfsError::Xdr(XdrError::Invalid("claim_type"))),
                };
                Op::Open {
                    clientid,
                    owner,
                    flags,
                    opentype,
                    createmode,
                    createattrs,
                    claim_type,
                    filename,
                }
            }
            OP_CREATE => {
                let ftype = r.u32()?;
                let linkdata = match ftype {
                    NF4LNK => r.string()?.to_vec(),
                    3 | 4 => {
                        // NF4BLK/NF4CHR: specdata4
                        let _ = r.u32()?;
                        let _ = r.u32()?;
                        Vec::new()
                    }
                    _ => Vec::new(),
                };
                let name = r.string()?.to_vec();
                let attrs = parse_fattr(r)?;
                Op::Create {
                    ftype,
                    linkdata,
                    name,
                    attrs,
                }
            }
            OP_REMOVE => Op::Remove(r.string()?.to_vec()),
            OP_RENAME => {
                let old = r.string()?.to_vec();
                let new = r.string()?.to_vec();
                Op::Rename { old, new }
            }
            OP_LINK => Op::Link(r.string()?.to_vec()),
            OP_SAVEFH => Op::SaveFh,
            OP_RESTOREFH => Op::RestoreFh,
            OP_SETATTR => {
                let _stateid = StateId::decode(r)?;
                let attrs = parse_fattr(r)?;
                Op::SetAttr { attrs }
            }
            OP_WRITE => {
                let _stateid = StateId::decode(r)?;
                let offset = r.u64()?;
                let stable = r.u32()?;
                let data = r.opaque()?.to_vec();
                Op::Write {
                    offset,
                    stable,
                    data,
                }
            }
            OP_COMMIT => {
                let offset = r.u64()?;
                let count = r.u32()?;
                Op::Commit { offset, count }
            }
            OP_SETCLIENTID => {
                // nfs_client_id4: verifier + id
                let v = r.opaque_fixed(8)?;
                let mut verifier = [0u8; 8];
                verifier.copy_from_slice(v);
                let client_name = r.opaque()?.to_vec();
                // cb_client4: program + location (netid, addr)
                let callback_prog = r.u32()?;
                let netid = String::from_utf8_lossy(r.string()?).to_string();
                let addr = String::from_utf8_lossy(r.string()?).to_string();
                // callback_ident
                let _ = r.u32()?;
                Op::SetClientId {
                    verifier,
                    client_name,
                    callback_prog,
                    netid,
                    addr,
                }
            }
            OP_SETCLIENTID_CONFIRM => {
                let clientid = r.u64()?;
                let v = r.opaque_fixed(8)?;
                let mut verifier = [0u8; 8];
                verifier.copy_from_slice(v);
                Op::SetClientIdConfirm { clientid, verifier }
            }
            OP_CLOSE => {
                let seqid = r.u32()?;
                let stateid = StateId::decode(r)?;
                Op::Close { seqid, stateid }
            }
            OP_LOCK => {
                let locktype = r.u32()?;
                let reclaim = r.bool()?;
                let offset = r.u64()?;
                let length = r.u64()?;
                let new_lock_owner = r.bool()?;
                let (open_seqid, open_stateid, lock_seqid, lock_stateid, lock_owner) =
                    if new_lock_owner {
                        let os = r.u32()?;
                        let ost = StateId::decode(r)?;
                        let ls = r.u32()?;
                        let _lock_clientid = r.u64()?;
                        let owner = r.opaque()?.to_vec();
                        (
                            os,
                            ost,
                            ls,
                            StateId {
                                seqid: 0,
                                other: [0u8; 12],
                            },
                            owner,
                        )
                    } else {
                        let ls = r.u32()?;
                        let lst = StateId::decode(r)?;
                        (
                            0,
                            StateId {
                                seqid: 0,
                                other: [0u8; 12],
                            },
                            ls,
                            lst,
                            Vec::new(),
                        )
                    };
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
                }
            }
            OP_LOCKU => {
                let locktype = r.u32()?;
                let seqid = r.u32()?;
                let stateid = StateId::decode(r)?;
                let offset = r.u64()?;
                let length = r.u64()?;
                Op::LockU {
                    locktype,
                    seqid,
                    stateid,
                    offset,
                    length,
                }
            }
            OP_RENEW => {
                let clientid = r.u64()?;
                Op::Renew { clientid }
            }
            n => return Err(NfsError::BadOp(n)),
        };
        Ok(op)
    }

    pub fn number(&self) -> u32 {
        match self {
            Op::PutFh(_) => OP_PUTFH,
            Op::PutRootFh => OP_PUTROOTFH,
            Op::GetFh => OP_GETFH,
            Op::Lookup(_) => OP_LOOKUP,
            Op::LookupP => OP_LOOKUPP,
            Op::GetAttr(_) => OP_GETATTR,
            Op::ReadDir { .. } => OP_READDIR,
            Op::Read { .. } => OP_READ,
            Op::Access { .. } => OP_ACCESS,
            Op::Open { .. } => OP_OPEN,
            Op::Create { .. } => OP_CREATE,
            Op::Remove(_) => OP_REMOVE,
            Op::Rename { .. } => OP_RENAME,
            Op::Link(_) => OP_LINK,
            Op::SaveFh => OP_SAVEFH,
            Op::RestoreFh => OP_RESTOREFH,
            Op::SetAttr { .. } => OP_SETATTR,
            Op::Write { .. } => OP_WRITE,
            Op::Commit { .. } => OP_COMMIT,
            Op::SetClientId { .. } => OP_SETCLIENTID,
            Op::SetClientIdConfirm { .. } => OP_SETCLIENTID_CONFIRM,
            Op::Close { .. } => OP_CLOSE,
            Op::Lock { .. } => OP_LOCK,
            Op::LockU { .. } => OP_LOCKU,
            Op::Renew { .. } => OP_RENEW,
        }
    }
}

// -- operation results -----------------------------------------------------

/// One operation's result: status + optional XDR body.
pub struct OpResult {
    pub opnum: u32,
    pub status: u32,
    pub body: Vec<u8>,
}

impl OpResult {
    pub fn ok(opnum: u32, body: Vec<u8>) -> Self {
        OpResult {
            opnum,
            status: NFS4_OK,
            body,
        }
    }

    pub fn err(opnum: u32, status: u32) -> Self {
        OpResult {
            opnum,
            status,
            body: Vec::new(),
        }
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.opnum);
        w.u32(self.status);
        if self.status == NFS4_OK {
            w.raw(&self.body);
        }
    }
}

/// Encode a full COMPOUND response.
pub fn encode_compound(tag: &[u8], overall: u32, results: &[OpResult]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(overall);
    w.string(tag);
    w.u32(results.len() as u32);
    for r in results {
        r.encode(&mut w);
    }
    w.into_bytes()
}

// -- attributes ------------------------------------------------------------

/// Attribute values for one file, encoded per the request mask.
pub struct AttrValues {
    pub mask: AttrMask,
    pub values: Vec<u8>, // XDR-encoded attrlist
}

/// File attributes for GETATTR/READDIR encoding.
pub struct FileAttrs {
    pub ftype: u32,
    pub size: u64,
    pub fileid: u64,
    pub mode: u32,
    pub nlink: u32,
    pub fsid_major: u64,
    pub fsid_minor: u64,
    /// Wire encoding of this object's filehandle (for FATTR4_FILEHANDLE).
    pub fh: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl AttrValues {
    /// Encode `requested` attributes of an inode. Unknown/unset
    /// attributes are silently omitted from the returned mask.
    pub fn encode(requested: &AttrMask, a: &FileAttrs) -> Self {
        // macOS sends an empty GETATTR mask as an fh-validity probe. Returning
        // zero attrs makes xnu's nfs_loadattrcache mark the vnode type as 0
        // (VNON), which later surfaces as ESTALE on open/readdir. Treat an
        // empty mask as a request for TYPE so the vnode type stays stable.
        let empty = requested.words.iter().all(|w| *w == 0);
        let mut out_mask = AttrMask { words: vec![0; 2] };
        let mut w = Writer::new();
        let mut set = |attr: u32, body: &mut dyn FnMut(&mut Writer)| {
            let word = (attr / 32) as usize;
            let bit = attr % 32;
            if word < out_mask.words.len() {
                out_mask.words[word] |= 1 << bit;
            }
            body(&mut w);
        };

        if empty || requested.wants(FATTR4_TYPE) {
            set(FATTR4_TYPE, &mut |w| w.u32(a.ftype));
        }
        if requested.wants(FATTR4_SIZE) {
            set(FATTR4_SIZE, &mut |w| w.u64(a.size));
        }
        if requested.wants(FATTR4_LINK_SUPPORT) {
            set(FATTR4_LINK_SUPPORT, &mut |w| w.u32(1)); // hard links supported
        }
        if requested.wants(FATTR4_FSID) {
            set(FATTR4_FSID, &mut |w| {
                w.u64(a.fsid_major);
                w.u64(a.fsid_minor);
            });
        }
        if requested.wants(FATTR4_FILEHANDLE) {
            set(FATTR4_FILEHANDLE, &mut |w| w.opaque(&a.fh));
        }
        if requested.wants(FATTR4_FILEID) {
            set(FATTR4_FILEID, &mut |w| w.u64(a.fileid));
        }
        if requested.wants(FATTR4_MODE) {
            set(FATTR4_MODE, &mut |w| w.u32(a.mode));
        }
        // NB: attr values must be encoded in increasing attribute number
        // order (RFC 7530 §5.5); NUMLINKS=35 sits between MODE=33 and OWNER=36.
        if requested.wants(FATTR4_NUMLINKS) {
            set(FATTR4_NUMLINKS, &mut |w| w.u32(a.nlink));
        }
        if requested.wants(FATTR4_OWNER) {
            let s = a.uid.to_string();
            set(FATTR4_OWNER, &mut |w| w.string(s.as_bytes()));
        }
        if requested.wants(FATTR4_OWNER_GROUP) {
            let s = a.gid.to_string();
            set(FATTR4_OWNER_GROUP, &mut |w| w.string(s.as_bytes()));
        }
        if requested.wants(FATTR4_SPACE_USED) {
            set(FATTR4_SPACE_USED, &mut |w| {
                w.u64(a.size.div_ceil(4096) * 4096)
            });
        }
        if requested.wants(FATTR4_TIME_ACCESS) {
            set(FATTR4_TIME_ACCESS, &mut |w| {
                w.i64(a.atime as i64);
                w.u32(0);
            });
        }
        if requested.wants(FATTR4_TIME_METADATA) {
            set(FATTR4_TIME_METADATA, &mut |w| {
                w.i64(a.ctime as i64);
                w.u32(0);
            });
        }
        if requested.wants(FATTR4_TIME_MODIFY) {
            set(FATTR4_TIME_MODIFY, &mut |w| {
                w.i64(a.mtime as i64);
                w.u32(0);
            });
        }
        if requested.wants(FATTR4_MOUNTED_ON_FILEID) {
            set(FATTR4_MOUNTED_ON_FILEID, &mut |w| w.u64(a.fileid));
        }

        AttrValues {
            mask: out_mask,
            values: w.into_bytes(),
        }
    }

    pub fn encode_result(&self, w: &mut Writer) {
        self.mask.encode(w);
        w.opaque(&self.values);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filehandle_roundtrip() {
        let fh = FileHandle {
            fs_uuid: [7u8; 16],
            inode: 42,
        };
        let mut w = Writer::new();
        fh.encode(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(FileHandle::decode(&mut r).unwrap(), fh);
    }

    #[test]
    fn test_lock_decode() {
        let mut w = Writer::new();
        w.u32(OP_LOCK);
        w.u32(2); // WRITE_LT
        w.bool(false); // reclaim
        w.u64(0); // offset
        w.u64(1000); // length
        w.bool(true); // new_lock_owner
        w.u32(0); // open seqid
        w.u32(0);
        w.opaque_fixed(&[1u8; 12]);
        w.u32(0); // lock seqid
        w.u64(1); // lock_owner clientid
        w.opaque(b"owner1");
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        let op = Op::decode(&mut r).unwrap();
        match op {
            Op::Lock {
                locktype,
                offset,
                length,
                ..
            } => {
                assert_eq!(locktype, 2);
                assert_eq!(offset, 0);
                assert_eq!(length, 1000);
            }
            _ => panic!("wrong op: {op:?}"),
        }
    }

    fn test_attrs() -> FileAttrs {
        FileAttrs {
            ftype: NF4DIR,
            size: 1234,
            fileid: 7,
            mode: 0o755,
            nlink: 3,
            fsid_major: 0x1111_2222_3333_4444,
            fsid_minor: 0x5555_6666_7777_8888,
            fh: vec![0xAA; 32],
            uid: 1000,
            gid: 1000,
            atime: 111,
            mtime: 222,
            ctime: 333,
        }
    }

    fn req_mask(attrs: &[u32]) -> AttrMask {
        let mut m = AttrMask { words: vec![0, 0] };
        for &a in attrs {
            m.words[(a / 32) as usize] |= 1 << (a % 32);
        }
        m
    }

    /// Regression test for the macOS mount failure ("RPC struct is bad"):
    /// FATTR4_NUMLINKS is 35, bit 5 is FATTR4_LINK_SUPPORT (bool). The old
    /// code had NUMLINKS=5 and wrote the link count into the LINK_SUPPORT
    /// slot, which strict clients reject (bool must be 0/1).
    #[test]
    fn link_support_and_numlinks_slots() {
        let av = AttrValues::encode(
            &req_mask(&[
                FATTR4_LINK_SUPPORT,
                FATTR4_FILEID,
                FATTR4_MODE,
                FATTR4_NUMLINKS,
            ]),
            &test_attrs(),
        );
        assert!(av.mask.wants(FATTR4_LINK_SUPPORT));
        assert!(av.mask.wants(FATTR4_NUMLINKS));
        let mut r = Reader::new(&av.values);
        // Attrs in increasing number order: 5, 20, 33, 35.
        assert_eq!(r.u32().unwrap(), 1); // LINK_SUPPORT = true
        assert_eq!(r.u64().unwrap(), 7); // FILEID
        assert_eq!(r.u32().unwrap(), 0o755); // MODE
        assert_eq!(r.u32().unwrap(), 3); // NUMLINKS = nlink
        assert!(r.is_empty());
    }

    /// Attr values must be in increasing attribute-number order (RFC 7530 §5.5).
    #[test]
    fn time_attrs_in_numeric_order() {
        let av = AttrValues::encode(
            &req_mask(&[FATTR4_TIME_ACCESS, FATTR4_TIME_METADATA, FATTR4_TIME_MODIFY]),
            &test_attrs(),
        );
        let mut r = Reader::new(&av.values);
        assert_eq!(r.i64().unwrap(), 111); // TIME_ACCESS
        assert_eq!(r.u32().unwrap(), 0);
        assert_eq!(r.i64().unwrap(), 333); // TIME_METADATA
        assert_eq!(r.u32().unwrap(), 0);
        assert_eq!(r.i64().unwrap(), 222); // TIME_MODIFY
        assert_eq!(r.u32().unwrap(), 0);
        assert!(r.is_empty());
    }

    /// FSID (attr 8) is a REQUIRED attribute; it must be returned when
    /// requested and sit between LINK_SUPPORT(5) and FILEID(20) on the wire.
    #[test]
    fn fsid_returned_in_order() {
        let av = AttrValues::encode(
            &req_mask(&[FATTR4_LINK_SUPPORT, FATTR4_FSID, FATTR4_FILEID]),
            &test_attrs(),
        );
        assert!(av.mask.wants(FATTR4_FSID));
        let mut r = Reader::new(&av.values);
        assert_eq!(r.u32().unwrap(), 1); // LINK_SUPPORT
        assert_eq!(r.u64().unwrap(), 0x1111_2222_3333_4444); // FSID major
        assert_eq!(r.u64().unwrap(), 0x5555_6666_7777_8888); // FSID minor
        assert_eq!(r.u64().unwrap(), 7); // FILEID
        assert!(r.is_empty());
    }

    /// macOS's mount probe requires FATTR4_FILEHANDLE in the GETATTR reply
    /// (xnu returns EBADRPC without it); it sits between FSID(8) and
    /// FILEID(20) on the wire.
    #[test]
    fn filehandle_returned_in_order() {
        let av = AttrValues::encode(
            &req_mask(&[FATTR4_FSID, FATTR4_FILEHANDLE, FATTR4_FILEID]),
            &test_attrs(),
        );
        assert!(av.mask.wants(FATTR4_FILEHANDLE));
        let mut r = Reader::new(&av.values);
        assert_eq!(r.u64().unwrap(), 0x1111_2222_3333_4444); // FSID major
        assert_eq!(r.u64().unwrap(), 0x5555_6666_7777_8888); // FSID minor
        let fh = r.opaque().unwrap();
        assert_eq!(fh, &[0xAA; 32]); // FILEHANDLE
        assert_eq!(r.u64().unwrap(), 7); // FILEID
        assert!(r.is_empty());
    }

    #[test]
    fn attr_mask_wants() {
        let mask = AttrMask {
            words: vec![(1 << 1) | (1 << 4), 1 << (33 - 32)],
        };
        assert!(mask.wants(FATTR4_TYPE));
        assert!(mask.wants(FATTR4_SIZE));
        assert!(mask.wants(FATTR4_MODE));
        assert!(!mask.wants(FATTR4_FILEID));
    }
}
