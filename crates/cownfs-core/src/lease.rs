//! R6: robust leader lease on a dedicated block.
//!
//! The lease used to live in the superblock, which meant every acquire,
//! renew, and release rewrote *both* superblock slots via `write_slots`,
//! destroying the fallback generation. It also relied on `flock`, which
//! only serializes processes on one host, and superblock reads went
//! through the page cache.
//!
//! Now the lease lives on a dedicated block (`LEASE_BLOCK`, block 2),
//! leaving the superblock slots alone. Lease I/O uses `O_DIRECT` where
//! supported (falling back to buffered I/O, e.g. on tmpfs) so that two
//! hosts sharing a SAN device observe each other's writes without page
//! cache staleness.
//!
//! Fencing: the lease block carries a monotonically increasing `epoch`.
//! An `Fs` that acquires the lease remembers the epoch; every durable
//! commit re-reads the lease block and refuses (`FsError::Fenced`) if the
//! epoch changed, i.e. another node took the lease. This bounds split-brain:
//! at most the current lease holder's commits succeed.

use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::checksum::checksum;
use crate::BLOCK_SIZE;

/// Fixed location of the lease block (block 2; the bitmap starts at 3 on
/// R6+ images).
pub const LEASE_BLOCK: u64 = 2;

/// P0: grace period after lease expiry before a new holder can acquire.
/// Ensures the old holder has stopped writing (its commits will be fenced,
/// but in-flight data block writes need time to cease).
/// Set to 10s: enough for in-flight writes to drain, short enough for tests.
pub const LEASE_GRACE_SECS: u64 = 10;

/// Lease block magic: `b"COWLEASE"`.
const LEASE_MAGIC: [u8; 8] = *b"COWLEASE";

/// Lease state stored on the dedicated block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseState {
    /// UTF-8 node ID, NUL-padded. All zeros = no holder.
    pub holder: [u8; 32],
    /// Unix timestamp when the lease expires. 0 = no lease.
    pub expiry: u64,
    /// Fencing epoch, bumped whenever the holder changes.
    pub epoch: u64,
}

impl LeaseState {
    pub fn empty() -> Self {
        LeaseState {
            holder: [0u8; 32],
            expiry: 0,
            epoch: 0,
        }
    }

    pub fn holder_name(&self) -> String {
        let end = self.holder.iter().position(|&b| b == 0).unwrap_or(32);
        String::from_utf8_lossy(&self.holder[..end]).into_owned()
    }

    pub fn is_live(&self, now: u64) -> bool {
        !self.holder.iter().all(|&b| b == 0) && self.expiry > now
    }

    fn encode(&self) -> [u8; BLOCK_SIZE] {
        let mut blk = [0u8; BLOCK_SIZE];
        blk[0..8].copy_from_slice(&LEASE_MAGIC);
        blk[8..40].copy_from_slice(&self.holder);
        blk[40..48].copy_from_slice(&self.expiry.to_le_bytes());
        blk[48..56].copy_from_slice(&self.epoch.to_le_bytes());
        let crc = checksum(&blk[0..56]) as u32;
        blk[56..60].copy_from_slice(&crc.to_le_bytes());
        blk
    }

    fn decode(blk: &[u8; BLOCK_SIZE]) -> Option<Self> {
        if blk[0..8] != LEASE_MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(blk[56..60].try_into().ok()?);
        if crc != checksum(&blk[0..56]) as u32 {
            return None;
        }
        Some(LeaseState {
            holder: blk[8..40].try_into().ok()?,
            expiry: u64::from_le_bytes(blk[40..48].try_into().ok()?),
            epoch: u64::from_le_bytes(blk[48..56].try_into().ok()?),
        })
    }
}

/// Open the image for lease I/O, preferring `O_DIRECT` (bypass the page
/// cache so SAN peers observe writes promptly). Falls back to buffered
/// I/O where `O_DIRECT` is unsupported (e.g. tmpfs).
fn open_lease_fd(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    // Use libc::O_DIRECT for the correct value on each platform
    // (0o40000 on x86_64 Linux is O_DIRECTORY on aarch64).
    let direct = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path);
    match direct {
        Ok(f) => Ok(f),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            OpenOptions::new().read(true).write(true).open(path)
        }
        Err(e) => Err(e),
    }
}

/// 4 KiB-aligned buffer for `O_DIRECT`.
/// Uses `std::alloc` with 4K alignment; the Vec-based approach with
/// `drain()` does not actually align (drain shifts within the same
/// allocation, leaving the base pointer unchanged).
fn aligned_buf() -> AlignedBuf {
    AlignedBuf::new()
}

/// A 4 KiB-aligned 4 KiB buffer for O_DIRECT I/O.
struct AlignedBuf {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    fn new() -> Self {
        let layout = std::alloc::Layout::from_size_align(BLOCK_SIZE, BLOCK_SIZE)
            .expect("4K layout");
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc_zeroed failed");
        Self { ptr, layout }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, BLOCK_SIZE) }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, BLOCK_SIZE) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// Read the lease state from `block` (usually [`LEASE_BLOCK`]).
/// Returns `Ok(None)` if the block has no lease magic (legacy image).
pub fn read_lease(path: &Path, block: u64) -> io::Result<Option<LeaseState>> {
    use std::os::unix::fs::FileExt;
    let f = open_lease_fd(path)?;
    let mut buf = aligned_buf();
    f.read_exact_at(buf.as_mut_slice(), block * BLOCK_SIZE as u64)?;
    let arr: &[u8; BLOCK_SIZE] = buf
        .as_slice()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "short lease block read"))?;
    // N7: CRC mismatch on a magic-bearing block means a torn write
    // (crash during lease update), not fatal corruption. Treat as "no
    // lease" so the image remains openable; the next acquirer will
    // overwrite the torn block. A torn lease must not make the image
    // unopenable (including for fsck).
    if arr[0..8] == LEASE_MAGIC {
        match LeaseState::decode(arr) {
            Some(st) => return Ok(Some(st)),
            None => return Ok(None),
        }
    }
    Ok(None)
}

/// Write the lease state to `block` and fsync.
pub fn write_lease(path: &Path, block: u64, state: &LeaseState) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let f = open_lease_fd(path)?;
    let blk = state.encode();
    let mut buf = aligned_buf();
    buf.as_mut_slice().copy_from_slice(&blk);
    f.write_all_at(buf.as_slice(), block * BLOCK_SIZE as u64)?;
    f.sync_all()
}

/// Node ID bytes for the holder field.
pub fn node_id_bytes(node_id: &str) -> [u8; 32] {
    let mut b = [0u8; 32];
    let src = node_id.as_bytes();
    let n = src.len().min(32);
    b[..n].copy_from_slice(&src[..n]);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_block_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cownfs-lease-rt-{}.img", std::process::id()));
        // 16 MiB image.
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
        drop(f);

        assert_eq!(read_lease(&path, LEASE_BLOCK).unwrap(), None);
        let mut st = LeaseState::empty();
        st.holder = node_id_bytes("node-1");
        st.expiry = 1_800_000_000;
        st.epoch = 7;
        write_lease(&path, LEASE_BLOCK, &st).unwrap();
        let back = read_lease(&path, LEASE_BLOCK).unwrap().unwrap();
        assert_eq!(back, st);
        assert_eq!(back.holder_name(), "node-1");
        std::fs::remove_file(&path).ok();
    }
}
