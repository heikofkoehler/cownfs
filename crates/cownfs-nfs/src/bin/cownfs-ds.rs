//! cownfs-ds: thin data-server daemon (Phase 3 prerequisite).
//!
//! A dumb block store for the pNFS data path: `read_block`, `write_block`,
//! checksum verify. No filesystem logic — the MDS (metadata server) owns
//! allocation and the B-tree; this daemon just stores 4 KiB blocks by ID.
//!
//! Protocol (TCP, big-endian):
//!   HELLO:  magic u32 (0x4453_3031 "DS01"), version u32 (1)
//!   READ:   tag 1, block_id u64 -> status u32, data [u8;4096], checksum u64
//!   WRITE:  tag 2, block_id u64, data [u8;4096] -> status u32, checksum u64
//!   STATUS: tag 3 -> status u32, blocks_stored u64
//!
//! Checksums are CRC32C over the block data, computed on write and
//! returned on read. The MDS validates them at LAYOUTCOMMIT time.
//!
//! Usage: cownfs-ds <store-file> [listen-addr]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use cownfs_core::BLOCK_SIZE;

const MAGIC: u32 = 0x4453_3031;
const VERSION: u32 = 1;

const TAG_READ: u8 = 1;
const TAG_WRITE: u8 = 2;
const TAG_STATUS: u8 = 3;

const STATUS_OK: u32 = 0;
const STATUS_IO_ERROR: u32 = 1;

/// CRC32C (Castagnoli) — matches cownfs_core::checksum.
fn crc32c(data: &[u8]) -> u64 {
    // Software CRC32C; the MDS compares against cownfs_core::checksum.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    (!(crc)) as u64
}

/// Sparse-file block store: block_id -> file offset (id * 4096).
/// A checksum index is kept in memory and rebuilt on startup by
/// scanning (v1: rebuilt lazily on first read of each block).
struct Store {
    file: std::fs::File,
    checksums: HashMap<u64, u64>,
    blocks_stored: u64,
}

impl Store {
    fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        Ok(Self {
            file,
            checksums: HashMap::new(),
            blocks_stored: 0,
        })
    }

    fn read_block(&mut self, id: u64) -> std::io::Result<([u8; BLOCK_SIZE], u64)> {
        let mut buf = [0u8; BLOCK_SIZE];
        self.file.seek(SeekFrom::Start(id * BLOCK_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        // Lazily populate the checksum index.
        let sum = *self.checksums.entry(id).or_insert_with(|| crc32c(&buf));
        Ok((buf, sum))
    }

    fn write_block(&mut self, id: u64, data: &[u8; BLOCK_SIZE]) -> std::io::Result<u64> {
        self.file.seek(SeekFrom::Start(id * BLOCK_SIZE as u64))?;
        self.file.write_all(data)?;
        let sum = crc32c(data);
        if self.checksums.insert(id, sum).is_none() {
            self.blocks_stored += 1;
        }
        Ok(sum)
    }
}

fn read_u32(s: &mut TcpStream) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(s: &mut TcpStream) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    s.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn handle_client(mut s: TcpStream, store: Arc<Mutex<Store>>) -> std::io::Result<()> {
    let magic = read_u32(&mut s)?;
    let version = read_u32(&mut s)?;
    if magic != MAGIC || version != VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad hello",
        ));
    }
    loop {
        let mut tag = [0u8; 1];
        if s.read_exact(&mut tag).is_err() {
            return Ok(()); // Client hung up.
        }
        match tag[0] {
            TAG_READ => {
                let id = read_u64(&mut s)?;
                let mut st = store.lock().unwrap();
                match st.read_block(id) {
                    Ok((data, sum)) => {
                        s.write_all(&STATUS_OK.to_be_bytes())?;
                        s.write_all(&data)?;
                        s.write_all(&sum.to_be_bytes())?;
                    }
                    Err(_) => {
                        s.write_all(&STATUS_IO_ERROR.to_be_bytes())?;
                    }
                }
            }
            TAG_WRITE => {
                let id = read_u64(&mut s)?;
                let mut data = [0u8; BLOCK_SIZE];
                s.read_exact(&mut data)?;
                let mut st = store.lock().unwrap();
                match st.write_block(id, &data) {
                    Ok(sum) => {
                        s.write_all(&STATUS_OK.to_be_bytes())?;
                        s.write_all(&sum.to_be_bytes())?;
                    }
                    Err(_) => {
                        s.write_all(&STATUS_IO_ERROR.to_be_bytes())?;
                    }
                }
            }
            TAG_STATUS => {
                let st = store.lock().unwrap();
                s.write_all(&STATUS_OK.to_be_bytes())?;
                s.write_all(&st.blocks_stored.to_be_bytes())?;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unknown tag",
                ))
            }
        }
        s.flush()?;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cownfs-ds <store-file> [listen-addr]");
        std::process::exit(1);
    }
    let addr = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:2060".into());
    let store = Arc::new(Mutex::new(Store::open(&args[1]).expect("open store file")));
    let listener = TcpListener::bind(&addr).expect("bind");
    eprintln!("cownfs-ds listening on {addr}, store {}", args[1]);
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(s, store) {
                        eprintln!("client error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_core() {
        let data = b"hello data server";
        let mut full = [0u8; BLOCK_SIZE];
        full[..data.len()].copy_from_slice(data);
        assert_eq!(crc32c(&full), cownfs_core::checksum::checksum(&full));
    }

    #[test]
    fn store_roundtrip() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("ds-test-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut st = Store::open(p.to_str().unwrap()).unwrap();
        let mut data = [0u8; BLOCK_SIZE];
        data[..5].copy_from_slice(b"hello");
        let sum = st.write_block(42, &data).unwrap();
        let (back, sum2) = st.read_block(42).unwrap();
        assert_eq!(&back[..5], b"hello");
        assert_eq!(sum, sum2);
        assert_eq!(st.blocks_stored, 1);
        // Overwrite does not double-count.
        st.write_block(42, &data).unwrap();
        assert_eq!(st.blocks_stored, 1);
        std::fs::remove_file(&p).ok();
    }
}
