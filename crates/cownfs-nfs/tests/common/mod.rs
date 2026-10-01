//! Shared userspace NFSv4 test client for the cownfs integration tests.
//!
//! Each test binary includes this module with:
//!
//! ```ignore
//! #[path = "common/mod.rs"]
//! mod common;
//! ```
//!
//! and then:
//!
//! ```ignore
//! let srv = common::spawn_server(4096);          // temp image + server on an ephemeral port
//! let mut c = common::NfsClient::connect(&srv.addr);
//! let mut ops = common::Ops::new();
//! ops.putfh(&srv.uuid, ROOT_INO);
//! ops.getattr(&[FATTR4_TYPE]);
//! let replies = c.check_ok(b"tag", ops);          // asserts every op returned NFS4_OK
//! ```
//!
//! The server under test is single-threaded (one connection at a time), so
//! each test uses exactly one `NfsClient` per spawned server.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cownfs_core::engine::Fs;
use cownfs_nfs::nfs4::{self, AttrMask, FileHandle};
use cownfs_nfs::rpc::{self, RecordReader};
use cownfs_nfs::xdr::{Reader, Writer};

// -- test server -----------------------------------------------------------

static IMG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A formatted image served on an ephemeral localhost port.
pub struct TestServer {
    pub addr: SocketAddr,
    pub uuid: [u8; 16],
    img: PathBuf,
}

/// Format a fresh image and serve it on 127.0.0.1:0. The listener is bound
/// before the serving thread spawns, so `NfsClient::connect` cannot race the
/// bind. The image file is removed when the server is dropped.
pub fn spawn_server(blocks: u64) -> TestServer {
    let n = IMG_COUNTER.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-it-{}-{n}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, blocks).expect("format test image");
    fs.commit().expect("commit fresh image");
    let uuid = fs.uuid();
    drop(fs);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener local addr");
    let img2 = img.clone();
    std::thread::spawn(move || {
        let fs = Fs::open(&img2).expect("open test image");
        let shared = cownfs_nfs::server::Shared::new(fs);
        let _ = cownfs_nfs::server::serve_listener(listener, &shared);
    });
    TestServer { addr, uuid, img }
}

/// Spawn a server on an ephemeral port that serves connections concurrently
/// (one thread per connection, shared filesystem and NFSv4 state).
/// The image file is removed when the server is dropped.
pub fn spawn_concurrent_server(blocks: u64) -> TestServer {
    let n = IMG_COUNTER.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-it-{}-{n}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, blocks).expect("format test image");
    fs.commit().expect("commit fresh image");
    let uuid = fs.uuid();
    drop(fs);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener local addr");
    let img2 = img.clone();
    std::thread::spawn(move || {
        let fs = Fs::open(&img2).expect("open test image");
        let shared = cownfs_nfs::server::Shared::new(fs);
        let _ = cownfs_nfs::server::serve_concurrent(listener, shared);
    });
    TestServer { addr, uuid, img }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.img);
    }
}

// -- replies ---------------------------------------------------------------

/// One directory entry from READDIR.
#[derive(Debug)]
pub struct Dirent {
    pub cookie: u64,
    pub name: Vec<u8>,
    /// Decoded (attrnum, raw XDR value) pairs.
    pub attrs: Vec<(u32, Vec<u8>)>,
}

/// Typed per-op reply.
#[derive(Debug)]
pub enum Reply {
    Ok,
    Err(u32),
    Fh(FileHandle),
    /// Decoded (attrnum, raw XDR value) pairs.
    Attrs(Vec<(u32, Vec<u8>)>),
    Dir(Vec<Dirent>),
    Read {
        eof: bool,
        data: Vec<u8>,
    },
    Written {
        count: u32,
        committed: u32,
    },
    Open {
        stateid: [u8; 16],
    },
    Lock {
        stateid: [u8; 16],
    },
    ClientId(u64),
    Access {
        supported: u32,
        granted: u32,
    },
    Secinfo(Vec<u32>),
}

