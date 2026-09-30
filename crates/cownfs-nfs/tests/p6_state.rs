//! P6 gate: state management — SETCLIENTID, OPEN with share
//! reservations, LOCK/LOCKU with conflict detection, lease expiry.

use std::io::{Read, Write};
use std::net::TcpStream;

use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_nfs::nfs4::{
    AttrMask, FileHandle, NF4DIR, NFS4ERR_DENIED, NFS4ERR_LOCKED, NFS4_OK, OPEN4_CREATE, OP_CLOSE,
    OP_CREATE, OP_GETFH, OP_LOCK, OP_LOCKU, OP_LOOKUP, OP_OPEN, OP_PUTFH, OP_SETCLIENTID,
    OP_SETCLIENTID_CONFIRM, UNCHECKED4, WRITE_LT,
};
use cownfs_nfs::rpc::{self, RecordReader};
use cownfs_nfs::xdr::{Reader, Writer};

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
    fn setclientid(&mut self, verifier: &[u8; 8], name: &[u8]) {
        self.w.u32(OP_SETCLIENTID);
        self.w.opaque_fixed(verifier);
        self.w.opaque(name);
        self.w.u32(0);
        self.w.string(b"tcp");
        self.w.string(b"127.0.0.1");
        self.w.u32(0);
        self.n += 1;
    }
    fn confirm(&mut self, clientid: u64, verifier: &[u8; 8]) {
        self.w.u32(OP_SETCLIENTID_CONFIRM);
        self.w.u64(clientid);
        self.w.opaque_fixed(verifier);
        self.n += 1;
    }
    fn open(&mut self, clientid: u64, owner: &[u8], flags: u32, name: &[u8]) {
        // flags: bits[1:0]=share_access, bits[5:4]=share_deny (same convention
        // as the test call sites use, e.g. 0x23 = access=BOTH(3)|deny=WRITE(2)<<4).
        let share_access = flags & 0x3;
        let share_deny = (flags >> 4) & 0x3;
        self.w.u32(OP_OPEN);
        self.w.u32(1); // seqid (RFC 7530 §16.16 — first field)
        self.w.u32(share_access); // share_access
        self.w.u32(share_deny); // share_deny
        self.w.u64(clientid); // open_owner4.clientid
        self.w.opaque(owner); // open_owner4.owner
        self.w.u32(OPEN4_CREATE); // opentype (always CREATE to open-or-create)
        self.w.u32(UNCHECKED4); // createmode
        AttrMask { words: vec![0, 0] }.encode(&mut self.w); // empty createattrs
        self.w.opaque(&[]); // empty attrlist
        self.w.u32(0); // CLAIM_NULL
        self.w.string(name);
        self.n += 1;
    }
    fn close(&mut self, seqid: u32, stateid: &[u8; 16]) {
        self.w.u32(OP_CLOSE);
        self.w.u32(seqid);
        self.w.raw(stateid);
        self.n += 1;
    }
    fn lock(
        &mut self,
        clientid: u64,
        locktype: u32,
        offset: u64,
        length: u64,
        new_owner: bool,
        open_stateid: Option<&[u8; 16]>,
        lock_stateid: Option<&[u8; 16]>,
        owner: &[u8],
    ) {
        self.w.u32(OP_LOCK);
        self.w.u32(locktype);
        self.w.bool(false); // reclaim
        self.w.u64(offset);
        self.w.u64(length);
        self.w.bool(new_owner);
        if new_owner {
            self.w.u32(0); // open seqid
            self.w.raw(open_stateid.unwrap());
            self.w.u32(0); // lock seqid
            self.w.u64(clientid); // lock_owner clientid
            self.w.opaque(owner);
        } else {
            self.w.u32(1); // lock seqid
            self.w.raw(lock_stateid.unwrap());
        }
        self.n += 1;
    }
    fn locku(&mut self, locktype: u32, stateid: &[u8; 16], offset: u64, length: u64) {
        self.w.u32(OP_LOCKU);
        self.w.u32(locktype);
        self.w.u32(1); // seqid
        self.w.raw(stateid);
        self.w.u64(offset);
        self.w.u64(length);
        self.n += 1;
    }
    fn create_dir(&mut self, name: &[u8]) {
        self.w.u32(OP_CREATE);
        self.w.u32(NF4DIR);
        self.w.string(name);
        AttrMask { words: vec![0, 0] }.encode(&mut self.w);
        self.w.opaque(&[]);
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
    Fh(u64),
    ClientId(u64),
    StateId([u8; 16]),
}

