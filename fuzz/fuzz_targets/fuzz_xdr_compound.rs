#![no_main]
use libfuzzer_sys::fuzz_target;
use cownfs_nfs::nfs4::Compound;
use cownfs_nfs::xdr::Reader;

fuzz_target!(|data: &[u8]| {
    // T3: XDR/COMPOUND decode. Must not panic on arbitrary bytes.
    let mut r = Reader::new(data);
    let _ = Compound::decode(&mut r);
});