impl Reply {
    /// The op's NFS status, or NFS4_OK for a successful typed reply.
    pub fn status(&self) -> u32 {
        match self {
            Reply::Err(s) => *s,
            _ => nfs4::NFS4_OK,
        }
    }

    pub fn expect_ok(&self) -> &Reply {
        assert_eq!(self.status(), nfs4::NFS4_OK, "op failed: {self:?}");
        self
    }
}

// -- attribute helpers -----------------------------------------------------

fn find_attr(attrs: &[(u32, Vec<u8>)], attr: u32) -> &[u8] {
    attrs
        .iter()
        .find(|(a, _)| *a == attr)
        .map(|(_, v)| v.as_slice())
        .unwrap_or_else(|| panic!("attr {attr} not present in {attrs:?}"))
}

pub fn has_attr(attrs: &[(u32, Vec<u8>)], attr: u32) -> bool {
    attrs.iter().any(|(a, _)| *a == attr)
}

/// Raw XDR attr value bytes (framing included for opaque/string attrs).
pub fn attr_raw(attrs: &[(u32, Vec<u8>)], attr: u32) -> &[u8] {
    find_attr(attrs, attr)
}

/// Raw XDR u32 attr value.
pub fn attr_u32(attrs: &[(u32, Vec<u8>)], attr: u32) -> u32 {
    let b = find_attr(attrs, attr);
    assert_eq!(b.len(), 4, "attr {attr} is not a u32");
    u32::from_be_bytes(b.try_into().unwrap())
}

/// Raw XDR u64 attr value.
pub fn attr_u64(attrs: &[(u32, Vec<u8>)], attr: u32) -> u64 {
    let b = find_attr(attrs, attr);
    assert_eq!(b.len(), 8, "attr {attr} is not a u64");
    u64::from_be_bytes(b.try_into().unwrap())
}

/// XDR string attr value (length prefix stripped).
pub fn attr_string(attrs: &[(u32, Vec<u8>)], attr: u32) -> String {
    let b = find_attr(attrs, attr);
    let mut r = Reader::new(b);
    String::from_utf8(r.string().expect("attr string").to_vec()).expect("attr utf8")
}

/// nfstime4 attr value: (seconds, nanoseconds).
pub fn attr_time(attrs: &[(u32, Vec<u8>)], attr: u32) -> (i64, u32) {
    let b = find_attr(attrs, attr);
    assert_eq!(b.len(), 12, "attr {attr} is not an nfstime4");
    (
        i64::from_be_bytes(b[..8].try_into().unwrap()),
        u32::from_be_bytes(b[8..12].try_into().unwrap()),
    )
}

/// Encode a u32 fattr value.
pub fn av_u32(v: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(v);
    w.into_bytes()
}

/// Encode a u64 fattr value.
pub fn av_u64(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(v);
    w.into_bytes()
}

/// Encode an XDR-string fattr value.
pub fn av_string(s: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(s);
    w.into_bytes()
}

/// Encode an nfstime4 fattr value (whole seconds).
pub fn av_time(secs: i64) -> Vec<u8> {
    let mut w = Writer::new();
    w.i64(secs);
    w.u32(0);
    w.into_bytes()
}

// -- op builder ------------------------------------------------------------

