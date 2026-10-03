//! Throttling: per-client and per-file rate limits.

use cownfs_nfs::throttle::{Throttle, ThrottleConfig};

#[test]
fn client_op_rate_limit() {
    let config = ThrottleConfig {
        client_ops_per_sec: 10.0,
        ..Default::default()
    };
    let t = Throttle::new(config);

    // Burst of 10 should succeed (bucket starts full).
    for _ in 0..10 {
        assert!(t.check_client_op("client1"));
    }
    // 11th should fail (no tokens left).
    assert!(!t.check_client_op("client1"));

    // Different client has its own bucket.
    assert!(t.check_client_op("client2"));
}

#[test]
fn client_byte_rate_limit() {
    let config = ThrottleConfig {
        client_bytes_per_sec: 1000.0,
        ..Default::default()
    };
    let t = Throttle::new(config);

    // 1000 bytes should succeed.
    assert!(t.check_client_bytes("c1", 1000));
    // Another 1000 should fail (bucket empty).
    assert!(!t.check_client_bytes("c1", 1000));
}

#[test]
fn file_writer_limit() {
    let config = ThrottleConfig {
        file_max_writers: 2,
        ..Default::default()
    };
    let t = Throttle::new(config);

    // Two writers OK.
    assert!(t.acquire_writer(42));
    assert!(t.acquire_writer(42));
    // Third fails.
    assert!(!t.acquire_writer(42));

    // Release one, now OK.
    t.release_writer(42);
    assert!(t.acquire_writer(42));

    // Different file has its own limit.
    assert!(t.acquire_writer(43));
}

#[test]
fn file_byte_rate_limit() {
    let config = ThrottleConfig {
        file_bytes_per_sec: 500.0,
        ..Default::default()
    };
    let t = Throttle::new(config);

    assert!(t.check_file_bytes(1, 500));
    assert!(!t.check_file_bytes(1, 500));
    // Different file OK.
    assert!(t.check_file_bytes(2, 500));
}
