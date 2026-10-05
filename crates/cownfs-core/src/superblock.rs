//! Ping-pong superblock: the root of all crash consistency.
//!
//! Blocks 0 and 1 each hold a self-checksummed superblock slot. Every
//! transaction commits by writing the *inactive* slot with a bumped
//! generation and syncing; a crash can only ever expose the last fully
//! written slot, so mount always sees a consistent filesystem.
//!
//! v2 (P2): the superblock additionally records the four B-tree roots
//! (block, generation, length) plus the inode/snapshot allocators, so a
//! freshly opened filesystem can locate all metadata.

use std::io;
use std::io::Read;

use crate::bitmap::{self, Bitmap};
use crate::block::BlockDevice;
use crate::checksum::checksum;
use crate::{Block, BLOCK_SIZE};

pub const MAGIC: u64 = u64::from_le_bytes(*b"cownfs01");
pub const VERSION: u32 = 4;

/// Magic identifying a bitmap CRC sidecar area. v3 images lack this magic
/// in the reserved sidecar blocks → CRC verification is skipped (legacy).
pub const BITMAP_CRC_MAGIC: u64 = u64::from_le_bytes(*b"cowbmcrc");

/// Block numbers of the two superblock slots.
pub const SLOT_BLOCKS: [u64; 2] = [0, 1];

/// Number of blocks needed for the CRC32C sidecar of one bitmap area.
/// Format: 8-byte magic + 4-byte CRC32C per bitmap block, rounded up to
/// whole 4KiB blocks. Returns 0 if bitmap_blocks is 0.
pub fn bitmap_crc_blocks(bitmap_blocks: u64) -> u64 {
    if bitmap_blocks == 0 {
        return 0;
    }
    let bytes = 8 + bitmap_blocks * 4;
    bytes.div_ceil(crate::BLOCK_SIZE as u64)
}

const HDR_LEN: usize = 256;
const OFF_CHECKSUM: usize = 64;

/// In-memory superblock. The on-disk encoding is a 256-byte little-endian
/// header at the start of the slot block; the rest of the block is zeroed.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub generation: u64,
    pub block_count: u64,
    pub uuid: [u8; 16],
    pub bitmap_start: u64,
    /// Blocks per bitmap area. Two areas live at
    /// `[bitmap_start, bitmap_start + 2*bitmap_blocks)`; the slot's
    /// `bitmap_area` selects the active one. The inactive area receives the
    /// next commit's bitmap, so a crash between the bitmap write and the
    /// slot write always leaves a consistent (bitmap, generation) pair.
    pub bitmap_blocks: u64,
    /// Active bitmap area (0 or 1); flipped on every commit.
    pub bitmap_area: u64,
    // Tree roots: (block, generation, length).
    pub inode_root: u64,
    pub inode_root_gen: u32,
    pub inode_len: u64,
    pub dir_root: u64,
    pub dir_root_gen: u32,
    pub dir_len: u64,
    pub extent_root: u64,
    pub extent_root_gen: u32,
    pub extent_len: u64,
    pub snap_root: u64,
    pub snap_root_gen: u32,
    pub snap_len: u64,
    // Allocators.
    pub next_inode: u64,
    pub next_snap: u64,
    /// Leader lease: node ID of current holder (UTF-8, NUL-padded).
    /// Empty (all zeros) = no lease.
    pub lease_holder: [u8; 32],
    /// Unix timestamp when the lease expires. 0 = no lease.
    pub lease_expiry: u64,
    /// Generation of the last full bitmap write. 0 = legacy mode: the
    /// `bitmap_area` selects a full bitmap (pre-delta format).
    pub bitmap_full_gen: u64,
    /// Generation that the delta area brings the bitmap up to.
    /// Equals `bitmap_full_gen` when no delta is pending.
    pub bitmap_delta_gen: u64,
    /// Which bitmap area (0 or 1) holds the full bitmap at `bitmap_full_gen`.
    /// The other area holds the delta. Swapped on checkpoint.
    pub bitmap_base_area: u64,
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
        hdr[184..192].copy_from_slice(&self.bitmap_area.to_le_bytes());
        hdr[72..80].copy_from_slice(&self.inode_root.to_le_bytes());
        hdr[80..84].copy_from_slice(&self.inode_root_gen.to_le_bytes());
        hdr[88..96].copy_from_slice(&self.inode_len.to_le_bytes());
        hdr[96..104].copy_from_slice(&self.dir_root.to_le_bytes());
        hdr[104..108].copy_from_slice(&self.dir_root_gen.to_le_bytes());
        hdr[112..120].copy_from_slice(&self.dir_len.to_le_bytes());
        hdr[120..128].copy_from_slice(&self.extent_root.to_le_bytes());
        hdr[128..132].copy_from_slice(&self.extent_root_gen.to_le_bytes());
        hdr[136..144].copy_from_slice(&self.extent_len.to_le_bytes());
        hdr[144..152].copy_from_slice(&self.snap_root.to_le_bytes());
        hdr[152..156].copy_from_slice(&self.snap_root_gen.to_le_bytes());
        hdr[160..168].copy_from_slice(&self.snap_len.to_le_bytes());
        hdr[168..176].copy_from_slice(&self.next_inode.to_le_bytes());
        hdr[176..184].copy_from_slice(&self.next_snap.to_le_bytes());
        hdr[192..224].copy_from_slice(&self.lease_holder);
        hdr[224..232].copy_from_slice(&self.lease_expiry.to_le_bytes());
        hdr[232..240].copy_from_slice(&self.bitmap_full_gen.to_le_bytes());
        hdr[240..248].copy_from_slice(&self.bitmap_delta_gen.to_le_bytes());
        hdr[248..256].copy_from_slice(&self.bitmap_base_area.to_le_bytes());
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
        let u32_at = |o: usize| -> Option<u32> {
            Some(u32::from_le_bytes(hdr.get(o..o + 4)?.try_into().ok()?))
        };
        let u64_at = |o: usize| -> Option<u64> {
            Some(u64::from_le_bytes(hdr.get(o..o + 8)?.try_into().ok()?))
        };
        Some(Self {
            generation: u64_at(16)?,
            block_count: u64_at(24)?,
            uuid: hdr[32..48].try_into().ok()?,
            bitmap_start: u64_at(48)?,
            bitmap_blocks: u64_at(56)?,
            bitmap_area: u64_at(184)?,
            inode_root: u64_at(72)?,
            inode_root_gen: u32_at(80)?,
            inode_len: u64_at(88)?,
            dir_root: u64_at(96)?,
            dir_root_gen: u32_at(104)?,
            dir_len: u64_at(112)?,
            extent_root: u64_at(120)?,
            extent_root_gen: u32_at(128)?,
            extent_len: u64_at(136)?,
            snap_root: u64_at(144)?,
            snap_root_gen: u32_at(152)?,
            snap_len: u64_at(160)?,
            next_inode: u64_at(168)?,
            next_snap: u64_at(176)?,
            lease_holder: hdr[192..224].try_into().ok()?,
            lease_expiry: u64_at(224)?,
            bitmap_full_gen: u64_at(232)?,
            bitmap_delta_gen: u64_at(240)?,
            bitmap_base_area: u64_at(248)?,
        })
    }

    /// Blank v3 superblock; the engine fills in roots before writing.
    pub fn blank(block_count: u64, bitmap_start: u64, bitmap_blocks: u64) -> Self {
        let mut uuid = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut uuid))
            .expect("/dev/urandom readable");
        Superblock {
            generation: 0,
            block_count,
            uuid,
            bitmap_start,
            bitmap_blocks,
            bitmap_area: 0,
            inode_root: 0,
            inode_root_gen: 0,
            inode_len: 0,
            dir_root: 0,
            dir_root_gen: 0,
            dir_len: 0,
            extent_root: 0,
            extent_root_gen: 0,
            extent_len: 0,
            snap_root: 0,
            snap_root_gen: 0,
            snap_len: 0,
            next_inode: 0,
            next_snap: 0,
            lease_holder: [0u8; 32],
            lease_expiry: 0,
            bitmap_full_gen: 0,
            bitmap_delta_gen: 0,
            bitmap_base_area: 0,
        }
    }
}

