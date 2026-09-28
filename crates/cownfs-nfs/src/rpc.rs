//! ONC RPC over TCP (RFC 5531): record marking, CALL/REPLY framing,
//! AUTH_SYS credential parsing. Only what NFSv4 needs.

use crate::xdr::{Reader, Writer, XdrError};

pub const RPC_VERSION: u32 = 2;
pub const NFS_PROGRAM: u32 = 100003;
pub const NFS_VERSION: u32 = 4;

pub const PROC_NULL: u32 = 0;
pub const PROC_COMPOUND: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    Xdr(XdrError),
    /// Unsupported RPC version.
    BadVersion,
    /// Not a CALL message.
    NotCall,
    /// Wrong program or version.
    BadProgram,
    /// Unsupported auth flavor (only AUTH_SYS and AUTH_NONE accepted).
    BadAuth,
    /// Record too large.
    RecordTooLarge,
}

impl From<XdrError> for RpcError {
    fn from(e: XdrError) -> Self {
        RpcError::Xdr(e)
    }
}

/// AUTH_SYS credential body (RFC 5531 §9.2).
#[derive(Debug, Clone)]
pub struct AuthSys {
    pub stamp: u32,
    pub machine: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

/// A decoded RPC CALL.
pub struct Call<'a> {
    pub xid: u32,
    pub proc: u32,
    pub auth: AuthSys,
    pub params: Reader<'a>,
}

impl<'a> Call<'a> {
    /// Decode an RPC CALL from one complete record's payload.
    pub fn decode(buf: &'a [u8]) -> Result<Self, RpcError> {
        let mut r = Reader::new(buf);
        let xid = r.u32()?;
        let msg_type = r.u32()?;
        if msg_type != 0 {
            return Err(RpcError::NotCall);
        }
        if r.u32()? != RPC_VERSION {
            return Err(RpcError::BadVersion);
        }
        if r.u32()? != NFS_PROGRAM || r.u32()? != NFS_VERSION {
            return Err(RpcError::BadProgram);
        }
        let proc = r.u32()?;
        let auth = decode_cred(&mut r)?;
        let _ = decode_opaque(&mut r)?; // reply verifier, ignore
        Ok(Call {
            xid,
            proc,
            auth,
            params: r,
        })
    }
}

fn decode_opaque(r: &mut Reader) -> Result<(), RpcError> {
    let flavor = r.u32()?;
    let body = r.opaque()?;
    let _ = (flavor, body);
    Ok(())
}

fn decode_cred(r: &mut Reader) -> Result<AuthSys, RpcError> {
    let flavor = r.u32()?;
    let body = r.opaque()?;
    match flavor {
        0 => Ok(AuthSys {
            // AUTH_NONE: treat as root, no groups.
            stamp: 0,
            machine: Vec::new(),
            uid: 0,
            gid: 0,
            gids: Vec::new(),
        }),
        1 => {
            let mut b = Reader::new(body);
            let stamp = b.u32()?;
            let machine = b.string()?.to_vec();
            let uid = b.u32()?;
            let gid = b.u32()?;
            let ngids = b.u32()? as usize;
            if ngids > 16 {
                return Err(RpcError::BadAuth);
            }
            let mut gids = Vec::with_capacity(ngids);
            for _ in 0..ngids {
                gids.push(b.u32()?);
            }
            b.end()?;
            Ok(AuthSys {
                stamp,
                machine,
                uid,
                gid,
                gids,
            })
        }
        _ => Err(RpcError::BadAuth),
    }
}

/// Encode a successful RPC REPLY carrying `results`.
pub fn encode_reply(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(xid);
    w.u32(1); // REPLY
    w.u32(0); // MSG_ACCEPTED
    w.u32(0); // AUTH_NONE verifier flavor
    w.u32(0); // verifier length
    w.u32(0); // SUCCESS
    w.raw(results);
    w.into_bytes()
}

/// Encode an RPC-level rejection (e.g. bad program/version).
pub fn encode_reject(xid: u32, accept_stat: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(xid);
    w.u32(1); // REPLY
    w.u32(0); // MSG_ACCEPTED
    w.u32(0);
    w.u32(0);
    w.u32(accept_stat);
    w.into_bytes()
}

/// Split a byte stream into TCP records (RFC 5531 §5): 4-byte header
/// with the high bit marking the last fragment.
#[derive(Default)]
pub struct RecordReader {
    buf: Vec<u8>,
    partial: Vec<u8>,
}

impl RecordReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Take the next complete record, if available.
    pub fn next_record(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        loop {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let hdr = u32::from_be_bytes(self.buf[..4].try_into().unwrap());
            let last = hdr & 0x8000_0000 != 0;
            let len = (hdr & 0x7fff_ffff) as usize;
            if len > 8 * 1024 * 1024 {
                return Err(RpcError::RecordTooLarge);
            }
            if self.buf.len() < 4 + len {
                return Ok(None);
            }
            self.partial.extend_from_slice(&self.buf[4..4 + len]);
            self.buf.drain(..4 + len);
            if last {
                return Ok(Some(std::mem::take(&mut self.partial)));
            }
        }
    }
}

/// Frame a reply payload as a single TCP record.
pub fn frame_record(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&((payload.len() as u32) | 0x8000_0000).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let mut rr = RecordReader::new();
        // Feed a record split across two feeds, in two fragments.
        let payload = b"hello-rpc";
        let mut frag1 = Vec::new();
        frag1.extend_from_slice(&(4u32.to_be_bytes())); // not last, len 4
        frag1.extend_from_slice(b"hell");
        let mut frag2 = Vec::new();
        frag2.extend_from_slice(&((5u32 | 0x8000_0000).to_be_bytes())); // last, len 5
        frag2.extend_from_slice(b"o-rpc");
        rr.feed(&frag1);
        assert!(rr.next_record().unwrap().is_none());
        rr.feed(&frag2);
        assert_eq!(rr.next_record().unwrap().unwrap(), payload);
    }
}
