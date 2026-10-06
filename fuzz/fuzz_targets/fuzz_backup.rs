#![no_main]
use libfuzzer_sys::fuzz_target;
use cownfs_nfs::backup_parse::parse_backup_stream;

fuzz_target!(|data: &[u8]| {
    // Must not panic; errors are fine.
    let _ = parse_backup_stream(data);
});