/// Write a single superblock slot. Public for incremental restore,
/// which advances the generation outside the normal commit path.
pub fn write_slot(dev: &mut impl BlockDevice, slot: usize, sb: &Superblock) -> io::Result<()> {
    let mut blk = [0u8; BLOCK_SIZE];
    blk[..HDR_LEN].copy_from_slice(&sb.encode());
    dev.write_block(SLOT_BLOCKS[slot], &blk)
}

/// Write both slots with `sb` as-is (no generation bump) and sync.
/// Used by mkfs; the engine's transaction path uses [`commit_generation`].
pub fn write_slots(dev: &mut impl BlockDevice, sb: &Superblock) -> io::Result<()> {
    write_slot(dev, 0, sb)?;
    write_slot(dev, 1, sb)?;
    dev.sync()
}

impl Superblock {
    /// First block of the active bitmap area.
    pub fn bitmap_area_start(&self) -> u64 {
        self.bitmap_start + self.bitmap_area * self.bitmap_blocks
    }
}

/// Reads and validates one superblock slot; `None` means corrupt/unwritten.
pub fn read_slot(dev: &impl BlockDevice, slot: usize) -> Option<Superblock> {
    let mut blk: Block = [0; BLOCK_SIZE];
    dev.read_block(SLOT_BLOCKS[slot], &mut blk).ok()?;
    let mut hdr = [0u8; HDR_LEN];
    hdr.copy_from_slice(&blk[..HDR_LEN]);
    Superblock::decode(&hdr)
}

/// Formats a fresh P0-style image: writes the bitmap and both superblock
/// slots at generation 1 (no filesystem trees; roots are zero).
/// The engine's `format_fs` builds on this for full P2 images.
pub fn format(dev: &mut impl BlockDevice, block_count: u64) -> io::Result<Superblock> {
    let bblocks = bitmap::blocks_needed(block_count);
    let reserved = 2 + 2 * bblocks; // slots + two alternating bitmap areas
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

    let mut sb = Superblock::blank(block_count, 2, bblocks);
    sb.generation = 1;
    write_slots(dev, &sb)?;
    Ok(sb)
}

/// Opens the filesystem: returns the valid slot with the highest generation
/// and its slot index. Fails if neither slot validates.
pub fn open(dev: &impl BlockDevice) -> io::Result<(Superblock, usize)> {
    let mut best: Option<(Superblock, usize)> = None;
    for i in 0..SLOT_BLOCKS.len() {
        if let Some(sb) = read_slot(dev, i) {
            let better = best
                .as_ref()
                .map_or(true, |(b, _)| sb.generation > b.generation);
            if better {
                best = Some((sb, i));
            }
        }
    }
    best.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no valid superblock slot"))
}

/// Transaction commit: advances the generation by writing the inactive slot
/// and syncing. The caller must have flushed dirty metadata and the bitmap
/// first; a crash before this point exposes the previous generation.
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
