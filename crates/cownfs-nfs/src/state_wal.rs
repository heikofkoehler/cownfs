//! Persistent state WAL (v40-state-partitioning §4.4, step 6).
//!
//! The in-memory state mutation log (§4.3) is also appended to a
//! write-ahead log file. On restart *without* a standby, the server
//! replays the WAL through [`StateManager::apply_record`] instead of
//! forcing clients through reclaim: the storm becomes a local disk read.
//!
//! File format (all integers big-endian):
//! - Header (24 bytes): magic `b"CWAL"` (4) + version `1` (1) +
//!   `u32 server_id` + `u32 boot_gen` + 11 reserved bytes.
//! - Then, repeatedly: `u64 seq` + `u32 len` + `len` bytes (encoded
//!   [`StateLogRecord`]).
//!
//! The WAL is fsynced after every append. State mutation rate is low
//! (opens/closes/locks, not data I/O), so the fsync cost is acceptable
//! for the durability it buys.

use crate::state::{StateLogRecord, StateManager};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const WAL_MAGIC: &[u8; 4] = b"CWAL";
const WAL_VERSION: u8 = 1;
const WAL_HEADER_LEN: usize = 24;

/// Persistent write-ahead log for state mutations.
pub struct StateWal {
    file: File,
}

impl StateWal {
    /// Open (or create) the WAL at `path`. If the file exists and has a
    /// valid header, the header is verified; the file is positioned for
    /// appending. Returns the (server_id, boot_gen) from the header, or
    /// None if the file was newly created.
    pub fn open(path: &Path) -> std::io::Result<(Self, Option<(u32, u32)>)> {
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        if existed && file.metadata()?.len() >= WAL_HEADER_LEN as u64 {
            // Existing WAL: read and verify the header.
            file.seek(SeekFrom::Start(0))?;
            let mut hdr = [0u8; WAL_HEADER_LEN];
            file.read_exact(&mut hdr)?;
            if &hdr[0..4] != WAL_MAGIC {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bad WAL magic",
                ));
            }
            if hdr[4] != WAL_VERSION {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unsupported WAL version",
                ));
            }
            let server_id = u32::from_be_bytes(hdr[5..9].try_into().unwrap());
            let boot_gen = u32::from_be_bytes(hdr[9..13].try_into().unwrap());
            // Position for appending.
            file.seek(SeekFrom::End(0))?;
            Ok((Self { file }, Some((server_id, boot_gen))))
        } else {
            // New WAL: header will be written on first append (once we know
            // the server_id/boot_gen).
            Ok((Self { file }, None))
        }
    }

    /// Write the header (for a new WAL). Must be called before any append
    /// if the WAL was newly created.
    pub fn write_header(&mut self, server_id: u32, boot_gen: u32) -> std::io::Result<()> {
        let mut hdr = [0u8; WAL_HEADER_LEN];
        hdr[0..4].copy_from_slice(WAL_MAGIC);
        hdr[4] = WAL_VERSION;
        hdr[5..9].copy_from_slice(&server_id.to_be_bytes());
        hdr[9..13].copy_from_slice(&boot_gen.to_be_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&hdr)?;
        self.file.sync_all()?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Append a record with its sequence number. Fsynced before returning.
    pub fn append(&mut self, seq: u64, record: &StateLogRecord) -> std::io::Result<()> {
        let encoded = record.encode();
        self.file.write_all(&seq.to_be_bytes())?;
        self.file.write_all(&(encoded.len() as u32).to_be_bytes())?;
        self.file.write_all(&encoded)?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Replay the WAL into `state`. Returns the number of records applied.
    /// The caller must have already set the server_id/boot_gen on `state`
    /// (from the WAL header) so applied records land under the right identity.
    pub fn replay(path: &Path, state: &StateManager) -> std::io::Result<usize> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < WAL_HEADER_LEN as u64 {
            return Ok(0);
        }
        // Skip header.
        file.seek(SeekFrom::Start(WAL_HEADER_LEN as u64))?;
        let mut count = 0;
        loop {
            let mut seq_buf = [0u8; 8];
            match file.read_exact(&mut seq_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let mut len_buf = [0u8; 4];
            file.read_exact(&mut len_buf)?;
            let rec_len = u32::from_be_bytes(len_buf) as usize;
            let mut rec_buf = vec![0u8; rec_len];
            file.read_exact(&mut rec_buf)?;
            if let Some((record, _)) = StateLogRecord::decode(&rec_buf) {
                state.apply_record(&record);
                count += 1;
            }
            // Silently skip undecodable records (torn tail write).
        }
        Ok(count)
    }

    /// Truncate the WAL (after a successful replay, or when the operator
    /// wants a clean slate). The header is preserved.
    pub fn truncate(path: &Path) -> std::io::Result<()> {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(WAL_HEADER_LEN as u64)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateManager;

    #[test]
    fn wal_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cownfs-wal-test-{}.wal", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Write.
        let (mut wal, existing) = StateWal::open(&path).unwrap();
        assert!(existing.is_none());
        wal.write_header(42, 1234).unwrap();
        let sm = StateManager::with_server_id(42);
        // (boot_gen is random; we just need the records to round-trip.)
        let rec = StateLogRecord::ClientConfirmed {
            clientid: 0x1234,
            verifier: [7u8; 8],
            name: b"test-client".to_vec(),
        };
        wal.append(1, &rec).unwrap();
        drop(wal);

        // Reopen and verify header.
        let (_wal2, existing) = StateWal::open(&path).unwrap();
        assert_eq!(existing, Some((42, 1234)));

        // Replay into a fresh StateManager.
        let sm2 = StateManager::with_server_id(42);
        let n = StateWal::replay(&path, &sm2).unwrap();
        assert_eq!(n, 1);
        assert_eq!(sm2.client_count(), 1);

        let _ = std::fs::remove_file(&path);
        let _ = sm;
    }
}
