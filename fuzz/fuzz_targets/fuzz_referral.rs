#![no_main]
use libfuzzer_sys::fuzz_target;
use cownfs_nfs::referrals::ReferralTable;

fuzz_target!(|data: &[u8]| {
    // Referral config is UTF-8 text; skip non-UTF8 inputs.
    if let Ok(text) = std::str::from_utf8(data) {
        // Must not panic; errors are fine.
        let _ = ReferralTable::parse(text);
    }
});
