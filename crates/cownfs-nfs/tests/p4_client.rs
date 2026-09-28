//! P4 gate: userspace NFSv4 client. Serves an image, walks it over NFS,
//! and compares `ls -R` + file contents against an engine-side manifest.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use cownfs_core::engine::{Fs, FTYPE_DIR, ROOT_INO};
use cownfs_nfs::nfs4::{
    AttrMask, FileHandle, FATTR4_MODE, FATTR4_SIZE, FATTR4_TYPE, NF4DIR, NFS4_OK, OP_GETATTR,
    OP_GETFH, OP_LOOKUP, OP_PUTFH, OP_READ, OP_READDIR,
};
use cownfs_nfs::nfs4::{NF4LNK, NF4REG};
use cownfs_nfs::rpc::{self, RecordReader};
use cownfs_nfs::xdr::{Reader, Writer};

// -- request builders ------------------------------------------------------

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
    fn readdir(&mut self, attrs: &[u32]) {
        self.w.u32(OP_READDIR);
        self.w.u64(0); // cookie
        self.w.raw(&[0u8; 8]); // cookieverf
        self.w.u32(0); // dircount (0 = no limit hint)
        self.w.u32(65536); // maxcount
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
        self.w.u32(0); // stateid seqid
        self.w.raw(&[0u8; 12]); // stateid other
        self.w.u64(offset);
        self.w.u32(count);
        self.n += 1;
    }
    fn finish(mut self) -> Vec<u8> {
        let mut out = Writer::new();
        out.u32(self.n);
        out.raw(&self.w.into_bytes());
        out.into_bytes()
    }
}

// -- reply parsing ---------------------------------------------------------

#[derive(Debug)]
struct Entry {
    name: Vec<u8>,
    ftype: u32,
    size: u64,
    mode: u32,
}

fn parse_attrs(r: &mut Reader) -> (u32, u64, u32) {
    // Returns (ftype, size, mode).
    let mask = AttrMask::decode(r).unwrap();
    let blob = r.opaque().unwrap();
    let mut br = Reader::new(blob);
    let mut ftype = 0;
    let mut size = 0;
    let mut mode = 0;
    for (wi, word) in mask.words.iter().enumerate() {
        for bit in 0..32 {
            if word & (1 << bit) == 0 {
                continue;
            }
            let attr = wi as u32 * 32 + bit;
            match attr {
                FATTR4_TYPE => ftype = br.u32().unwrap(),
                FATTR4_SIZE => size = br.u64().unwrap(),
                FATTR4_MODE => mode = br.u32().unwrap(),
                _ => panic!("unexpected attr {attr} in test"),
            }
        }
    }
    (ftype, size, mode)
}

/// Minimal blocking NFSv4 client.
struct Client {
    stream: TcpStream,
    rr: RecordReader,
    xid: u32,
    uuid: [u8; 16],
}

impl Client {
    fn connect(addr: &str, uuid: [u8; 16]) -> Self {
        Client {
            stream: TcpStream::connect(addr).unwrap(),
            rr: RecordReader::new(),
            xid: 0x1000,
            uuid,
        }
    }

