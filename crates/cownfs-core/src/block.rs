//! Raw block access: the bottom of the storage stack.
//!
//! Everything above this layer speaks in 4 KiB blocks numbered from 0.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::{Block, BLOCK_SIZE};

/// Abstraction over the raw storage holding fixed-size blocks.
pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()>;
    fn write_block(&mut self, n: u64, buf: &Block) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}

/// A [`BlockDevice`] backed by a regular file (or a block device node).
pub struct FileDevice {
    file: File,
    blocks: u64,
    /// Fault injection for B3/D3 testing. None = disabled.
    faults: Option<FaultInjector>,
}

/// Fault injection modes for testing crash consistency.
#[derive(Debug, Clone, Default)]
pub struct FaultInjector {
    /// If Some(n), writes only the first n bytes of each block (torn write).
    pub torn_write_bytes: Option<usize>,
    /// Probability (0.0-1.0) of flipping a random bit in each written block.
    pub bit_flip_prob: f64,
    /// If true, buffer writes and flush in reverse order on sync (reordering).
    pub reorder_writes: bool,
    /// Buffered writes when reorder_writes is true.
    buffered: Vec<(u64, Block)>,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_torn_writes(mut self, bytes: usize) -> Self {
        self.torn_write_bytes = Some(bytes);
        self
    }

    pub fn with_bit_flips(mut self, prob: f64) -> Self {
        self.bit_flip_prob = prob;
        self
    }

    pub fn with_reordering(mut self) -> Self {
        self.reorder_writes = true;
        self
    }
}

impl FileDevice {
    /// Set the fault injector (B3/D3 testing).
    pub fn set_faults(&mut self, faults: FaultInjector) {
        self.faults = Some(faults);
    }

    /// Clear the fault injector.
    pub fn clear_faults(&mut self) {
        self.faults = None;
    }

    /// Reads `buf.len() / BLOCK_SIZE` contiguous blocks starting at `start`
    /// in a single syscall. `buf.len()` must be a multiple of BLOCK_SIZE.
    pub fn read_blocks(&self, start: u64, buf: &mut [u8]) -> io::Result<()> {
        assert!(buf.len() % BLOCK_SIZE == 0);
        let count = buf.len() / BLOCK_SIZE;
        if start + count as u64 > self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block range out of range",
            ));
        }
        self.file.read_exact_at(buf, start * BLOCK_SIZE as u64)
    }
    /// Creates a new image file with `blocks` zeroed blocks.
    pub fn create(path: &Path, blocks: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(blocks * BLOCK_SIZE as u64)?;
        Ok(Self {
            file,
            blocks,
            faults: None,
        })
    }

    /// Opens an existing image; its size must be a multiple of the block size.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        if len % BLOCK_SIZE as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image size is not a multiple of the block size",
            ));
        }
        Ok(Self {
            blocks: len / BLOCK_SIZE as u64,
            file,
            faults: None,
        })
    }
}

impl BlockDevice for FileDevice {
    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()> {
        if n >= self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block number out of range",
            ));
        }
        self.file.read_exact_at(buf, n * BLOCK_SIZE as u64)
    }

    fn write_block(&mut self, n: u64, buf: &Block) -> io::Result<()> {
        if n >= self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block number out of range",
            ));
        }
        // B3/D3: fault injection.
        if let Some(faults) = &mut self.faults {
            // Reordering: buffer the write, flush on sync.
            if faults.reorder_writes {
                faults.buffered.push((n, *buf));
                return Ok(());
            }
            let mut data = *buf;
            // Torn write: only first N bytes.
            if let Some(torn_bytes) = faults.torn_write_bytes {
                let mut torn = [0u8; BLOCK_SIZE];
                let nb = torn_bytes.min(BLOCK_SIZE);
                torn[..nb].copy_from_slice(&data[..nb]);
                // The rest stays as it was (we don't know old content, so
                // just write the partial — the test will verify detection).
                self.file.write_all_at(&torn[..nb], n * BLOCK_SIZE as u64)?;
                return Ok(());
            }
            // Bit flip.
            if faults.bit_flip_prob > 0.0 {
                // Simple deterministic PRNG for reproducibility.
                let seed = n.wrapping_mul(0x9e3779b97f4a7c15);
                let r = ((seed >> 33) as f64) / (u64::MAX as f64);
                if r < faults.bit_flip_prob {
                    let bit = (seed % (BLOCK_SIZE as u64 * 8)) as usize;
                    data[bit / 8] ^= 1 << (bit % 8);
                }
            }
            self.file.write_all_at(&data, n * BLOCK_SIZE as u64)?;
            return Ok(());
        }
        self.file.write_all_at(buf, n * BLOCK_SIZE as u64)
    }

    fn sync(&mut self) -> io::Result<()> {
        // B3/D3: flush buffered writes in reverse order (reordering test).
        if let Some(faults) = &mut self.faults {
            if faults.reorder_writes && !faults.buffered.is_empty() {
                let buffered = std::mem::take(&mut faults.buffered);
                // Reverse order: superblock (written last) hits disk first.
                for (n, buf) in buffered.into_iter().rev() {
                    self.file.write_all_at(&buf, n * BLOCK_SIZE as u64)?;
                }
            }
        }
        self.file.sync_all()
    }
}
