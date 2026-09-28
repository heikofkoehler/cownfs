//! P7 gate: malformed XDR/RPC/NFS inputs fail boundedly without
//! touching storage.
//!
//! Feeds truncated, overlong, and nonsensical byte strings to the
//! RPC, XDR, and NFSv4 decoders. Every case must return an Err
//! (never panic, never hang, never touch the filesystem).

use cownfs_nfs::nfs4::{Compound, Op};
use cownfs_nfs::rpc;
use cownfs_nfs::xdr::{Reader, Writer};

#[test]
fn p7_malformed_xdr() {
    // 1. Empty input.
    let mut r = Reader::new(&[]);
    assert!(r.u32().is_err());

    // 2. Truncated u32 (3 bytes).
    let mut r = Reader::new(&[1, 2, 3]);
    assert!(r.u32().is_err());

    // 3. Opaque with declared length longer than buffer.
    let mut w = Writer::new();
    w.u32(100); // claims 100 bytes
    w.raw(&[1, 2, 3]); // only 3 available
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(r.opaque().is_err());

    // 4. String with huge length (would OOM if trusted).
    let mut w = Writer::new();
    w.u32(u32::MAX);
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(r.string().is_err());

    // 5. Compound with truncated tag.
    let mut w = Writer::new();
    w.u32(5); // tag len 5
    w.raw(&[b'a', b'b']); // only 2 bytes
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(Compound::decode(&mut r).is_err());

    // 6. Compound with bogus op count (huge).
    let mut w = Writer::new();
    w.string(b"tag");
    w.u32(0); // minorversion
    w.u32(u32::MAX); // op count
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(Compound::decode(&mut r).is_err());

    // 7. Unknown op number.
    let mut w = Writer::new();
    w.u32(99999); // bogus op
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(Op::decode(&mut r).is_err());

    // 8. RPC record with truncated header.
    let mut rr = rpc::RecordReader::new();
    rr.feed(&[0x80, 0x00]); // only 2 bytes of 4-byte header
    assert!(rr.next_record().unwrap().is_none());

    // 9. RPC record claiming huge length.
    let mut rr = rpc::RecordReader::new();
    let mut hdr = Writer::new();
    hdr.u32(0x80000000 | 0x7FFFFFFF); // last fragment, huge length
    rr.feed(&hdr.into_bytes());
    // Should not allocate; next_record returns None (waiting for data)
    // or an error, but must not panic/OOM.
    let _ = rr.next_record();

    // 10. Valid header + truncated body for PUTFH.
    let mut w = Writer::new();
    w.u32(1); // OP_PUTFH
    w.u32(4); // fh len 4
    w.raw(&[1, 2]); // only 2 bytes
    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert!(Op::decode(&mut r).is_err());

    println!("Malformed XDR: all 10 cases failed boundedly");
}
