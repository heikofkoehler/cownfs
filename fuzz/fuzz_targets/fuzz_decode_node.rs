#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // T3: decode_node. Must not panic on arbitrary 4KiB blocks.
    cownfs_core::store::fuzz::decode_node_bytes(data);
});
