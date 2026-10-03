//! Metrics and health endpoints.

#[path = "common/mod.rs"]
mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn http_get(addr: &str, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until we have headers + body (simple: read once, server closes).
    match stream.read(&mut tmp) {
        Ok(n) => buf.extend_from_slice(&tmp[..n]),
        Err(_) => {}
    }
    let resp = String::from_utf8_lossy(&buf).to_string();
    let parts: Vec<&str> = resp.splitn(2, "\r\n\r\n").collect();
    let headers = parts.get(0).unwrap_or(&"").to_string();
    let body = parts.get(1).unwrap_or(&"").to_string();
    (headers, body)
}

#[test]
fn healthz_returns_ok() {
    let server = spawn_server(256);
    // Metrics are on port+1000.
    let metrics_addr = format!("127.0.0.1:{}", server.addr.port() + 1000);
    // Give the metrics thread time to start.
    std::thread::sleep(Duration::from_millis(100));

    let (headers, body) = http_get(&metrics_addr, "/healthz");
    assert!(headers.contains("200 OK"), "headers: {headers}");
    assert_eq!(body.trim(), "ok");
}

#[test]
fn metrics_returns_prometheus() {
    let server = spawn_server(256);
    let metrics_addr = format!("127.0.0.1:{}", server.addr.port() + 1000);
    std::thread::sleep(Duration::from_millis(100));

    let (headers, body) = http_get(&metrics_addr, "/metrics");
    assert!(headers.contains("200 OK"), "headers: {headers}");
    // Should contain Prometheus-format metrics.
    assert!(
        body.contains("cownfs_op_total") || body.contains("cownfs_uptime_seconds"),
        "body: {body}"
    );
}
