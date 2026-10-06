//! State mutation log tailing (v40-state-partitioning §4.3, step 4).
//!
//! The primary appends a [`StateLogRecord`] for every state mutation.
//! A standby connects to the primary's `--state-log-addr`, sends the
//! sequence number to resume from, and receives a stream of records
//! which it applies to its own [`StateManager`] via `apply_record`.
//!
//! Wire protocol (all integers big-endian):
//! - Standby -> Primary: `u64 from_seq`
//! - Primary -> Standby: `u8 status` (0 = too far behind, 1 = ok),
//!   then `u32 server_id` + `u32 boot_gen` (the primary's identity;
//!   the standby adopts it so it owns the primary's stateids on failover)
//! - Then, repeatedly: `u64 seq` + `u32 len` + `len` bytes (encoded record).
//!   The stream stays open; the primary sends new records as they appear.

use crate::state::{StateLogRecord, StateManager};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// Run the tail server on `addr`. Each connection gets its own thread.
/// `state` is the primary's StateManager.
pub fn serve_tail(addr: &str, state: Arc<StateManager>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    if let Err(e) = handle_tail(stream, &state) {
                        eprintln!("state-log tail error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("state-log accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_tail(mut stream: TcpStream, state: &Arc<StateManager>) -> std::io::Result<()> {
    // Read from_seq.
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf)?;
    let mut from_seq = u64::from_be_bytes(buf);

    // Check if we're too far behind.
    let oldest = state.log_oldest_seq();
    if from_seq < oldest {
        stream.write_all(&[0u8])?;
        return Ok(());
    }
    if from_seq < 1 {
        from_seq = 1;
    }
    stream.write_all(&[1u8])?;
    // Send the primary's identity so the standby can adopt it.
    stream.write_all(&state.server_id().to_be_bytes())?;
    stream.write_all(&state.boot_gen().to_be_bytes())?;

    // Stream records.
    let mut next = from_seq;
    loop {
        let (records, _) = state
            .log_since(next)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "log truncated"))?;
        for (seq, rec) in records {
            let encoded = rec.encode();
            stream.write_all(&seq.to_be_bytes())?;
            stream.write_all(&(encoded.len() as u32).to_be_bytes())?;
            stream.write_all(&encoded)?;
            next = seq + 1;
        }
        // Wait for more records (or connection close).
        std::thread::sleep(Duration::from_millis(10));
        // Peek to detect close without blocking.
        let mut peek = [0u8; 1];
        match stream.peek(&mut peek) {
            Ok(0) => return Ok(()), // closed
            Ok(_) => {}             // unexpected data; ignore
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return Ok(()),
        }
    }
}

/// Tail the primary's log forever, applying records to `state`.
/// `from_seq` is the sequence to resume from (1 = from the beginning).
/// Calls `on_primary_loss` when the connection breaks and reconnect fails.
pub fn tail_forever(
    primary_addr: &str,
    state: Arc<StateManager>,
    mut from_seq: u64,
    on_primary_loss: impl Fn() + Send + 'static,
) {
    let mut consecutive_failures = 0u32;
    loop {
        match tail_once(primary_addr, &state, &mut from_seq) {
            Ok(()) => {
                // Clean EOF (primary closed). Reconnect after a beat.
                consecutive_failures = 0;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                consecutive_failures += 1;
                eprintln!("state-log tail: {e} (failure {consecutive_failures})");
                if consecutive_failures >= 5 {
                    eprintln!("state-log tail: primary appears lost, promoting");
                    on_primary_loss();
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// One tail session: connect, stream until EOF or error.
/// Updates `from_seq` as records are applied.
fn tail_once(
    primary_addr: &str,
    state: &Arc<StateManager>,
    from_seq: &mut u64,
) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(primary_addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    // Make peek non-blocking for close detection.
    stream.set_nonblocking(false)?;
    stream.write_all(&from_seq.to_be_bytes())?;
    let mut status = [0u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "standby too far behind; resync required",
        ));
    }
    // Adopt the primary's identity so we own its stateids on failover.
    let mut idbuf = [0u8; 8];
    stream.read_exact(&mut idbuf)?;
    let server_id = u32::from_be_bytes(idbuf[0..4].try_into().unwrap());
    let boot_gen = u32::from_be_bytes(idbuf[4..8].try_into().unwrap());
    state.set_server_id(server_id);
    state.set_boot_gen(boot_gen);
    loop {
        let mut hdr = [0u8; 12];
        match stream.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let seq = u64::from_be_bytes(hdr[0..8].try_into().unwrap());
        let len = u32::from_be_bytes(hdr[8..12].try_into().unwrap()) as usize;
        if len > 10 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "record too large",
            ));
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)?;
        let (rec, _) = StateLogRecord::decode(&buf)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad record"))?;
        state.apply_record(&rec);
        *from_seq = seq + 1;
    }
}
