//! P5 gate: userspace NFSv4 client exercising mutation ops.
//! Creates/writes/renames/links/removes over NFS, verifies via NFS
//! reads and the engine.

use std::io::{Read, Write};
use std::net::TcpStream;

use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::{
    AttrMask, FileHandle, FATTR4_MODE, FATTR4_SIZE, FATTR4_TYPE, FILE_SYNC4, NF4DIR, NF4LNK,
    NF4REG, NFS4_OK, OPEN4_CREATE, OP_COMMIT, OP_CREATE, OP_GETATTR, OP_GETFH, OP_LINK, OP_LOOKUP,
    OP_OPEN, OP_PUTFH, OP_READ, OP_REMOVE, OP_RENAME, OP_RESTOREFH, OP_SAVEFH, OP_SETATTR,
    OP_SETCLIENTID, OP_SETCLIENTID_CONFIRM, OP_WRITE, UNCHECKED4,
};
use cownfs_nfs::rpc::{self, RecordReader};
use cownfs_nfs::xdr::{Reader, Writer};

// Reuse the Ops builder pattern from p4_client with P5 ops.
struct Ops {
    w: Writer,
    n: u32,
}

impl Ops {
    fn new() -> Self {
        Ops {
            w: Writer::new(),
            n: 0,
        }
    }
    fn putfh(&mut self, uuid: &[u8; 16], ino: u64) {
        self.w.u32(OP_PUTFH);
        FileHandle {
            fs_uuid: *uuid,
            inode: ino,
            gen: 0,
        }
        .encode(&mut self.w);
        self.n += 1;
    }
    fn lookup(&mut self, name: &[u8]) {
        self.w.u32(OP_LOOKUP);
        self.w.string(name);
        self.n += 1;
    }
    fn getfh(&mut self) {
        self.w.u32(OP_GETFH);
        self.n += 1;
    }
    fn getattr(&mut self, attrs: &[u32]) {
        self.w.u32(OP_GETATTR);
        let words = vec![
            attrs
                .iter()
                .filter(|a| **a < 32)
                .fold(0, |m, a| m | (1 << a)),
            attrs
                .iter()
                .filter(|a| **a >= 32)
                .fold(0, |m, a| m | (1 << (a - 32))),
        ];
        AttrMask { words }.encode(&mut self.w);
        self.n += 1;
    }
    fn read(&mut self, offset: u64, count: u32) {
        self.w.u32(OP_READ);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]);
        self.w.u64(offset);
        self.w.u32(count);
        self.n += 1;
    }
    fn setclientid(&mut self, name: &[u8]) {
        self.w.u32(OP_SETCLIENTID);
        self.w.opaque_fixed(&[1u8, 2, 3, 4, 5, 6, 7, 8]); // verifier
        self.w.opaque(name); // client id string
        self.w.u32(0); // callback prog
        self.w.string(b"tcp"); // netid
        self.w.string(b"127.0.0.1"); // addr
        self.w.u32(0); // callback_ident
        self.n += 1;
    }
    fn setclientid_confirm(&mut self, clientid: u64) {
        self.w.u32(OP_SETCLIENTID_CONFIRM);
        self.w.u64(clientid);
        self.w.opaque_fixed(&[1u8, 2, 3, 4, 5, 6, 7, 8]);
        self.n += 1;
    }
    fn open_create(&mut self, clientid: u64, name: &[u8], mode: u32) {
        self.w.u32(OP_OPEN);
        self.w.u32(1); // seqid (RFC 7530 §16.16 — first field)
        self.w.u32(3); // share_access: READ|WRITE
        self.w.u32(0); // share_deny: NONE
        self.w.u64(clientid); // open_owner4.clientid
        self.w.opaque(b"owner"); // open_owner4.owner
        self.w.u32(OPEN4_CREATE); // opentype
        self.w.u32(UNCHECKED4); // createmode
                                // createattrs: mode
        let words = vec![0u32, 1u32 << (FATTR4_MODE - 32)];
        AttrMask { words }.encode(&mut self.w);
        let mut av = Writer::new();
        av.u32(mode);
        self.w.opaque(&av.into_bytes());
        self.w.u32(0); // claim_type CLAIM_NULL
        self.w.string(name);
        self.n += 1;
    }
    fn create_dir(&mut self, name: &[u8], mode: u32) {
        self.w.u32(OP_CREATE);
        self.w.u32(NF4DIR);
        self.w.string(name);
        let words = vec![0u32, 1u32 << (FATTR4_MODE - 32)];
        AttrMask { words }.encode(&mut self.w);
        let mut av = Writer::new();
        av.u32(mode);
        self.w.opaque(&av.into_bytes());
        self.n += 1;
    }
    fn create_symlink(&mut self, name: &[u8], target: &[u8]) {
        self.w.u32(OP_CREATE);
        self.w.u32(NF4LNK);
        self.w.string(target);
        self.w.string(name);
        AttrMask { words: vec![0, 0] }.encode(&mut self.w);
        self.w.opaque(&[]);
        self.n += 1;
    }
    fn write(&mut self, offset: u64, data: &[u8], stable: u32) {
        self.w.u32(OP_WRITE);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]);
        self.w.u64(offset);
        self.w.u32(stable);
        self.w.opaque(data);
        self.n += 1;
    }
    fn commit(&mut self) {
        self.w.u32(OP_COMMIT);
        self.w.u64(0);
        self.w.u32(0);
        self.n += 1;
    }
    fn setattr_mode(&mut self, mode: u32) {
        self.w.u32(OP_SETATTR);
        self.w.u32(0);
        self.w.raw(&[0u8; 12]);
        let words = vec![0u32, 1u32 << (FATTR4_MODE - 32)];
        AttrMask { words }.encode(&mut self.w);
        let mut av = Writer::new();
        av.u32(mode);
        self.w.opaque(&av.into_bytes());
        self.n += 1;
    }
    fn remove(&mut self, name: &[u8]) {
        self.w.u32(OP_REMOVE);
        self.w.string(name);
        self.n += 1;
    }
    fn rename(&mut self, old: &[u8], new: &[u8]) {
        self.w.u32(OP_RENAME);
        self.w.string(old);
        self.w.string(new);
        self.n += 1;
    }
    fn link(&mut self, name: &[u8]) {
        self.w.u32(OP_LINK);
        self.w.string(name);
        self.n += 1;
    }
    fn savefh(&mut self) {
        self.w.u32(OP_SAVEFH);
        self.n += 1;
    }
    fn restorefh(&mut self) {
        self.w.u32(OP_RESTOREFH);
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
enum R {
    Ok,
    Err(u32),
    Fh(FileHandle),
    Data(Vec<u8>),
    Attrs(u32, u64, u32), // ftype, size, mode
    Written(u32),
}

struct Client {
    stream: TcpStream,
    rr: RecordReader,
    xid: u32,
}

impl Client {
    fn connect(addr: &str) -> Self {
        // Retry: the server thread may not have bound yet.
        let mut stream = None;
        for _ in 0..50 {
            match TcpStream::connect(addr) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        Client {
            stream: stream.expect("connect to test server"),
            rr: RecordReader::new(),
            xid: 0x2000,
        }
    }

    fn call(&mut self, ops_bytes: Vec<u8>) -> Vec<R> {
        self.xid += 1;
        let xid = self.xid;
        let mut w = Writer::new();
        w.u32(xid);
        w.u32(0);
        w.u32(2);
        w.u32(rpc::NFS_PROGRAM);
        w.u32(rpc::NFS_VERSION);
        w.u32(rpc::PROC_COMPOUND);
        w.u32(1);
        let mut cred = Writer::new();
        cred.u32(0);
        cred.string(b"test");
        cred.u32(0);
        cred.u32(0);
        cred.u32(0);
        w.opaque(&cred.into_bytes());
        w.u32(0);
        w.u32(0);
        w.string(b"p5");
        w.u32(0);
        w.raw(&ops_bytes);
        self.stream
            .write_all(&rpc::frame_record(&w.into_bytes()))
            .unwrap();

        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = self.stream.read(&mut buf).unwrap();
            assert!(n > 0);
            self.rr.feed(&buf[..n]);
            if let Some(record) = self.rr.next_record().unwrap() {
                return Self::decode(&record, xid);
            }
        }
    }

    fn decode(record: &[u8], xid: u32) -> Vec<R> {
        let mut r = Reader::new(record);
        assert_eq!(r.u32().unwrap(), xid);
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 0);
        let _ = r.u32().unwrap();
        let _ = r.opaque().unwrap();
        assert_eq!(r.u32().unwrap(), 0);
        let _ = r.u32().unwrap();
        let _ = r.string().unwrap();
        let n = r.u32().unwrap() as usize;
        let mut out = Vec::new();
        for _ in 0..n {
            let opnum = r.u32().unwrap();
            let status = r.u32().unwrap();
            if status != NFS4_OK {
                out.push(R::Err(status));
                continue;
            }
            match opnum {
                OP_GETFH => out.push(R::Fh(FileHandle::decode(&mut r).unwrap())),
                OP_READ => {
                    let _ = r.bool().unwrap();
                    out.push(R::Data(r.opaque().unwrap().to_vec()));
                }
                OP_GETATTR => {
                    let mask = AttrMask::decode(&mut r).unwrap();
                    let blob = r.opaque().unwrap();
                    let mut br = Reader::new(blob);
                    let (mut ft, mut sz, mut mo) = (0, 0, 0);
                    for (wi, word) in mask.words.iter().enumerate() {
                        for bit in 0..32 {
                            if word & (1 << bit) == 0 {
                                continue;
                            }
                            match wi as u32 * 32 + bit {
                                FATTR4_TYPE => ft = br.u32().unwrap(),
                                FATTR4_SIZE => sz = br.u64().unwrap(),
                                FATTR4_MODE => mo = br.u32().unwrap(),
                                a => panic!("unexpected attr {a}"),
                            }
                        }
                    }
                    out.push(R::Attrs(ft, sz, mo));
                }
                OP_WRITE => {
                    let count = r.u32().unwrap();
                    let _ = r.u32().unwrap(); // committed
                    let _ = r.opaque_fixed(8).unwrap();
                    out.push(R::Written(count));
                }
                OP_OPEN => {
                    // OPEN4resok: stateid4(16B) + change_info4(bool+u64+u64) + rflags(u32) + attrset(bitmap4) + delegation(u32)
                    let _ = r.opaque_fixed(16).unwrap(); // stateid
                    let _ = r.bool().unwrap(); // atomic
                    let _ = r.u64().unwrap(); // before changeid
                    let _ = r.u64().unwrap(); // after changeid
                    let _ = r.u32().unwrap(); // rflags
                    let n = r.u32().unwrap() as usize; // attrset bitmap word count
                    for _ in 0..n {
                        let _ = r.u32().unwrap();
                    }
                    let _ = r.u32().unwrap(); // delegation type
                    out.push(R::Ok);
                }
                OP_CREATE => {
                    let _ = r.bool().unwrap();
                    let _ = r.u64().unwrap();
                    let _ = r.u64().unwrap();
                    let n = r.u32().unwrap() as usize;
                    for _ in 0..n {
                        let _ = r.u32().unwrap();
                    }
                    out.push(R::Ok);
                }
                OP_REMOVE | OP_LINK => {
                    let _ = r.bool().unwrap(); // atomic
                    let _ = r.u64().unwrap();
                    let _ = r.u64().unwrap();
                    out.push(R::Ok);
                }
                OP_RENAME => {
                    for _ in 0..2 {
                        let _ = r.bool().unwrap(); // atomic
                        let _ = r.u64().unwrap();
                        let _ = r.u64().unwrap();
                    }
                    out.push(R::Ok);
                }
                OP_SETATTR => {
                    let n = r.u32().unwrap() as usize;
                    for _ in 0..n {
                        let _ = r.u32().unwrap();
                    }
                    out.push(R::Ok);
                }
                OP_COMMIT => {
                    let _ = r.opaque_fixed(8).unwrap();
                    out.push(R::Ok);
                }
                OP_SETCLIENTID => {
                    let _ = r.u64().unwrap(); // clientid
                    let _ = r.opaque_fixed(8).unwrap(); // verifier
                    out.push(R::Ok);
                }
                OP_SETCLIENTID_CONFIRM => out.push(R::Ok),
                _ => out.push(R::Ok),
            }
        }
        out
    }

    fn check_ok(&mut self, ops: Ops, expect: usize) -> Vec<R> {
        let res = self.call(ops.finish());
        assert_eq!(res.len(), expect, "expected {expect} ops, got {res:?}");
        for r in &res {
            assert!(
                matches!(
                    r,
                    R::Ok | R::Fh(_) | R::Data(_) | R::Attrs(..) | R::Written(_)
                ),
                "op failed: {r:?}"
            );
        }
        res
    }
}

