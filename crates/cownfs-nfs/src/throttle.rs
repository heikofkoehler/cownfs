//! Throttling: per-client and per-file rate limits.
//!
//! Prevents a single client or hot file from overloading the service.
//! Uses token buckets:
//!
//! - **Per-client**: max ops/sec and max bytes/sec per client IP.
//! - **Per-file**: max concurrent writers per inode, max bytes/sec per file.
//!
//! Throttled requests return NFS4ERR_DELAY (retry) or NFS4ERR_RESOURCE
//! (overload). Clients should back off and retry.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token bucket for rate limiting.
struct Bucket {
    tokens: f64,
    max_tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl Bucket {
    fn new(max_tokens: f64, refill_per_sec: f64) -> Self {
        Bucket {
            tokens: max_tokens,
            max_tokens,
            refill_per_sec,
            last: Instant::now(),
        }
    }

    /// Try to consume `n` tokens. Returns true if allowed.
    fn try_consume(&mut self, n: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.max_tokens);
        self.last = now;
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

/// Throttle configuration.
#[derive(Clone)]
pub struct ThrottleConfig {
    /// Max ops per second per client.
    pub client_ops_per_sec: f64,
    /// Max bytes per second per client (0 = unlimited).
    pub client_bytes_per_sec: f64,
    /// Max bytes per second per file (0 = unlimited).
    pub file_bytes_per_sec: f64,
    /// Max concurrent writers per file.
    pub file_max_writers: usize,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        ThrottleConfig {
            client_ops_per_sec: 1000.0,
            client_bytes_per_sec: 100.0 * 1024.0 * 1024.0, // 100 MiB/s
            file_bytes_per_sec: 50.0 * 1024.0 * 1024.0,   // 50 MiB/s
            file_max_writers: 10,
        }
    }
}

/// Throttle state.
pub struct Throttle {
    config: ThrottleConfig,
    /// Per-client op buckets.
    client_ops: Mutex<HashMap<String, Bucket>>,
    /// Per-client byte buckets.
    client_bytes: Mutex<HashMap<String, Bucket>>,
    /// Per-file byte buckets.
    file_bytes: Mutex<HashMap<u64, Bucket>>,
    /// Per-file active writer count.
    file_writers: Mutex<HashMap<u64, usize>>,
}

impl Throttle {
    pub fn new(config: ThrottleConfig) -> Self {
        Throttle {
            config,
            client_ops: Mutex::new(HashMap::new()),
            client_bytes: Mutex::new(HashMap::new()),
            file_bytes: Mutex::new(HashMap::new()),
            file_writers: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a client op is allowed. Returns false if throttled.
    pub fn check_client_op(&self, client: &str) -> bool {
        let mut buckets = self.client_ops.lock().unwrap();
        let bucket = buckets.entry(client.to_string()).or_insert_with(|| {
            Bucket::new(
                self.config.client_ops_per_sec,
                self.config.client_ops_per_sec,
            )
        });
        bucket.try_consume(1.0)
    }

    /// Check if client can send `bytes`. Returns false if throttled.
    pub fn check_client_bytes(&self, client: &str, bytes: u64) -> bool {
        if self.config.client_bytes_per_sec == 0.0 {
            return true;
        }
        let mut buckets = self.client_bytes.lock().unwrap();
        let bucket = buckets.entry(client.to_string()).or_insert_with(|| {
            Bucket::new(
                self.config.client_bytes_per_sec,
                self.config.client_bytes_per_sec,
            )
        });
        bucket.try_consume(bytes as f64)
    }

    /// Check if file can accept `bytes` write. Returns false if throttled.
    pub fn check_file_bytes(&self, ino: u64, bytes: u64) -> bool {
        if self.config.file_bytes_per_sec == 0.0 {
            return true;
        }
        let mut buckets = self.file_bytes.lock().unwrap();
        let bucket = buckets.entry(ino).or_insert_with(|| {
            Bucket::new(
                self.config.file_bytes_per_sec,
                self.config.file_bytes_per_sec,
            )
        });
        bucket.try_consume(bytes as f64)
    }

    /// Try to acquire a write slot for a file. Returns false if too many writers.
    pub fn acquire_writer(&self, ino: u64) -> bool {
        let mut writers = self.file_writers.lock().unwrap();
        let count = writers.entry(ino).or_insert(0);
        if *count >= self.config.file_max_writers {
            false
        } else {
            *count += 1;
            true
        }
    }

    /// Release a write slot.
    pub fn release_writer(&self, ino: u64) {
        let mut writers = self.file_writers.lock().unwrap();
        if let Some(count) = writers.get_mut(&ino) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                writers.remove(&ino);
            }
        }
    }

    /// Cleanup old entries (call periodically).
    pub fn cleanup(&self) {
        // Remove buckets that haven't been used in 60s.
        // For simplicity, we just clear all periodically.
        // A production system would track last-use time.
        let cutoff = Duration::from_secs(60);
        let now = Instant::now();
        // Note: Bucket doesn't track last use separately from refill,
        // so we use a simple heuristic: clear if tokens are full (unused).
        self.client_ops.lock().unwrap().retain(|_, b| {
            now.duration_since(b.last) < cutoff || b.tokens < b.max_tokens
        });
        self.client_bytes.lock().unwrap().retain(|_, b| {
            now.duration_since(b.last) < cutoff || b.tokens < b.max_tokens
        });
        self.file_bytes.lock().unwrap().retain(|_, b| {
            now.duration_since(b.last) < cutoff || b.tokens < b.max_tokens
        });
    }
}
