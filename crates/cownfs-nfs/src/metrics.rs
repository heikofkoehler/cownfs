//! Metrics: operation counters and latency histograms.
//!
//! Exposed via a Prometheus-compatible text endpoint.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Per-operation metrics.
#[derive(Default)]
struct OpMetrics {
    count: u64,
    errors: u64,
    total_micros: u64,
}

/// Global metrics registry.
#[derive(Default)]
pub struct Metrics {
    ops: Mutex<HashMap<String, OpMetrics>>,
    start: Option<Instant>,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ops: Mutex::new(HashMap::new()),
            start: Some(Instant::now()),
        })
    }

    /// Record an operation completion.
    pub fn record(&self, op: &str, duration: std::time::Duration, is_error: bool) {
        let mut ops = self.ops.lock().unwrap();
        let m = ops.entry(op.to_string()).or_default();
        m.count += 1;
        if is_error {
            m.errors += 1;
        }
        m.total_micros += duration.as_micros() as u64;
    }

    /// Render as Prometheus text format.
    pub fn render(&self) -> String {
        let ops = self.ops.lock().unwrap();
        let mut out = String::new();
        out.push_str("# HELP cownfs_op_total Total NFS operations\n");
        out.push_str("# TYPE cownfs_op_total counter\n");
        out.push_str("# HELP cownfs_op_errors Total failed operations\n");
        out.push_str("# TYPE cownfs_op_errors counter\n");
        out.push_str("# HELP cownfs_op_latency_us Total latency microseconds\n");
        out.push_str("# TYPE cownfs_op_latency_us counter\n");
        for (op, m) in ops.iter() {
            out.push_str(&format!("cownfs_op_total{{op=\"{op}\"}} {}\n", m.count));
            out.push_str(&format!("cownfs_op_errors{{op=\"{op}\"}} {}\n", m.errors));
            out.push_str(&format!(
                "cownfs_op_latency_us{{op=\"{op}\"}} {}\n",
                m.total_micros
            ));
        }
        if let Some(start) = self.start {
            out.push_str("# HELP cownfs_uptime_seconds Server uptime\n");
            out.push_str("# TYPE cownfs_uptime_seconds gauge\n");
            out.push_str(&format!(
                "cownfs_uptime_seconds {}\n",
                start.elapsed().as_secs()
            ));
        }
        out
    }
}
