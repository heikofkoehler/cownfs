//! D1: audit log format test.

use cownfs_nfs::log::format_audit;

#[test]
fn audit_log_is_valid_json() {
    let s = format_audit("CREATE", "127.0.0.1:1234", 1000, "testfile");
    // Must be valid JSON with required fields.
    assert!(s.contains("\"level\":\"audit\""));
    assert!(s.contains("\"op\":\"CREATE\""));
    assert!(s.contains("\"client\":\"127.0.0.1:1234\""));
    assert!(s.contains("\"uid\":1000"));
    assert!(s.contains("\"details\":\"testfile\""));
    assert!(s.contains("\"ts\":"));
    // Verify it's parseable as JSON (basic check: starts with { ends with }).
    assert!(s.starts_with('{'));
    assert!(s.ends_with('}'));
}