/// Builds the op list of a COMPOUND. Method names mirror the NFSv4 ops;
/// argument order follows this server's (simplified) wire format.
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

    fn op(&mut self) {
        self.n += 1;
    }

    pub fn putfh(&mut self, uuid: &[u8; 16], ino: u64) {
        self.w.u32(nfs4::OP_PUTFH);
        FileHandle {
            fs_uuid: *uuid,
            inode: ino,
        }
        .encode(&mut self.w);
        self.op();
    }

    /// PUTFH with caller-supplied opaque bytes (e.g. a bad-magic handle).
    /// `fh` must already be the full `opaque<>` encoding.
    pub fn putfh_raw(&mut self, fh: &[u8]) {
        self.w.u32(nfs4::OP_PUTFH);
        self.w.raw(fh);
        self.op();
    }

    pub fn putrootfh(&mut self) {
        self.w.u32(nfs4::OP_PUTROOTFH);
        self.op();
    }

    pub fn getfh(&mut self) {
        self.w.u32(nfs4::OP_GETFH);
        self.op();
    }

    pub fn lookup(&mut self, name: &[u8]) {
        self.w.u32(nfs4::OP_LOOKUP);
        self.w.string(name);
        self.op();
    }

    pub fn lookupp(&mut self) {
        self.w.u32(nfs4::OP_LOOKUPP);
        self.op();
    }

    fn attr_mask(&mut self, attrs: &[u32]) {
        let mut words = vec![0u32; 2];
        for a in attrs {
            let wi = (*a / 32) as usize;
            if wi >= words.len() {
                words.resize(wi + 1, 0);
            }
            words[wi] |= 1 << (*a % 32);
        }
        AttrMask { words }.encode(&mut self.w);
    }

    pub fn getattr(&mut self, attrs: &[u32]) {
        self.w.u32(nfs4::OP_GETATTR);
        self.attr_mask(attrs);
        self.op();
    }

    pub fn readdir(&mut self, cookie: u64, maxcount: u32, attrs: &[u32]) {
        self.w.u32(nfs4::OP_READDIR);
        self.w.u64(cookie);
        self.w.raw(&[0u8; 8]); // cookieverf
        self.w.u32(0); // dircount (no hint)
        self.w.u32(maxcount);
        self.attr_mask(attrs);
        self.op();
    }

    pub fn read(&mut self, offset: u64, count: u32) {
        self.w.u32(nfs4::OP_READ);
        self.w.u32(0); // stateid seqid
        self.w.raw(&[0u8; 12]); // stateid other
        self.w.u64(offset);
        self.w.u32(count);
        self.op();
    }

    pub fn access(&mut self, bits: u32) {
        self.w.u32(nfs4::OP_ACCESS);
        self.w.u32(bits);
        self.op();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        clientid: u64,
        owner: &[u8],
        flags: u32,
        opentype: u32,
        createmode: u32,
        createattrs: &[(u32, Vec<u8>)],
        claim: u32,
        filename: &[u8],
    ) {
        // RFC 7530 §16.16: seqid, share_access, share_deny, then
        // open_owner4 { clientid, owner }. `flags` packs share_access in
        // the low 2 bits and share_deny in bits[5:4].
        self.w.u32(nfs4::OP_OPEN);
        self.w.u32(1); // seqid
        self.w.u32(flags & 0x3); // share_access
        self.w.u32((flags >> 4) & 0x3); // share_deny
        self.w.u64(clientid);
        self.w.opaque(owner);
        self.w.u32(opentype);
        if opentype == nfs4::OPEN4_CREATE {
            self.w.u32(createmode);
            self.fattr(createattrs);
        }
        self.w.u32(claim);
        if claim == 0 {
            self.w.string(filename); // CLAIM_NULL
        }
        self.op();
    }

    /// OPEN+CREATE (UNCHECKED4) with a mode attr; flags = share_access (low 2
    /// bits) | share_deny << 4, e.g. 3 = read+write access, no deny.
    pub fn open_create(&mut self, clientid: u64, owner: &[u8], flags: u32, name: &[u8], mode: u32) {
        self.open(
            clientid,
            owner,
            flags,
            nfs4::OPEN4_CREATE,
            nfs4::UNCHECKED4,
            &[(nfs4::FATTR4_MODE, av_u32(mode))],
            0,
            name,
        );
    }

    pub fn open_nocreate(&mut self, clientid: u64, owner: &[u8], flags: u32, name: &[u8]) {
        self.open(
            clientid,
            owner,
            flags,
            nfs4::OPEN4_NOCREATE,
            0,
            &[],
            0,
            name,
        );
    }

    /// OPEN+CREATE with EXCLUSIVE4 mode and an 8-byte verifier (RFC 7530 §16.16).
    pub fn open_exclusive(
        &mut self,
        clientid: u64,
        owner: &[u8],
        flags: u32,
        name: &[u8],
        verifier: &[u8; 8],
    ) {
        self.w.u32(nfs4::OP_OPEN);
        self.w.u32(1); // seqid
        self.w.u32(flags & 0x3); // share_access
        self.w.u32((flags >> 4) & 0x3); // share_deny
        self.w.u64(clientid);
        self.w.opaque(owner);
        self.w.u32(nfs4::OPEN4_CREATE);
        self.w.u32(nfs4::EXCLUSIVE4);
        self.w.raw(verifier); // 8-byte verifier
        self.w.u32(0); // CLAIM_NULL
        self.w.string(name);
        self.op();
    }

    /// Encode an fattr4: bitmap words + values. Values MUST be written in
    /// ascending attribute-number order (RFC 7530 15.1.2), so the caller's
    /// pairs are sorted; callers may pass them in any order.
    fn fattr(&mut self, attrs: &[(u32, Vec<u8>)]) {
        let mut sorted: Vec<&(u32, Vec<u8>)> = attrs.iter().collect();
        sorted.sort_by_key(|(a, _)| *a);
        self.attr_mask(&sorted.iter().map(|(a, _)| *a).collect::<Vec<_>>());
        let mut av = Writer::new();
        for (_, v) in sorted {
            av.raw(v);
        }
        self.w.opaque(&av.into_bytes());
    }

    pub fn create_dir(&mut self, name: &[u8], mode: u32) {
        self.w.u32(nfs4::OP_CREATE);
        self.w.u32(nfs4::NF4DIR);
        self.w.string(name);
        self.fattr(&[(nfs4::FATTR4_MODE, av_u32(mode))]);
        self.op();
    }

    pub fn create_symlink(&mut self, name: &[u8], target: &[u8]) {
        self.w.u32(nfs4::OP_CREATE);
        self.w.u32(nfs4::NF4LNK);
        self.w.string(target);
        self.w.string(name);
        self.fattr(&[]);
        self.op();
    }

    /// CREATE with a caller-chosen file type. `linkdata` is only written for
    /// NF4LNK (symlink target).
    pub fn create_raw(&mut self, ftype: u32, linkdata: Option<&[u8]>, name: &[u8]) {
        self.w.u32(nfs4::OP_CREATE);
        self.w.u32(ftype);
        if ftype == nfs4::NF4LNK {
            self.w.string(linkdata.unwrap_or(b""));
        }
        self.w.string(name);
        self.fattr(&[]);
        self.op();
    }

    pub fn remove(&mut self, name: &[u8]) {
        self.w.u32(nfs4::OP_REMOVE);
        self.w.string(name);
        self.op();
    }

    pub fn secinfo(&mut self, name: &[u8]) {
        self.w.u32(nfs4::OP_SECINFO);
        self.w.string(name);
        self.op();
    }

    pub fn rename(&mut self, old: &[u8], new: &[u8]) {
        self.w.u32(nfs4::OP_RENAME);
        self.w.string(old);
        self.w.string(new);
        self.op();
    }

    pub fn link(&mut self, name: &[u8]) {
        self.w.u32(nfs4::OP_LINK);
        self.w.string(name);
        self.op();
    }

    pub fn savefh(&mut self) {
        self.w.u32(nfs4::OP_SAVEFH);
        self.op();
    }

    pub fn restorefh(&mut self) {
        self.w.u32(nfs4::OP_RESTOREFH);
        self.op();
    }

    pub fn setattr(&mut self, attrs: &[(u32, Vec<u8>)]) {
        self.w.u32(nfs4::OP_SETATTR);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]); // stateid
        self.fattr(attrs);
        self.op();
    }

    pub fn write(&mut self, offset: u64, stable: u32, data: &[u8]) {
        self.w.u32(nfs4::OP_WRITE);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]); // stateid
        self.w.u64(offset);
        self.w.u32(stable);
        self.w.opaque(data);
        self.op();
    }

    pub fn commit(&mut self) {
        self.w.u32(nfs4::OP_COMMIT);
        self.w.u64(0);
        self.w.u32(0);
        self.op();
    }

    pub fn setclientid(&mut self, verifier: &[u8; 8], name: &[u8]) {
        self.w.u32(nfs4::OP_SETCLIENTID);
        self.w.opaque_fixed(verifier);
        self.w.opaque(name);
        self.w.u32(0); // callback prog
        self.w.string(b"tcp");
        self.w.string(b"127.0.0.1");
        self.w.u32(0); // callback_ident
        self.op();
    }

    pub fn confirm(&mut self, clientid: u64, verifier: &[u8; 8]) {
        self.w.u32(nfs4::OP_SETCLIENTID_CONFIRM);
        self.w.u64(clientid);
        self.w.opaque_fixed(verifier);
        self.op();
    }

    pub fn close(&mut self, seqid: u32, stateid: &[u8; 16]) {
        self.w.u32(nfs4::OP_CLOSE);
        self.w.u32(seqid);
        self.w.raw(stateid);
        self.op();
    }

    /// LOCK with a new lock owner (lock_owner4 form).
    pub fn lock_new(
        &mut self,
        clientid: u64,
        locktype: u32,
        offset: u64,
        length: u64,
        open_stateid: &[u8; 16],
        owner: &[u8],
    ) {
        self.w.u32(nfs4::OP_LOCK);
        self.w.u32(locktype);
        self.w.bool(false); // reclaim
        self.w.u64(offset);
        self.w.u64(length);
        self.w.bool(true); // new lock owner
        self.w.u32(0); // open seqid
        self.w.raw(open_stateid);
        self.w.u32(0); // lock seqid
        self.w.u64(clientid); // lock_owner clientid
        self.w.opaque(owner);
        self.op();
    }

    pub fn locku(
        &mut self,
        locktype: u32,
        seqid: u32,
        stateid: &[u8; 16],
        offset: u64,
        length: u64,
    ) {
        self.w.u32(nfs4::OP_LOCKU);
        self.w.u32(locktype);
        self.w.u32(seqid);
        self.w.raw(stateid);
        self.w.u64(offset);
        self.w.u64(length);
        self.op();
    }

    pub fn renew(&mut self, clientid: u64) {
        self.w.u32(nfs4::OP_RENEW);
        self.w.u64(clientid);
        self.op();
    }

    /// An op number the server does not implement (no body).
    pub fn bogus_op(&mut self, opnum: u32) {
        self.w.u32(opnum);
        self.op();
    }

    /// Op count + encoded ops (the COMPOUND body after tag/minorversion).
    pub fn finish(self) -> Vec<u8> {
        let mut out = Writer::new();
        out.u32(self.n);
        out.raw(&self.w.into_bytes());
        out.into_bytes()
    }
}

