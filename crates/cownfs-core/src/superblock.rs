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

const HDR_LEN: usize = 320;
/// R7: legacy header lengths. Decode accepts 320 (current), 312 (S3), and
/// 256 (pre-R7); the 280-byte R7 intermediate was never deployed.
const HDR_LEN_LEGACY: usize = 256;
/// S3 intermediate (live counts but no free_blocks).
const HDR_LEN_S3: usize = 312;
const OFF_CHECKSUM: usize = 64;

/// R7: feature flags understood by this build.
/// - `compat`: safe to ignore unknown bits.
/// - `ro_compat`: unknown bits → open read-only.
/// - `incompat`: unknown bits → refuse to open.
///
/// N9: S3/S4 format features are backward-compatible (old code falls back
/// to walks/recomputes), so they are COMPAT bits.
pub const COMPAT_PERSISTED_COUNTS: u64 = 1 << 0; // S3: per-arena live counts
pub const COMPAT_QUOTA_TABLE: u64 = 1 << 1; // S3: persisted quota usage table
pub const COMPAT_FREE_BLOCKS: u64 = 1 << 2; // S4: persisted free_blocks counter
pub const COMPAT_PAGED_BITMAP: u64 = 1 << 3; // S4: per-slot bitmap areas
pub const KNOWN_COMPAT: u64 =
    COMPAT_PERSISTED_COUNTS | COMPAT_QUOTA_TABLE | COMPAT_FREE_BLOCKS | COMPAT_PAGED_BITMAP;
pub const KNOWN_RO_COMPAT: u64 = 0;
pub const KNOWN_INCOMPAT: u64 = 0;

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
    /// R7: feature flags (ZFS/ext4 style).
    /// Unknown `compat` bits are safe to ignore.
    pub compat: u64,
    /// Unknown `ro_compat` bits force a read-only open.
    pub ro_compat: u64,
    /// Unknown `incompat` bits refuse the open entirely.
    pub incompat: u64,
    /// S3: per-arena live node counts, persisted on every commit so open()
    /// doesn't need the O(tree) `reachable_multi` seeding walk.
    pub live_inodes: u64,
    pub live_dirs: u64,
    pub live_extents: u64,
    pub live_snaps: u64,
    /// S4: P3 free-block counter, persisted on every commit.
    pub free_blocks: u64,
    /// Not serialized: true if the on-disk header predates S3 live counts
    /// (legacy 256-byte header). Open falls back to the reachability walk.
    #[allow(dead_code)]
    pub has_live_counts: bool,
    /// Not serialized: true if the on-disk header has the free_blocks
    /// field (320-byte header). Older headers (256/312 bytes) decode with
    /// free_blocks=0; open recomputes via popcount (N8).
    pub has_free_blocks: bool,
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
        // R7: feature flags.
        hdr[256..264].copy_from_slice(&self.compat.to_le_bytes());
        hdr[264..272].copy_from_slice(&self.ro_compat.to_le_bytes());
        hdr[272..280].copy_from_slice(&self.incompat.to_le_bytes());
        // S3: per-arena live counts.
        hdr[280..288].copy_from_slice(&self.live_inodes.to_le_bytes());
        hdr[288..296].copy_from_slice(&self.live_dirs.to_le_bytes());
        hdr[296..304].copy_from_slice(&self.live_extents.to_le_bytes());
        hdr[304..312].copy_from_slice(&self.live_snaps.to_le_bytes());
        // S4: P3 free-block counter.
        hdr[312..320].copy_from_slice(&self.free_blocks.to_le_bytes());
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
        // R7: accept both the current 280-byte header and the legacy
        // 256-byte header (pre-feature-flags images have zeros past 256,
        // so their 256-byte checksum won't verify as a 280-byte one).
        let mut tmp = *hdr;
        tmp[OFF_CHECKSUM..OFF_CHECKSUM + 8].fill(0);
        // Accept 320 (current), 312 (S3, no free_blocks), and 256 (pre-R7).
        // Older images have zeros past their length, so their shorter
        // checksum won't verify as a longer one.
        let hdr_len = if checksum(&tmp) == stored {
            HDR_LEN
        } else {
            let mut tmp312 = tmp;
            tmp312[HDR_LEN_S3..].fill(0);
            if checksum(&tmp312[..HDR_LEN_S3]) == stored {
                HDR_LEN_S3
            } else {
                let mut tmp256 = tmp;
                tmp256[HDR_LEN_LEGACY..].fill(0);
                if checksum(&tmp256[..HDR_LEN_LEGACY]) == stored {
                    HDR_LEN_LEGACY
                } else {
                    return None;
                }
            }
        };
        let legacy = hdr_len == HDR_LEN_LEGACY;
        let has_free_blocks = hdr_len == HDR_LEN;
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
            // R7: legacy images have no flag area → all zeros.
            compat: if legacy { 0 } else { u64_at(256)? },
            ro_compat: if legacy { 0 } else { u64_at(264)? },
            incompat: if legacy { 0 } else { u64_at(272)? },
            // S3: legacy images have no live counts → 0 (caller falls back
            // to the reachability walk when all four are zero... see open).
            // Actually: 0 is a valid count (empty FS), so we seed from the
            // walk only when the image predates S3 (see `has_live_counts`).
            live_inodes: if legacy { 0 } else { u64_at(280)? },
            live_dirs: if legacy { 0 } else { u64_at(288)? },
            live_extents: if legacy { 0 } else { u64_at(296)? },
            live_snaps: if legacy { 0 } else { u64_at(304)? },
            // S4: S3-era images (312 bytes) have no free_blocks → 0.
            free_blocks: if has_free_blocks { u64_at(312)? } else { 0 },
            has_live_counts: !legacy,
            has_free_blocks,
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
            compat: KNOWN_COMPAT,
            ro_compat: 0,
            incompat: 0,
            live_inodes: 0,
            live_dirs: 0,
            live_extents: 0,
            live_snaps: 0,
            free_blocks: 0,
            has_live_counts: true,
            has_free_blocks: true,
        }
    }
}

