//! Regression: slow clients must not trigger WouldBlock errors.
//!
//! The non-blocking listener bug (2026-10-02) caused accepted sockets
//! to inherit non-blocking mode. Fast test clients never triggered it
//! (data was already available). Slow clients did (Mac mount reset).
//!
//! This test connects, waits, then sends — forcing the server to block
//! on read. If the socket is non-blocking, the server gets WouldBlock
//! and closes the connection.

#[path = "common/mod.rs"]
mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn slow_client_succeeds() {
    let server = spawn_server(256);
    let addr = server.addr;

    // Connect but don't send immediately.
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // Wait — if the server socket is non-blocking, it will try to read
    // now, get WouldBlock, and close the connection.
    std::thread::sleep(Duration::from_millis(200));

    // Now send an NFS NULL call.
    // RPC header: xid, msgtype=0 (CALL), rpcvers=2, prog=100003, vers=4, proc=0
    let mut req = Vec::new();
    req.extend_from_slice(&0x12345678u32.to_be_bytes()); // xid
    req.extend_from_slice(&0u32.to_be_bytes()); // CALL
    req.extend_from_slice(&2u32.to_be_bytes()); // rpcvers
    req.extend_from_slice(&100003u32.to_be_bytes()); // NFS
    req.extend_from_slice(&4u32.to_be_bytes()); // v4
    req.extend_from_slice(&0u32.to_be_bytes()); // NULL proc
    req.extend_from_slice(&0u32.to_be_bytes()); // auth flavor (none)
    req.extend_from_slice(&0u32.to_be_bytes()); // auth length
    req.extend_from_slice(&0u32.to_be_bytes()); // verifier flavor
    req.extend_from_slice(&0u32.to_be_bytes()); // verifier length

    // Frame as RPC-over-TCP record.
    let mut framed = Vec::new();
    framed.extend_from_slice(&((req.len() as u32) | 0x80000000).to_be_bytes());
    framed.extend_from_slice(&req);

    stream.write_all(&framed).unwrap();

    // Read reply. If the server closed the connection, this fails.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).unwrap();
    let len = u32::from_be_bytes(len_buf) & 0x7fffffff;
    assert!(len > 0 && len < 1024, "valid reply length: {len}");

    let mut reply = vec![0u8; len as usize];
    stream.read_exact(&mut reply).unwrap();

    // Verify it's a reply (msgtype=1).
    assert_eq!(&reply[4..8], &1u32.to_be_bytes());
}