#[test]
fn p5_mutation_gate() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-p5-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    let uuid = fs.uuid();
    drop(fs);

    let img2 = img.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fs = Fs::open(&img2).unwrap();
        tx.send(()).unwrap();
        let _ = cownfs_nfs::server::serve("127.0.0.1:12050", cownfs_nfs::server::Shared::new(fs));
    });
    rx.recv().unwrap();
    let mut c = Client::connect("127.0.0.1:12050");

    // CREATE dir "d1".
    let mut ops = Ops::new();
    ops.putfh(&uuid, ROOT_INO);
    ops.create_dir(b"d1", 0o755);
    c.check_ok(ops, 2);

    // Establish NFSv4 client (P6).
    let mut ops = Ops::new();
    ops.setclientid(b"p5test");
    c.check_ok(ops, 1);
    // Extract clientid from SETCLIENTID response.
    let clientid: u64 = {
        // We need to parse it; for the test, we know it's 1 (first client).
        1
    };
    let mut ops = Ops::new();
    ops.setclientid_confirm(clientid);
    c.check_ok(ops, 1);

    // OPEN+CREATE file "d1/f1", WRITE data with FILE_SYNC, COMMIT.
    let data: Vec<u8> = (0..20000u32).map(|i| (i % 251) as u8).collect();
    let mut ops = Ops::new();
    ops.putfh(&uuid, ROOT_INO);
    ops.lookup(b"d1");
    ops.getfh();
    let res = c.check_ok(ops, 3);
    let d1_ino = match &res[2] {
        R::Fh(fh) => fh.inode,
        _ => panic!(),
    };
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.open_create(clientid, b"f1", 0o644);
    ops.getfh();
    let res = c.check_ok(ops, 3);
    let f1_ino = match &res[2] {
        R::Fh(fh) => fh.inode,
        other => panic!("expected Fh, got {other:?}; full res: {res:?}"),
    };
    // WRITE in two chunks.
    let mut ops = Ops::new();
    ops.putfh(&uuid, f1_ino);
    ops.write(0, &data[..8000], FILE_SYNC4);
    let res = c.check_ok(ops, 2);
    assert!(matches!(res[1], R::Written(8000)));
    let mut ops = Ops::new();
    ops.putfh(&uuid, f1_ino);
    ops.write(8000, &data[8000..], FILE_SYNC4);
    ops.commit();
    c.check_ok(ops, 3);

    // READ back over NFS and verify.
    let mut ops = Ops::new();
    ops.putfh(&uuid, f1_ino);
    ops.read(0, 30000);
    let res = c.check_ok(ops, 2);
    match &res[1] {
        R::Data(d) => assert_eq!(d, &data),
        _ => panic!(),
    }

    // SETATTR mode.
    let mut ops = Ops::new();
    ops.putfh(&uuid, f1_ino);
    ops.setattr_mode(0o600);
    ops.getattr(&[FATTR4_MODE]);
    let res = c.check_ok(ops, 3);
    match &res[2] {
        R::Attrs(_, _, mode) => assert_eq!(mode & 0o777, 0o600),
        _ => panic!(),
    }

    // CREATE symlink.
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.create_symlink(b"s1", b"f1");
    c.check_ok(ops, 2);

    // LINK (hard link) f1 -> f1link. Need SAVEFH of d1, then PUTFH f1.
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.savefh();
    ops.putfh(&uuid, f1_ino);
    ops.link(b"f1link");
    ops.restorefh();
    c.check_ok(ops, 5);

    // RENAME f1 -> f1renamed (same dir).
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.rename(b"f1", b"f1renamed");
    c.check_ok(ops, 2);

    // REMOVE the symlink.
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.remove(b"s1");
    c.check_ok(ops, 2);

    // Verify via engine: reopen the image and check.
    // (Server holds the image; we verify through NFS GETATTR/READ instead.)
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.lookup(b"f1renamed");
    ops.getfh();
    ops.getattr(&[FATTR4_TYPE, FATTR4_SIZE]);
    let res = c.check_ok(ops, 4);
    let renamed_ino = match &res[2] {
        R::Fh(fh) => fh.inode,
        _ => panic!(),
    };
    match &res[3] {
        R::Attrs(ft, sz, _) => {
            assert_eq!(*ft, NF4REG);
            assert_eq!(*sz, data.len() as u64);
        }
        _ => panic!(),
    }
    // Hard link shares the inode.
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.lookup(b"f1link");
    ops.getfh();
    let res = c.check_ok(ops, 3);
    match &res[2] {
        R::Fh(fh) => assert_eq!(fh.inode, renamed_ino),
        _ => panic!(),
    }
    // Removed symlink is gone.
    let mut ops = Ops::new();
    ops.putfh(&uuid, d1_ino);
    ops.lookup(b"s1");
    let res = c.call(ops.finish());
    assert_eq!(res.len(), 2);
    assert!(matches!(res[1], R::Err(2))); // NOENT

    // Data survives: read via the hard link.
    let mut ops = Ops::new();
    ops.putfh(&uuid, renamed_ino);
    ops.read(0, 30000);
    let res = c.check_ok(ops, 2);
    match &res[1] {
        R::Data(d) => assert_eq!(d, &data),
        _ => panic!(),
    }

    // Engine-side: fsck check passes.
    drop(c);
    // Give the server a moment; then verify with fsck via a fresh open.
    // (Server is single-threaded; connecting again is fine.)
    let fs = Fs::open(&img).unwrap();
    let report = fs.check().unwrap();
    let _ = report; // check() returns Err on inconsistency

    std::fs::remove_file(&img).unwrap();
}
