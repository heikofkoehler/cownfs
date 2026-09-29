//! Minimal NFSv4 client for benchmarking: PUTFH/WRITE/READ/COMMIT round
//! trips against the userspace server. Trimmed-down port of the p5 test
//! client; the server is single-threaded and speaks the custom OPEN wire
//! layout, but plain PUTFH by (uuid, ino) needs no state setup.

use std::io::{Read, Write};
use std::net::TcpStream;

use cownfs_nfs::nfs4::{FileHandle, FILE_SYNC4, NFS4_OK, OP_COMMIT, OP_PUTFH, OP_READ, OP_WRITE};
use cownfs_nfs::rpc::{self, RecordReader};
use cownfs_nfs::xdr::{Reader, Writer};

pub struct Ops {
    w: Writer,
    n: u32,
}

impl Ops {
    pub fn new() -> Self {
        Ops {
            w: Writer::new(),
            n: 0,
        }
    }
    pub fn putfh(&mut self, uuid: &[u8; 16], ino: u64) {
        self.w.u32(OP_PUTFH);
        FileHandle {
            fs_uuid: *uuid,
            inode: ino,
        }
        .encode(&mut self.w);
        self.n += 1;
    }
    /// WRITE with an all-zero stateid (accepted by this server).
    pub fn write(&mut self, offset: u64, data: &[u8], stable: u32) {
        self.w.u32(OP_WRITE);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]);
        self.w.u64(offset);
        self.w.u32(stable);
        self.w.opaque(data);
        self.n += 1;
    }
    pub fn read(&mut self, offset: u64, count: u32) {
        self.w.u32(OP_READ);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]);
        self.w.u64(offset);
        self.w.u32(count);
        self.n += 1;
    }
    pub fn commit(&mut self) {
        self.w.u32(OP_COMMIT);
        self.w.u64(0);
        self.w.u32(0);
        self.n += 1;
    }
    fn finish(self) -> Vec<u8> {
        let mut out = Writer::new();
        out.u32(self.n);
        out.raw(&self.w.into_bytes());
        out.into_bytes()
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum R {
    Ok,
    Err(u32),
    Data(Vec<u8>),
    Written(u32),
}

pub struct Client {
    stream: TcpStream,
    rr: RecordReader,
    xid: u32,
}

impl Client {
    pub fn connect(addr: &str) -> Self {
        let mut stream = None;
        for _ in 0..100 {
            match TcpStream::connect(addr) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        Client {
            stream: stream.expect("connect to bench server"),
            rr: RecordReader::new(),
            xid: 0x4000,
        }
    }

    pub fn stable() -> u32 {
        FILE_SYNC4
    }

    /// One COMPOUND call; returns per-op results. Panics on transport error.
    pub fn call(&mut self, ops: Ops) -> Vec<R> {
        self.xid += 1;
        let xid = self.xid;
        let mut w = Writer::new();
        w.u32(xid);
        w.u32(0); // CALL
        w.u32(2); // RPC v2
        w.u32(rpc::NFS_PROGRAM);
        w.u32(rpc::NFS_VERSION);
        w.u32(rpc::PROC_COMPOUND);
        w.u32(1); // AUTH_NONE flavor
        let mut cred = Writer::new();
        cred.u32(0);
        cred.string(b"bench");
        cred.u32(0);
        cred.u32(0);
        cred.u32(0);
        w.opaque(&cred.into_bytes());
        w.u32(0); // verifier flavor
        w.u32(0); // verifier len
        w.string(b"bench");
        w.u32(0); // minorversion
        w.raw(&ops.finish());
        self.stream
            .write_all(&rpc::frame_record(&w.into_bytes()))
            .unwrap();

        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = self.stream.read(&mut buf).unwrap();
            assert!(n > 0, "server closed connection");
            self.rr.feed(&buf[..n]);
            if let Some(record) = self.rr.next_record().unwrap() {
                return Self::decode(&record, xid);
            }
        }
    }

    fn decode(record: &[u8], xid: u32) -> Vec<R> {
        let mut r = Reader::new(record);
        assert_eq!(r.u32().unwrap(), xid);
        assert_eq!(r.u32().unwrap(), 1); // REPLY
        assert_eq!(r.u32().unwrap(), 0); // MSG_ACCEPTED
        let _ = r.u32().unwrap(); // verifier flavor
        let _ = r.opaque().unwrap(); // verifier body
        assert_eq!(r.u32().unwrap(), 0); // SUCCESS
        let _ = r.u32().unwrap(); // accept state
        let _ = r.string().unwrap(); // tag
        let n = r.u32().unwrap() as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let opnum = r.u32().unwrap();
            let status = r.u32().unwrap();
            if status != NFS4_OK {
                out.push(R::Err(status));
                continue;
            }
            match opnum {
                OP_READ => {
                    let _ = r.bool().unwrap(); // eof
                    out.push(R::Data(r.opaque().unwrap().to_vec()));
                }
                OP_WRITE => {
                    let count = r.u32().unwrap();
                    let _ = r.u32().unwrap(); // committed
                    let _ = r.opaque_fixed(8).unwrap(); // verifier
                    out.push(R::Written(count));
                }
                OP_COMMIT => {
                    let _ = r.opaque_fixed(8).unwrap();
                    out.push(R::Ok);
                }
                _ => out.push(R::Ok),
            }
        }
        out
    }

    pub fn check(&mut self, ops: Ops, expect: usize) -> Vec<R> {
        let res = self.call(ops);
        assert_eq!(res.len(), expect, "expected {expect} ops, got {res:?}");
        for r in &res {
            assert!(
                matches!(r, R::Ok | R::Data(_) | R::Written(_)),
                "nfs op failed: {r:?}"
            );
        }
        res
    }
}
