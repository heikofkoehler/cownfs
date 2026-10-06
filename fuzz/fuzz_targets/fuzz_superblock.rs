#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // T3: superblock parse. Must not panic on arbitrary headers.
    cownfs_core::superblock::fuzz::decode_header(data);
});