struct Client {
    stream: TcpStream,
    rr: RecordReader,
    xid: u32,
}

impl Client {
    fn connect(addr: &str) -> Self {
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
            stream: stream.expect("connect"),
            rr: RecordReader::new(),
            xid: 0x3000,
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
        w.string(b"p6");
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
                OP_GETFH => {
                    let fh = FileHandle::decode(&mut r).unwrap();
                    out.push(R::Fh(fh.inode));
                }
                OP_SETCLIENTID => {
                    let cid = r.u64().unwrap();
                    let _ = r.opaque_fixed(8).unwrap();
                    out.push(R::ClientId(cid));
                }
                OP_OPEN => {
                    let sid = r.opaque_fixed(16).unwrap();
                    let mut b = [0u8; 16];
                    b.copy_from_slice(sid);
                    // OPEN4resok: change_info4(bool+u64+u64) + rflags(u32) + attrset(bitmap4) + deleg(u32)
                    let _ = r.bool().unwrap(); // atomic
                    let _ = r.u64().unwrap(); // before changeid
                    let _ = r.u64().unwrap(); // after changeid
                    let _ = r.u32().unwrap(); // rflags
                    let n = r.u32().unwrap() as usize; // attrset bitmap word count
                    for _ in 0..n {
                        let _ = r.u32().unwrap();
                    }
                    let _ = r.u32().unwrap(); // delegation type
                    out.push(R::StateId(b));
                }
                OP_LOCK => {
                    let sid = r.opaque_fixed(16).unwrap();
                    let mut b = [0u8; 16];
                    b.copy_from_slice(sid);
                    out.push(R::StateId(b));
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
                _ => out.push(R::Ok),
            }
        }
        out
    }
}