// -- client ----------------------------------------------------------------

/// Minimal blocking NFSv4 client over one TCP connection.
pub struct NfsClient {
    stream: TcpStream,
    rr: RecordReader,
    xid: u32,
}

impl NfsClient {
    pub fn connect(addr: &SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .expect("set read timeout");
        NfsClient {
            stream,
            rr: RecordReader::new(),
            xid: 0x4000,
        }
    }

    /// Send a COMPOUND with an explicit minor version. Returns the overall
    /// status plus one reply per executed op.
    pub fn call_raw(&mut self, tag: &[u8], minor: u32, ops: Vec<u8>) -> (u32, Vec<Reply>) {
        self.xid += 1;
        let xid = self.xid;
        let mut w = Writer::new();
        w.u32(xid);
        w.u32(0); // CALL
        w.u32(2); // RPC version
        w.u32(rpc::NFS_PROGRAM);
        w.u32(rpc::NFS_VERSION);
        w.u32(rpc::PROC_COMPOUND);
        // AUTH_SYS credential.
        w.u32(1);
        let mut cred = Writer::new();
        cred.u32(0);
        cred.string(b"test");
        cred.u32(0);
        cred.u32(0);
        cred.u32(0);
        w.opaque(&cred.into_bytes());
        // AUTH_NONE verifier.
        w.u32(0);
        w.u32(0);
        w.string(tag);
        w.u32(minor);
        w.raw(&ops);
        self.stream
            .write_all(&rpc::frame_record(&w.into_bytes()))
            .expect("write compound");

        let record = self.read_record();
        Self::decode(&record, xid)
    }

