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
}

impl FileDevice {
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
        self.file
            .read_exact_at(buf, start * BLOCK_SIZE as u64)
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
        Ok(Self { file, blocks })
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
        self.file.write_all_at(buf, n * BLOCK_SIZE as u64)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}