    fn call(&mut self, ops_bytes: Vec<u8>) -> Vec<OpReply> {
        self.xid += 1;
        let xid = self.xid;
        let mut w = Writer::new();
        w.u32(xid);
        w.u32(0); // CALL
        w.u32(2);
        w.u32(rpc::NFS_PROGRAM);
        w.u32(rpc::NFS_VERSION);
        w.u32(rpc::PROC_COMPOUND);
        w.u32(1); // AUTH_SYS
        let mut cred = Writer::new();
        cred.u32(0);
        cred.string(b"test");
        cred.u32(0);
        cred.u32(0);
        cred.u32(0);
        w.opaque(&cred.into_bytes());
        w.u32(0); // verf flavor
        w.u32(0); // verf len
        w.string(b"p4");
        w.u32(0); // minorversion
        w.raw(&ops_bytes);
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

    fn decode(record: &[u8], xid: u32) -> Vec<OpReply> {
        let mut r = Reader::new(record);
        assert_eq!(r.u32().unwrap(), xid);
        assert_eq!(r.u32().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 0);
        let _ = r.u32().unwrap();
        let _ = r.opaque().unwrap();
        assert_eq!(r.u32().unwrap(), 0);
        let _overall = r.u32().unwrap();
        let _ = r.string().unwrap();
        let n = r.u32().unwrap() as usize;
        let mut out = Vec::new();
        for _ in 0..n {
            let opnum = r.u32().unwrap();
            let status = r.u32().unwrap();
            let reply = if status == NFS4_OK {
                match opnum {
                    OP_GETFH => OpReply::Fh(FileHandle::decode(&mut r).unwrap()),
                    OP_GETATTR => {
                        let (ftype, size, mode) = parse_attrs(&mut r);
                        OpReply::Attrs { ftype, size, mode }
                    }
                    OP_READDIR => {
                        let _ = r.u64().unwrap(); // cookieverf
                        let mut entries = Vec::new();
                        while r.bool().unwrap() {
                            let _cookie = r.u64().unwrap();
                            let name = r.opaque().unwrap().to_vec();
                            let (ftype, size, mode) = parse_attrs(&mut r);
                            entries.push(Entry {
                                name,
                                ftype,
                                size,
                                mode,
                            });
                        }
                        let _eof = r.bool().unwrap();
                        OpReply::Dir(entries)
                    }
                    OP_READ => {
                        let _eof = r.bool().unwrap();
                        let data = r.opaque().unwrap().to_vec();
                        OpReply::Data(data)
                    }
                    _ => OpReply::Ok,
                }
            } else {
                OpReply::Err(status)
            };
            out.push(reply);
        }
        out
    }

    /// PUTFH(ino) + LOOKUP(name) + GETFH, returning the child's fh.
    fn lookup_fh(&mut self, dir_ino: u64, name: &[u8]) -> (u32, Option<FileHandle>) {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir_ino);
        ops.lookup(name);
        ops.getfh();
        let res = self.call(ops.finish());
        // A failed LOOKUP stops the compound: 2 ops (PUTFH ok, LOOKUP err).
        match res.as_slice() {
            [OpReply::Ok, OpReply::Ok, OpReply::Fh(fh)] => (NFS4_OK, Some(fh.clone())),
            [OpReply::Ok, OpReply::Err(st)] => (*st, None),
            [OpReply::Err(st), ..] => (*st, None),
            _ => panic!(
                "unexpected lookup reply for {:?}: {res:?}",
                String::from_utf8_lossy(name)
            ),
        }
    }