    /// Read one full RPC record, tolerating TCP fragmentation.
    fn read_record(&mut self) -> Vec<u8> {
        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = self.stream.read(&mut buf).expect("read rpc reply");
            assert!(n > 0, "server closed connection mid-reply");
            self.rr.feed(&buf[..n]);
            if let Some(record) = self.rr.next_record().expect("record decode") {
                return record;
            }
        }
    }

    /// Send a COMPOUND (minorversion 0). Returns overall status + replies.
    pub fn call(&mut self, tag: &[u8], ops: Ops) -> (u32, Vec<Reply>) {
        self.call_raw(tag, 0, ops.finish())
    }

    /// Like `call`, but asserts the overall status and every op is NFS4_OK.
    pub fn check_ok(&mut self, tag: &[u8], ops: Ops) -> Vec<Reply> {
        let (overall, res) = self.call(tag, ops);
        assert_eq!(overall, nfs4::NFS4_OK, "compound failed: {res:?}");
        for r in &res {
            r.expect_ok();
        }
        res
    }

    /// RPC NULL probe: the server must answer with a well-formed SUCCESS.
    pub fn rpc_null(&mut self) {
        self.xid += 1;
        let xid = self.xid;
        let mut w = Writer::new();
        w.u32(xid);
        w.u32(0);
        w.u32(2);
        w.u32(rpc::NFS_PROGRAM);
        w.u32(rpc::NFS_VERSION);
        w.u32(rpc::PROC_NULL);
        w.u32(0); // AUTH_NONE
        w.u32(0);
        w.u32(0); // verifier
        w.u32(0);
        self.stream
            .write_all(&rpc::frame_record(&w.into_bytes()))
            .expect("write null");
        let record = self.read_record();
        let mut r = Reader::new(&record);
        assert_eq!(r.u32().expect("xid"), xid);
        assert_eq!(r.u32().expect("reply"), 1);
        assert_eq!(r.u32().expect("accepted"), 0);
        assert_eq!(r.u32().expect("verifier flavor"), 0);
        let _ = r.opaque().expect("verifier body");
        assert_eq!(r.u32().expect("accept_stat"), 0);
    }

    fn decode(record: &[u8], xid: u32) -> (u32, Vec<Reply>) {
        let mut r = Reader::new(record);
        assert_eq!(r.u32().expect("xid"), xid);
        assert_eq!(r.u32().expect("REPLY"), 1);
        assert_eq!(r.u32().expect("MSG_ACCEPTED"), 0);
        assert_eq!(r.u32().expect("verifier flavor"), 0);
        let _ = r.opaque().expect("verifier body");
        assert_eq!(r.u32().expect("SUCCESS"), 0);
        let overall = r.u32().expect("overall status");
        let _ = r.string().expect("tag");
        let n = r.u32().expect("op count") as usize;
        // Never pre-allocate from a wire count: a corrupt length must not
        // turn into a multi-gigabyte allocation.
        let mut out = Vec::new();
        for _ in 0..n {
            let opnum = r.u32().expect("opnum");
            let status = r.u32().expect("status");
            if status != nfs4::NFS4_OK {
                out.push(Reply::Err(status));
                continue;
            }
            out.push(match opnum {
                nfs4::OP_GETFH => Reply::Fh(FileHandle::decode(&mut r).expect("fh")),
                nfs4::OP_GETATTR => Reply::Attrs(nfs4::parse_fattr(&mut r).expect("getattr attrs")),
                nfs4::OP_READDIR => {
                    let _ = r.u64().expect("cookieverf");
                    let mut entries = Vec::new();
                    while r.bool().expect("value_follows") {
                        let cookie = r.u64().expect("cookie");
                        let name = r.opaque().expect("name").to_vec();
                        let attrs = nfs4::parse_fattr(&mut r).expect("entry attrs");
                        entries.push(Dirent {
                            cookie,
                            name,
                            attrs,
                        });
                    }
                    let _ = r.bool().expect("eof");
                    Reply::Dir(entries)
                }
                nfs4::OP_READ => {
                    let eof = r.bool().expect("eof");
                    let data = r.opaque().expect("data").to_vec();
                    Reply::Read { eof, data }
                }
                nfs4::OP_WRITE => {
                    let count = r.u32().expect("count");
                    let committed = r.u32().expect("committed");
                    let _ = r.opaque_fixed(8).expect("verifier");
                    Reply::Written { count, committed }
                }
                nfs4::OP_ACCESS => {
                    let supported = r.u32().expect("supported");
                    let granted = r.u32().expect("granted");
                    Reply::Access { supported, granted }
                }
                nfs4::OP_OPEN => {
                    let sid = r.opaque_fixed(16).expect("stateid");
                    let mut stateid = [0u8; 16];
                    stateid.copy_from_slice(sid);
                    // OPEN4resok: change_info4(bool + u64 + u64), rflags,
                    // attrset bitmap, delegation type.
                    let _ = r.bool().expect("atomic");
                    let _ = r.u64().expect("before");
                    let _ = r.u64().expect("after");
                    let _ = r.u32().expect("rflags");
                    let nattr = r.u32().expect("attrset len") as usize;
                    for _ in 0..nattr {
                        let _ = r.u32().expect("attrset word");
                    }
                    let _ = r.u32().expect("delegation");
                    Reply::Open { stateid }
                }
                nfs4::OP_LOCK => {
                    let sid = r.opaque_fixed(16).expect("stateid");
                    let mut stateid = [0u8; 16];
                    stateid.copy_from_slice(sid);
                    Reply::Lock { stateid }
                }
                nfs4::OP_SETCLIENTID => {
                    let clientid = r.u64().expect("clientid");
                    let _ = r.opaque_fixed(8).expect("verifier");
                    Reply::ClientId(clientid)
                }
                nfs4::OP_CREATE => {
                    // change_info4: atomic + before + after + attrset.
                    let _ = r.bool().expect("atomic");
                    let _ = r.u64().expect("before");
                    let _ = r.u64().expect("after");
                    let nattr = r.u32().expect("attrset len") as usize;
                    for _ in 0..nattr {
                        let _ = r.u32().expect("attrset word");
                    }
                    Reply::Ok
                }
                nfs4::OP_SECINFO => {
                    let n = r.u32().expect("secinfo count") as usize;
                    let mut flavors = Vec::with_capacity(n);
                    for _ in 0..n {
                        let flavor = r.u32().expect("flavor");
                        // secinfo4 is a discriminated union (RFC 7530 §16.31.3):
                        // only RPCSEC_GSS (6) carries flavor_info; AUTH_SYS (1)
                        // and AUTH_NONE (0) take the void arm.
                        if flavor == 6 {
                            let _ = r.opaque().expect("gss flavor_info");
                        }
                        flavors.push(flavor);
                    }
                    Reply::Secinfo(flavors)
                }
                _ => Reply::Ok,
            });
        }
        (overall, out)
    }

    // -- convenience flows -------------------------------------------------

    /// PUTFH(dir) + LOOKUP(name) + GETFH. Ok(handle) or the LOOKUP status.
    pub fn lookup_fh(
        &mut self,
        uuid: &[u8; 16],
        dir_ino: u64,
        name: &[u8],
    ) -> Result<FileHandle, u32> {
        let mut ops = Ops::new();
        ops.putfh(uuid, dir_ino);
        ops.lookup(name);
        ops.getfh();
        let (_, res) = self.call(b"lookup_fh", ops);
        match res.as_slice() {
            [_, _, Reply::Fh(fh)] => Ok(fh.clone()),
            [_, Reply::Err(st)] => Err(*st),
            [Reply::Err(st), ..] => Err(*st),
            _ => panic!("unexpected lookup_fh reply: {res:?}"),
        }
    }

    /// PUTFH(ino) + GETATTR. Ok(attrs) or the GETATTR status.
    pub fn getattr(
        &mut self,
        uuid: &[u8; 16],
        ino: u64,
        attrs: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>, u32> {
        let mut ops = Ops::new();
        ops.putfh(uuid, ino);
        ops.getattr(attrs);
        let (_, res) = self.call(b"getattr", ops);
        match res.as_slice() {
            [_, Reply::Attrs(a)] => Ok(a.clone()),
            [_, Reply::Err(st)] => Err(*st),
            [Reply::Err(st), ..] => Err(*st),
            _ => panic!("unexpected getattr reply: {res:?}"),
        }
    }

    /// Full READDIR walk using cookies (works regardless of maxcount
    /// truncation). Returns all entries except "." and "..".
    pub fn readdir_all(
        &mut self,
        uuid: &[u8; 16],
        ino: u64,
        maxcount: u32,
        attrs: &[u32],
    ) -> Vec<Dirent> {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        loop {
            let mut ops = Ops::new();
            ops.putfh(uuid, ino);
            ops.readdir(cookie, maxcount, attrs);
            let res = self.check_ok(b"readdir_all", ops);
            let entries = match &res[1] {
                Reply::Dir(e) => e,
                r => panic!("unexpected readdir reply: {r:?}"),
            };
            if entries.is_empty() {
                break;
            }
            cookie = entries.last().unwrap().cookie;
            out.extend(entries.iter().filter_map(|e| {
                if e.name == b"." || e.name == b".." {
                    None
                } else {
                    Some(Dirent {
                        cookie: e.cookie,
                        name: e.name.clone(),
                        attrs: e.attrs.clone(),
                    })
                }
            }));
        }
        out
    }

    /// Read `size` bytes from the start of a file.
    pub fn read_all(&mut self, uuid: &[u8; 16], ino: u64, size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0u64;
        while off < size {
            let mut ops = Ops::new();
            ops.putfh(uuid, ino);
            ops.read(off, 65536);
            let res = self.check_ok(b"read_all", ops);
            match &res[1] {
                Reply::Read { data, .. } => {
                    if data.is_empty() {
                        break;
                    }
                    off += data.len() as u64;
                    out.extend_from_slice(data);
                }
                r => panic!("unexpected read reply: {r:?}"),
            }
        }
        out
    }
}