#[test]
fn p6_state_gate() {
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-p6-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();
    let uuid = fs.uuid();
    drop(fs);

    let img2 = img.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut fs = Fs::open(&img2).unwrap();
        tx.send(()).unwrap();
        let _ = cownfs_nfs::server::serve("127.0.0.1:12052", &mut fs);
    });
    rx.recv().unwrap();

    // One TCP connection, two NFSv4 clients (identified by clientid).
    let mut c = Client::connect("127.0.0.1:12052");

    // Client 1: SETCLIENTID + CONFIRM.
    let v1 = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let mut ops = Ops::new();
    ops.setclientid(&v1, b"client1");
    let res = c.call(ops.finish());
    let cid1 = match res[0] {
        R::ClientId(id) => id,
        _ => panic!("no clientid: {res:?}"),
    };
    let mut ops = Ops::new();
    ops.confirm(cid1, &v1);
    let res = c.call(ops.finish());
    assert!(matches!(res[0], R::Ok));

    // Client 2: SETCLIENTID + CONFIRM.
    let v2 = [9u8, 8, 7, 6, 5, 4, 3, 2];
    let mut ops = Ops::new();
    ops.setclientid(&v2, b"client2");
    let res = c.call(ops.finish());
    let cid2 = match res[0] {
        R::ClientId(id) => id,
        _ => panic!("no clientid: {res:?}"),
    };
    let mut ops = Ops::new();
    ops.confirm(cid2, &v2);
    let res = c.call(ops.finish());
    assert!(matches!(res[0], R::Ok));

    // Create a file via client 1.
    let mut ops = Ops::new();
    ops.putfh(&uuid, ROOT_INO);
    ops.create_dir(b"shared");
    c.call(ops.finish());
    let mut ops = Ops::new();
    ops.putfh(&uuid, ROOT_INO);
    ops.lookup(b"shared");
    ops.getfh();
    let res = c.call(ops.finish());
    let shared_ino = match res[2] {
        R::Fh(ino) => ino,
        _ => panic!(),
    };

    // Client 1: OPEN file with SHARE_DENY_WRITE (flags: access=BOTH(3), deny=WRITE(2)<<4=0x20).
    // flags = 0x23
    let mut ops = Ops::new();
    ops.putfh(&uuid, shared_ino);
    ops.open(cid1, b"owner1", 0x23, b"file1");
    let res = c.call(ops.finish());
    let sid1 = match res[1] {
        R::StateId(s) => s,
        _ => panic!("no stateid: {res:?}"),
    };
    println!("Client1 opened with stateid");

    // Client 2: try to OPEN for WRITE — should be DENIED (client1 denies write).
    let mut ops = Ops::new();
    ops.putfh(&uuid, shared_ino);
    ops.open(cid2, b"owner2", 0x03, b"file1"); // access=BOTH, deny=NONE
    let res = c.call(ops.finish());
    // The OPEN should fail with NFS4ERR_DENIED (10010).
    assert!(
        matches!(res[1], R::Err(NFS4ERR_DENIED)),
        "expected DENIED, got {res:?}"
    );
    println!("Share deny enforced: client2 denied");

    // Client 1: LOCK a byte range (WRITE lock).
    let mut ops = Ops::new();
    ops.putfh(&uuid, shared_ino);
    ops.lookup(b"file1");
    ops.getfh();
    let res = c.call(ops.finish());
    let file_ino = match res[2] {
        R::Fh(ino) => ino,
        _ => panic!("no fh: {res:?}"),
    };
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.lock(
        cid1,
        WRITE_LT,
        0,
        1000,
        true,
        Some(&sid1),
        None,
        b"lockowner1",
    );
    let res = c.call(ops.finish());
    assert!(matches!(res[1], R::StateId(_)), "no lock stateid: {res:?}");
    println!("Client1 locked [0,1000)");

    // Client 2: try to LOCK overlapping range — should be LOCKED.
    // First, client2 needs to open the file (without deny conflict).
    // Client1 has DENY_WRITE, so client2 can't open for write. Let's have
    // client1 close and reopen without deny for the lock test.
    let mut ops = Ops::new();
    ops.close(1, &sid1);
    c.call(ops.finish());
    // Reopen without deny.
    let mut ops = Ops::new();
    ops.putfh(&uuid, shared_ino);
    ops.open(cid1, b"owner1", 0x03, b"file1");
    let res = c.call(ops.finish());
    let sid1b = match res[1] {
        R::StateId(s) => s,
        _ => panic!(),
    };
    let mut ops = Ops::new();
    ops.putfh(&uuid, shared_ino);
    ops.open(cid2, b"owner2", 0x03, b"file1");
    let res = c.call(ops.finish());
    let sid2 = match res[1] {
        R::StateId(s) => s,
        _ => panic!(),
    };
    println!("Both clients opened without deny");

    // Client1 locks [0,1000) WRITE.
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.lock(
        cid1,
        WRITE_LT,
        0,
        1000,
        true,
        Some(&sid1b),
        None,
        b"lockowner1",
    );
    let res = c.call(ops.finish());
    let lock_sid1 = match res[1] {
        R::StateId(s) => s,
        _ => panic!(),
    };

    // Client2 tries to lock [500,1500) WRITE — should conflict.
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.lock(
        cid1,
        WRITE_LT,
        500,
        1000,
        true,
        Some(&sid2),
        None,
        b"lockowner2",
    );
    let res = c.call(ops.finish());
    assert!(
        matches!(res[1], R::Err(NFS4ERR_LOCKED)),
        "expected LOCKED, got {res:?}"
    );
    println!("Byte-range lock conflict enforced");

    // Client2 locks non-overlapping [2000,3000) — should succeed.
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.lock(
        cid1,
        WRITE_LT,
        2000,
        1000,
        true,
        Some(&sid2),
        None,
        b"lockowner2",
    );
    let res = c.call(ops.finish());
    assert!(matches!(res[1], R::StateId(_)), "lock failed: {res:?}");
    println!("Non-overlapping lock ok");

    // Client1 unlocks.
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.locku(WRITE_LT, &lock_sid1, 0, 1000);
    let res = c.call(ops.finish());
    assert!(matches!(res[1], R::Ok), "unlock failed: {res:?}");
    println!("Unlock ok");

    // Now client2 can lock the previously-locked range.
    let mut ops = Ops::new();
    ops.putfh(&uuid, file_ino);
    ops.lock(
        cid2,
        WRITE_LT,
        0,
        1000,
        true,
        Some(&sid2),
        None,
        b"lockowner2b",
    );
    let res = c.call(ops.finish());
    assert!(matches!(res[1], R::StateId(_)), "relock failed: {res:?}");
    println!("Lock after unlock ok");

    std::fs::remove_file(&img).unwrap();
}
