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

// Operation numbers.
pub const OP_ACCESS: u32 = 3;
pub const OP_GETATTR: u32 = 9;
pub const OP_GETFH: u32 = 10;
pub const OP_LOOKUP: u32 = 15;
pub const OP_LOOKUPP: u32 = 20;
pub const OP_PUTFH: u32 = 22;
pub const OP_PUTROOTFH: u32 = 24;
pub const OP_READ: u32 = 25;
pub const OP_READDIR: u32 = 26;

// Attribute numbers (RFC 7530 §5).
pub const FATTR4_TYPE: u32 = 1;
pub const FATTR4_SIZE: u32 = 4;
pub const FATTR4_NUMLINKS: u32 = 5;
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

// Access bits.
pub const ACCESS4_READ: u32 = 0x0001;
pub const ACCESS4_LOOKUP: u32 = 0x0002;
pub const ACCESS4_MODIFY: u32 = 0x0004;
pub const ACCESS4_EXTEND: u32 = 0x0008;
pub const ACCESS4_DELETE: u32 = 0x0010;
pub const ACCESS4_EXECUTE: u32 = 0x0020;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NfsError {
    Xdr(XdrError),
    /// Unknown operation number.
    BadOp(u32),
    /// Minor version != 0.
    BadMinor,
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
    pub fn encode(&self, w: &mut Writer) {
        let mut fh = [0u8; FH_LEN];
        fh[0..4].copy_from_slice(&FH_MAGIC.to_be_bytes());
        fh[4..20].copy_from_slice(&self.fs_uuid);
        fh[20..28].copy_from_slice(&self.inode.to_be_bytes());
        // gen = 0
        w.opaque(&fh);
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

impl Op {
    fn decode(r: &mut Reader) -> Result<Self, NfsError> {
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

        if requested.wants(FATTR4_TYPE) {
            set(FATTR4_TYPE, &mut |w| w.u32(a.ftype));
        }
        if requested.wants(FATTR4_SIZE) {
            set(FATTR4_SIZE, &mut |w| w.u64(a.size));
        }
        if requested.wants(FATTR4_NUMLINKS) {
            set(FATTR4_NUMLINKS, &mut |w| w.u32(a.nlink));
        }
        if requested.wants(FATTR4_FILEID) {
            set(FATTR4_FILEID, &mut |w| w.u64(a.fileid));
        }
        if requested.wants(FATTR4_MODE) {
            set(FATTR4_MODE, &mut |w| w.u32(a.mode));
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
        if requested.wants(FATTR4_TIME_MODIFY) {
            set(FATTR4_TIME_MODIFY, &mut |w| {
                w.i64(a.mtime as i64);
                w.u32(0);
            });
        }
        if requested.wants(FATTR4_TIME_METADATA) {
            set(FATTR4_TIME_METADATA, &mut |w| {
                w.i64(a.ctime as i64);
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