    fn readdir(&mut self, dir_ino: u64) -> Vec<Entry> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir_ino);
        ops.readdir(&[FATTR4_TYPE, FATTR4_SIZE, FATTR4_MODE]);
        let res = self.call(ops.finish());
        assert_eq!(res.len(), 2);
        match (&res[0], &res[1]) {
            (OpReply::Ok, OpReply::Dir(e)) => e
                .iter()
                .filter(|e| e.name != b"." && e.name != b"..")
                .cloned()
                .collect(),
            _ => panic!("unexpected readdir reply {res:?}"),
        }
    }

    fn read(&mut self, ino: u64, size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0u64;
        while off < size {
            let mut ops = Ops::new();
            ops.putfh(&self.uuid, ino);
            ops.read(off, 65536);
            let res = self.call(ops.finish());
            match (&res[0], &res[1]) {
                (OpReply::Ok, OpReply::Data(d)) => {
                    if d.is_empty() {
                        break;
                    }
                    off += d.len() as u64;
                    out.extend_from_slice(d);
                }
                _ => panic!("unexpected read reply {res:?}"),
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
enum OpReply {
    Ok,
    Err(u32),
    Fh(FileHandle),
    Attrs { ftype: u32, size: u64, mode: u32 },
    Dir(Vec<Entry>),
    Data(Vec<u8>),
}

impl Clone for Entry {
    // manual Clone (Debug derived above)
    fn clone(&self) -> Self {
        Entry {
            name: self.name.clone(),
            ftype: self.ftype,
            size: self.size,
            mode: self.mode,
        }
    }
}

// -- the P4 gate -----------------------------------------------------------

/// Recursively list over NFS and compare with the engine manifest.
fn ls_r(
    client: &mut Client,
    dir_ino: u64,
    prefix: &str,
    manifest: &BTreeMap<String, (u32, u64, Vec<u8>)>,
) {
    for e in client.readdir(dir_ino) {
        let name = String::from_utf8(e.name.clone()).unwrap();
        let path = format!("{prefix}/{name}");
        let (ftype, size, mode) = (e.ftype, e.size, e.mode);
        let key = path.clone();
        let (m_ftype, m_size, m_data) = manifest
            .get(&key)
            .unwrap_or_else(|| panic!("{key} not in manifest"));
        let expect_ftype = if *m_ftype as u8 == FTYPE_DIR {
            NF4DIR
        } else if *m_ftype as u8 == cownfs_core::engine::FTYPE_SYMLINK {
            cownfs_nfs::nfs4::NF4LNK
        } else {
            NF4REG
        };
        assert_eq!(ftype, expect_ftype, "ftype mismatch for {key}");
        assert_eq!(size, *m_size, "size mismatch for {key}");
        let expect_mode = if ftype == NF4DIR {
            0o755_u32
        } else {
            0o644_u32
        };
        if ftype != NF4LNK {
            assert_eq!(mode & 0o7777, expect_mode, "mode mismatch for {key}");
        }
        if ftype == NF4DIR {
            let (_, child_fh) = client.lookup_fh(dir_ino, e.name.as_slice());
            let child_ino = child_fh.unwrap().inode;
            ls_r(client, child_ino, &path, manifest);
        } else {
            let (_, child_fh) = client.lookup_fh(dir_ino, e.name.as_slice());
            let data = client.read(child_fh.unwrap().inode, size);
            assert_eq!(&data, m_data, "data mismatch for {key}");
        }
        let _ = mode;
    }
}

#[test]
fn p4_readonly_gate() {
    // Build a populated image.
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-p4-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).unwrap();
    let mut manifest: BTreeMap<String, (u32, u64, Vec<u8>)> = BTreeMap::new();

    let sub = fs.mkdir(ROOT_INO, b"sub", 0o755, 1000, 1000).unwrap();
    manifest.insert("/sub".into(), (FTYPE_DIR as u32, 0, Vec::new()));
    let deep = fs.mkdir(sub, b"deep", 0o755, 1000, 1000).unwrap();
    manifest.insert("/sub/deep".into(), (FTYPE_DIR as u32, 0, Vec::new()));

    for (name, parent, size, seed) in [
        ("a.txt", ROOT_INO, 0usize, 1u8),
        ("b.bin", ROOT_INO, 5000usize, 2u8),
        ("c.txt", sub, 9000usize, 3u8),
        ("d.bin", deep, 100usize, 4u8),
    ] {
        let ino = fs
            .create(parent, name.as_bytes(), 0o644, 1000, 1000)
            .unwrap();
        let data: Vec<u8> = (0..size).map(|i| seed.wrapping_add(i as u8)).collect();
        if !data.is_empty() {
            fs.write(ino, 0, &data).unwrap();
        }
        let pname = if parent == ROOT_INO {
            format!("/{name}")
        } else if parent == sub {
            format!("/sub/{name}")
        } else {
            format!("/sub/deep/{name}")
        };
        manifest.insert(
            pname,
            (cownfs_core::engine::FTYPE_FILE as u32, size as u64, data),
        );
    }
    // Empty file and a symlink.
    let e = fs.create(ROOT_INO, b"empty", 0o644, 1000, 1000).unwrap();
    manifest.insert(
        "/empty".into(),
        (cownfs_core::engine::FTYPE_FILE as u32, 0, Vec::new()),
    );
    let _ = e;
    fs.symlink(ROOT_INO, b"link", b"/sub/c.txt", 1000, 1000)
        .unwrap();
    manifest.insert(
        "/link".into(),
        (
            cownfs_core::engine::FTYPE_SYMLINK as u32,
            b"/sub/c.txt".len() as u64,
            b"/sub/c.txt".to_vec(),
        ),
    );
    fs.commit().unwrap();
    let uuid = fs.uuid();
    drop(fs);

    // Serve it in a background thread.
    let img2 = img.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut fs = Fs::open(&img2).unwrap();
        tx.send(()).unwrap();
        let _ = cownfs_nfs::server::serve("127.0.0.1:12049", &mut fs);
    });
    rx.recv().unwrap();

    // Walk over NFS and compare.
    let mut client = Client::connect("127.0.0.1:12049", uuid);
    // Root fh sanity: PUTFH(root) + GETATTR(type).
    let mut ops = Ops::new();
    ops.putfh(&uuid, ROOT_INO);
    ops.getattr(&[FATTR4_TYPE]);
    let res = client.call(ops.finish());
    assert!(matches!(res[0], OpReply::Ok));
    assert!(matches!(res[1], OpReply::Attrs { ftype: NF4DIR, .. }));

    ls_r(&mut client, ROOT_INO, "", &manifest);

    // READ on a symlink returns its target (v4.0 behavior).
    let (_, fh) = client.lookup_fh(ROOT_INO, b"link");
    let target = client.read(fh.unwrap().inode, 64);
    assert_eq!(target, b"/sub/c.txt");

    // LOOKUP of a missing name -> NOENT.
    let (st, _) = client.lookup_fh(ROOT_INO, b"no-such-file");
    assert_eq!(st, cownfs_nfs::nfs4::NFS4ERR_NOENT);

    std::fs::remove_file(&img).unwrap();
}
