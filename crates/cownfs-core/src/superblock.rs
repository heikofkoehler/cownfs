//! Ping-pong superblock: the root of all crash consistency.
//!
//! Blocks 0 and 1 each hold a self-checksummed superblock slot. Every
//! transaction commits by writing the *inactive* slot with a bumped
//! generation and syncing; a crash can only ever expose the last fully
//! written slot, so mount always sees a consistent filesystem.

use std::io;
use std::io::Read;

use crate::bitmap::{self, Bitmap};
use crate::block::BlockDevice;
use crate::checksum::checksum;
use crate::{Block, BLOCK_SIZE};

pub const MAGIC: u64 = u64::from_le_bytes(*b"cownfs01");
pub const VERSION: u32 = 1;

/// Block numbers of the two superblock slots.
pub const SLOT_BLOCKS: [u64; 2] = [0, 1];

const HDR_LEN: usize = 128;
const OFF_CHECKSUM: usize = 64;

/// In-memory superblock. The on-disk encoding is a 128-byte little-endian
/// header at the start of the slot block; the rest of the block is zeroed.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub generation: u64,
    pub block_count: u64,
    pub uuid: [u8; 16],
    pub bitmap_start: u64,
    pub bitmap_blocks: u64,
}

impl Superblock {
    fn encode(&self) -> [u8; HDR_LEN] {
        let mut hdr = [0u8; HDR_LEN];
        hdr[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[16..24].copy_from_slice(&self.generation.to_le_bytes());
        hdr[24..32].copy_from_slice(&self.block_count.to_le_bytes());
        hdr[32..48].copy_from_slice(&self.uuid);
        hdr[48..56].copy_from_slice(&self.bitmap_start.to_le_bytes());
        hdr[56..64].copy_from_slice(&self.bitmap_blocks.to_le_bytes());
        // Checksum covers the header with the checksum field zeroed.
        let sum = checksum(&hdr);
        hdr[OFF_CHECKSUM..OFF_CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        hdr
    }

    fn decode(hdr: &[u8; HDR_LEN]) -> Option<Self> {
        let magic = u64::from_le_bytes(hdr[0..8].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(hdr[8..12].try_into().ok()?);
        if version != VERSION {
            return None;
        }
        let stored = u64::from_le_bytes(hdr[OFF_CHECKSUM..OFF_CHECKSUM + 8].try_into().ok()?);
        let mut tmp = *hdr;
        tmp[OFF_CHECKSUM..OFF_CHECKSUM + 8].fill(0);
        if checksum(&tmp) != stored {
            return None;
        }
        Some(Self {
            generation: u64::from_le_bytes(hdr[16..24].try_into().ok()?),
            block_count: u64::from_le_bytes(hdr[24..32].try_into().ok()?),
            uuid: hdr[32..48].try_into().ok()?,
            bitmap_start: u64::from_le_bytes(hdr[48..56].try_into().ok()?),
            bitmap_blocks: u64::from_le_bytes(hdr[56..64].try_into().ok()?),
        })
    }
}

fn write_slot(dev: &mut impl BlockDevice, slot: usize, sb: &Superblock) -> io::Result<()> {
    let mut blk = [0u8; BLOCK_SIZE];
    blk[..HDR_LEN].copy_from_slice(&sb.encode());
    dev.write_block(SLOT_BLOCKS[slot], &blk)
}

/// Reads and validates one superblock slot; `None` means corrupt/unwritten.
pub fn read_slot(dev: &impl BlockDevice, slot: usize) -> Option<Superblock> {
    let mut blk: Block = [0; BLOCK_SIZE];
    dev.read_block(SLOT_BLOCKS[slot], &mut blk).ok()?;
    let mut hdr = [0u8; HDR_LEN];
    hdr.copy_from_slice(&blk[..HDR_LEN]);
    Superblock::decode(&hdr)
}

/// Formats a fresh filesystem: writes the bitmap and both superblock slots
/// at generation 1. Returns the superblock.
pub fn format(dev: &mut impl BlockDevice, block_count: u64) -> io::Result<Superblock> {
    let bblocks = bitmap::blocks_needed(block_count);
    let reserved = 2 + bblocks; // slots + bitmap
    if block_count < reserved + 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "image too small to format",
        ));
    }

    let mut map = Bitmap::new(block_count);
    for b in 0..reserved {
        map.set(b);
    }
    let bytes = map.to_bytes();
    let mut blk = [0u8; BLOCK_SIZE];
    for (i, chunk) in bytes.chunks(BLOCK_SIZE).enumerate() {
        blk.fill(0);
        blk[..chunk.len()].copy_from_slice(chunk);
        dev.write_block(2 + i as u64, &blk)?;
    }

    let mut uuid = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut uuid)?;

    let sb = Superblock {
        generation: 1,
        block_count,
        uuid,
        bitmap_start: 2,
        bitmap_blocks: bblocks,
    };
    write_slot(dev, 0, &sb)?;
    write_slot(dev, 1, &sb)?;
    dev.sync()?;
    Ok(sb)
}

/// Opens the filesystem: returns the valid slot with the highest generation
/// and its slot index. Fails if neither slot validates.
pub fn open(dev: &impl BlockDevice) -> io::Result<(Superblock, usize)> {
    let mut best: Option<(Superblock, usize)> = None;
    for i in 0..SLOT_BLOCKS.len() {
        if let Some(sb) = read_slot(dev, i) {
            let better = best.as_ref().map_or(true, |(b, _)| sb.generation > b.generation);
            if better {
                best = Some((sb, i));
            }
        }
    }
    best.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no valid superblock slot")
    })
}

/// P0 transaction commit primitive: advances the generation by writing the
/// inactive slot and syncing. P2 extends this into a full transaction commit
/// carrying dirty data/metadata blocks.
pub fn commit_generation(
    dev: &mut impl BlockDevice,
    sb: &mut Superblock,
    active: &mut usize,
) -> io::Result<()> {
    *active = 1 - *active;
    sb.generation += 1;
    write_slot(dev, *active, sb)?;
    dev.sync()
}
