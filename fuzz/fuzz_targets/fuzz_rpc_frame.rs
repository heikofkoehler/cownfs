#![no_main]
use libfuzzer_sys::fuzz_target;
use cownfs_nfs::rpc::Call;

fuzz_target!(|data: &[u8]| {
    // T3: RPC record framing. Must not panic on arbitrary bytes.
    let _ = Call::decode(data);
});