/// Write a single superblock slot. Public for incremental restore,
/// which advances the generation outside the normal commit path.
pub fn write_slot(dev: &impl BlockDevice, slot: usize, sb: &Superblock) -> io::Result<()> {
    let mut blk = [0u8; BLOCK_SIZE];
    blk[..HDR_LEN].copy_from_slice(&sb.encode());
    dev.write_block(SLOT_BLOCKS[slot], &blk)
}

/// Write both slots with `sb` as-is (no generation bump) and sync.
/// Used by mkfs; the engine's transaction path uses [`commit_generation`].
pub fn write_slots(dev: &impl BlockDevice, sb: &Superblock) -> io::Result<()> {
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

/// Check if a slot contains a v3 superblock (for R1 migration error).
/// Returns true if the magic matches but the version is 3.
pub fn is_v3_slot(dev: &impl BlockDevice, slot: usize) -> bool {
    let mut blk: Block = [0; BLOCK_SIZE];
    if dev.read_block(SLOT_BLOCKS[slot], &mut blk).is_err() {
        return false;
    }
    let magic = u64::from_le_bytes(blk[0..8].try_into().unwrap_or([0u8; 8]));
    if magic != MAGIC {
        return false;
    }
    let version = u32::from_le_bytes(blk[8..12].try_into().unwrap_or([0u8; 4]));
    version == 3
}

/// Formats a fresh P0-style image: writes the bitmap and both superblock
/// slots at generation 1 (no filesystem trees; roots are zero).
/// The engine's `format_fs` builds on this for full P2 images.
pub fn format(dev: &impl BlockDevice, block_count: u64) -> io::Result<Superblock> {
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
    dev: &impl BlockDevice,
    sb: &mut Superblock,
    active: &mut usize,
) -> io::Result<()> {
    *active = 1 - *active;
    sb.generation += 1;
    write_slot(dev, *active, sb)?;
    dev.sync()
}

// ---- S3: quota usage table in the superblock block padding ----
//
// The 4 KiB superblock block holds the 312-byte header; the quota table
// lives at offset 1024. Format:
//   [1024..1032]: magic b"QUOTA\0\0\0"
//   [1032..1040]: u64 entry count, or u64::MAX = "too many UIDs, rebuild"
//   [1040..]: entries, 12 bytes each: u32 LE uid, u64 LE blocks
//   after entries: u32 LE CRC32C of the table bytes [1024..end]
const QUOTA_OFF: usize = 1024;
const QUOTA_MAGIC: [u8; 8] = *b"QUOTA\0\0\0";
const QUOTA_ENTRY: usize = 12;
const QUOTA_MAX: usize = 200; // 200*12 = 2400 bytes, fits in the block

/// Sentinel: quota table overflowed; the opener must rebuild from the trees.
const QUOTA_REBUILD: u64 = u64::MAX;

/// Write the quota usage table into the superblock block's padding.
/// Called on every commit (alongside the slot write).
pub fn write_quota_table(
    dev: &impl BlockDevice,
    slot: usize,
    usage: &std::collections::HashMap<u32, u64>,
) -> io::Result<()> {
    let mut blk = [0u8; BLOCK_SIZE];
    dev.read_block(SLOT_BLOCKS[slot], &mut blk)?;
    // Preserve the header (written by write_slot); fill the quota area.
    blk[QUOTA_OFF..QUOTA_OFF + 8].copy_from_slice(&QUOTA_MAGIC);
    if usage.len() > QUOTA_MAX {
        blk[QUOTA_OFF + 8..QUOTA_OFF + 16].copy_from_slice(&QUOTA_REBUILD.to_le_bytes());
    } else {
        let mut entries: Vec<(&u32, &u64)> = usage.iter().collect();
        entries.sort_by_key(|(uid, _)| *uid);
        blk[QUOTA_OFF + 8..QUOTA_OFF + 16].copy_from_slice(&(entries.len() as u64).to_le_bytes());
        let mut off = QUOTA_OFF + 16;
        for (uid, blocks) in entries {
            blk[off..off + 4].copy_from_slice(&uid.to_le_bytes());
            blk[off + 4..off + 12].copy_from_slice(&blocks.to_le_bytes());
            off += QUOTA_ENTRY;
        }
        let crc = crate::checksum::checksum32(&blk[QUOTA_OFF..off]);
        blk[off..off + 4].copy_from_slice(&crc.to_le_bytes());
    }
    dev.write_block(SLOT_BLOCKS[slot], &blk)?;
    Ok(())
}

/// Read the quota usage table. Returns `None` if absent/corrupt (caller
/// falls back to rebuilding from the inode tree), or `Some(Err(()))` if
/// the sentinel requests a rebuild.
pub fn read_quota_table(
    dev: &impl BlockDevice,
    slot: usize,
) -> Option<Result<std::collections::HashMap<u32, u64>, ()>> {
    let mut blk = [0u8; BLOCK_SIZE];
    dev.read_block(SLOT_BLOCKS[slot], &mut blk).ok()?;
    if blk[QUOTA_OFF..QUOTA_OFF + 8] != QUOTA_MAGIC {
        return None;
    }
    let count = u64::from_le_bytes(blk[QUOTA_OFF + 8..QUOTA_OFF + 16].try_into().ok()?);
    if count == QUOTA_REBUILD {
        return Some(Err(()));
    }
    let count = count as usize;
    if count > QUOTA_MAX {
        return None;
    }
    let end = QUOTA_OFF + 16 + count * QUOTA_ENTRY;
    if end + 4 > BLOCK_SIZE {
        return None;
    }
    let crc = u32::from_le_bytes(blk[end..end + 4].try_into().ok()?);
    if crc != crate::checksum::checksum32(&blk[QUOTA_OFF..end]) {
        return None;
    }
    let mut map = std::collections::HashMap::new();
    let mut off = QUOTA_OFF + 16;
    for _ in 0..count {
        let uid = u32::from_le_bytes(blk[off..off + 4].try_into().ok()?);
        let blocks = u64::from_le_bytes(blk[off + 4..off + 12].try_into().ok()?);
        map.insert(uid, blocks);
        off += QUOTA_ENTRY;
    }
    Some(Ok(map))
}

/// T3: fuzz helpers.
pub mod fuzz {
    use super::*;

    /// Fuzz superblock header decode. Must not panic.
    pub fn decode_header(buf: &[u8]) {
        if buf.len() != HDR_LEN {
            return;
        }
        let mut hdr = [0u8; HDR_LEN];
        hdr.copy_from_slice(buf);
        let _ = Superblock::decode(&hdr);
    }
}