/// Register + confirm an NFSv4 client; returns its clientid.
pub fn establish_client(c: &mut NfsClient, name: &[u8]) -> u64 {
    establish_client_verifier(c, &[0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8], name)
}

pub fn establish_client_verifier(c: &mut NfsClient, verifier: &[u8; 8], name: &[u8]) -> u64 {
    let mut ops = Ops::new();
    ops.setclientid(verifier, name);
    let res = c.check_ok(b"setclientid", ops);
    let id = match &res[0] {
        Reply::ClientId(id) => *id,
        r => panic!("setclientid failed: {r:?}"),
    };
    let mut ops = Ops::new();
    ops.confirm(id, verifier);
    c.check_ok(b"confirm", ops);
    id
}

/// OPEN+CREATE a file under `dir_ino`; returns the new file's inode.
pub fn create_file(
    c: &mut NfsClient,
    uuid: &[u8; 16],
    clientid: u64,
    dir_ino: u64,
    name: &[u8],
    mode: u32,
) -> u64 {
    let mut ops = Ops::new();
    ops.putfh(uuid, dir_ino);
    ops.open_create(clientid, b"owner", 3, name, mode);
    ops.getfh();
    let res = c.check_ok(b"create_file", ops);
    match &res[2] {
        Reply::Fh(fh) => fh.inode,
        r => panic!("create_file: unexpected reply {r:?}"),
    }
}
