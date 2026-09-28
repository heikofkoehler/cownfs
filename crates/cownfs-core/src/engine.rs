//! The cownfs filesystem engine: inodes, directories, and extents on top of
//! four copy-on-write B-trees, with transactional commit via the ping-pong
//! superblock.
//!
//! Data blocks are *never* overwritten in place: every write allocates fresh
//! blocks, updates the extent tree, and frees the old blocks. A crash before
//! [`Fs::commit`] can therefore only leak blocks, never expose torn data —
//! the previous superblock generation is always intact.

use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bitmap::Bitmap;
use crate::block::{BlockDevice, FileDevice};
use crate::btree::{BTree, NodeId};
use crate::store::{self, BlockArena, BlockCodec, Shared, StoreError};
use crate::superblock::{self, Superblock};
use crate::BLOCK_SIZE;

pub const ROOT_INO: u64 = 1;

/// File types (also used as NFSv4 file type values).
pub const FTYPE_FILE: u8 = 1;
pub const FTYPE_DIR: u8 = 2;
pub const FTYPE_SYMLINK: u8 = 3;

/// B-tree minimum degrees, sized so `2T-1` keys fit in a 4 KiB node block
/// (see `store::max_keys_per_node`; asserted by `BlockArena::new`).
pub const T_INODE: usize = 13;
pub const T_DIR: usize = 7;
pub const T_EXTENT: usize = 42;
pub const T_SNAP: usize = 14;

pub type InodeTree = BTree<u64, Inode, BlockArena<u64, Inode>, T_INODE>;
pub type DirTree = BTree<DirKey, DirEnt, BlockArena<DirKey, DirEnt>, T_DIR>;
pub type ExtentTree = BTree<ExtentKey, Extent, BlockArena<ExtentKey, Extent>, T_EXTENT>;
pub type SnapTree = BTree<u64, SnapRecord, BlockArena<u64, SnapRecord>, T_SNAP>;

#[derive(Debug)]
pub enum FsError {
    Store(StoreError),
    NotFound,
    AlreadyExists,
    NotDir,
    NotFile,
    NotEmpty,
    BadName,
    NoSpace,
    Invalid(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::Store(e) => write!(f, "{e}"),
            FsError::NotFound => write!(f, "no such file or directory"),
            FsError::AlreadyExists => write!(f, "file exists"),
            FsError::NotDir => write!(f, "not a directory"),
            FsError::NotFile => write!(f, "not a regular file"),
            FsError::NotEmpty => write!(f, "directory not empty"),
            FsError::BadName => write!(f, "invalid file name"),
            FsError::NoSpace => write!(f, "no space left on device"),
            FsError::Invalid(s) => write!(f, "invalid: {s}"),
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FsError::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for FsError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NoSpace => FsError::NoSpace,
            other => FsError::Store(other),
        }
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        FsError::Store(StoreError::Io(e))
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Inode record, 128 bytes on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inode {
    pub mode: u32, // permission bits (file type lives in `ftype`)
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64, // seconds since epoch
    pub mtime: u64,
    pub ctime: u64,
    pub gen: u64, // inode generation (filehandle stale detection)
    pub ftype: u8,
    pub parent: u64, // parent inode (single-parent: no hardlinks in v1)
}

impl BlockCodec for Inode {
    const SIZE: usize = 128;
    fn encode(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.mode.to_le_bytes());
        out[4..8].copy_from_slice(&self.uid.to_le_bytes());
        out[8..12].copy_from_slice(&self.gid.to_le_bytes());
        out[12..16].copy_from_slice(&self.nlink.to_le_bytes());
        out[16..24].copy_from_slice(&self.size.to_le_bytes());
        out[24..32].copy_from_slice(&self.atime.to_le_bytes());
        out[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        out[40..48].copy_from_slice(&self.ctime.to_le_bytes());
        out[48..56].copy_from_slice(&self.gen.to_le_bytes());
        out[56] = self.ftype;
        out[64..72].copy_from_slice(&self.parent.to_le_bytes());
    }
    fn decode(raw: &[u8]) -> Self {
        let u32_at = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
        Inode {
            mode: u32_at(0),
            uid: u32_at(4),
            gid: u32_at(8),
            nlink: u32_at(12),
            size: u64_at(16),
            atime: u64_at(24),
            mtime: u64_at(32),
            ctime: u64_at(40),
            gen: u64_at(48),
            ftype: raw[56],
            parent: u64_at(64),
        }
    }
}

/// Directory key: `(parent_inode, name)`, 264 bytes. Names are null-padded;
/// padding compares correctly for prefix names (`"a" < "ab"`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirKey {
    pub parent: u64,
    pub name: [u8; 256],
}

impl DirKey {
    pub fn new(parent: u64, name: &[u8]) -> Result<Self, FsError> {
        if name.is_empty() || name.len() > 255 || name.contains(&b'/') || name.contains(&0) {
            return Err(FsError::BadName);
        }
        let mut arr = [0u8; 256];
        arr[..name.len()].copy_from_slice(name);
        Ok(DirKey { parent, name: arr })
    }

    pub fn name_bytes(&self) -> &[u8] {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(256);
        &self.name[..n]
    }

    /// Inclusive range covering every name under `parent`.
    pub fn range_for(parent: u64) -> (DirKey, DirKey) {
        (
            DirKey {
                parent,
                name: [0u8; 256],
            },
            DirKey {
                parent,
                name: [0xffu8; 256],
            },
        )
    }
}

impl BlockCodec for DirKey {
    const SIZE: usize = 264;
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.parent.to_le_bytes());
        out[8..264].copy_from_slice(&self.name);
    }
    fn decode(raw: &[u8]) -> Self {
        DirKey {
            parent: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
            name: raw[8..264].try_into().unwrap(),
        }
    }
}

/// Directory entry value, 16 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEnt {
    pub ino: u64,
    pub typ: u8,
}

impl BlockCodec for DirEnt {
    const SIZE: usize = 16;
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8] = self.typ;
    }
    fn decode(raw: &[u8]) -> Self {
        DirEnt {
            ino: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
            typ: raw[8],
        }
    }
}

/// Extent key: `(inode, file_block_index)`, 16 bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtentKey {
    pub ino: u64,
    pub off: u64,
}

impl BlockCodec for ExtentKey {
    const SIZE: usize = 16;
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8..16].copy_from_slice(&self.off.to_le_bytes());
    }
    fn decode(raw: &[u8]) -> Self {
        ExtentKey {
            ino: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
            off: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
        }
    }
}

/// Extent value: one file block, 16 bytes. (`len` is reserved for future
/// multi-block extents; always 1 in P2.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extent {
    pub blk: u64,
    pub len: u32,
}

impl BlockCodec for Extent {
    const SIZE: usize = 16;
    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.blk.to_le_bytes());
        out[8..12].copy_from_slice(&self.len.to_le_bytes());
    }
    fn decode(raw: &[u8]) -> Self {
        Extent {
            blk: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
            len: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        }
    }
}

/// Snapshot record, 124 bytes: the three tree roots (+ generations, lengths)
/// captured at snapshot time, plus a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapRecord {
    pub roots: [u64; 3],
    pub root_gens: [u32; 3],
    pub lens: [u64; 3],
    pub name: [u8; 64],
}

impl BlockCodec for SnapRecord {
    const SIZE: usize = 124;
    fn encode(&self, out: &mut [u8]) {
        for (i, r) in self.roots.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&r.to_le_bytes());
        }
        for (i, g) in self.root_gens.iter().enumerate() {
            out[24 + i * 4..24 + i * 4 + 4].copy_from_slice(&g.to_le_bytes());
        }
        for (i, l) in self.lens.iter().enumerate() {
            out[36 + i * 8..36 + i * 8 + 8].copy_from_slice(&l.to_le_bytes());
        }
        out[60..124].copy_from_slice(&self.name);
    }
    fn decode(raw: &[u8]) -> Self {
        let mut roots = [0u64; 3];
        let mut root_gens = [0u32; 3];
        let mut lens = [0u64; 3];
        for i in 0..3 {
            roots[i] = u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
            root_gens[i] = u32::from_le_bytes(raw[24 + i * 4..24 + i * 4 + 4].try_into().unwrap());
            lens[i] = u64::from_le_bytes(raw[36 + i * 8..36 + i * 8 + 8].try_into().unwrap());
        }
        SnapRecord {
            roots,
            root_gens,
            lens,
            name: raw[60..124].try_into().unwrap(),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fs
// ---------------------------------------------------------------------------

/// Summary of a consistency check.
#[derive(Debug)]
pub struct CheckReport {
    /// Metadata blocks reachable from the four tree roots.
    pub meta_blocks: u64,
    /// Data blocks referenced by extents.
    pub data_blocks: u64,
    /// Bits set in the active bitmap area.
    pub allocated_blocks: u64,
}

pub struct Fs {
    shared: Rc<RefCell<Shared>>,
    sb: Superblock,
    active_slot: usize,
    inodes: InodeTree,
    dirs: DirTree,
    extents: ExtentTree,
    snaps: SnapTree,
    next_inode: u64,
    next_snap: u64,
}

/// Optional setattr fields.
#[derive(Default)]
pub struct SetAttrs {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<u64>,
    pub mtime: Option<u64>,
}

impl Fs {
    // -- lifecycle ---------------------------------------------------------

    /// Format a fresh filesystem image with an empty root directory.
    pub fn format(path: &Path, blocks: u64) -> Result<Self, FsError> {
        let mut dev = FileDevice::create(path, blocks)?;
        let bblocks = store::bitmap_blocks_for(blocks);
        let reserved = 2 + 2 * bblocks; // slots + two alternating bitmap areas
        if blocks < reserved + 64 {
            return Err(FsError::Invalid("image too small to format".into()));
        }
        let mut bitmap = Bitmap::new(blocks);
        for b in 0..reserved {
            bitmap.set(b);
        }
        store::write_bitmap(&mut dev, &bitmap, 2, bblocks)?;
        let shared = Rc::new(RefCell::new(Shared {
            dev,
            bitmap,
            pending_free: Vec::new(),
        }));

        let (ia, iroot) = BlockArena::new_tree(Rc::clone(&shared), T_INODE)?;
        let (da, droot) = BlockArena::new_tree(Rc::clone(&shared), T_DIR)?;
        let (ea, eroot) = BlockArena::new_tree(Rc::clone(&shared), T_EXTENT)?;
        let (sa, sroot) = BlockArena::new_tree(Rc::clone(&shared), T_SNAP)?;

        let mut fs = Fs {
            shared: Rc::clone(&shared),
            sb: Superblock::blank(blocks, 2, bblocks),
            active_slot: 0,
            inodes: InodeTree::open(ia, iroot, 0),
            dirs: DirTree::open(da, droot, 0),
            extents: ExtentTree::open(ea, eroot, 0),
            snaps: SnapTree::open(sa, sroot, 0),
            next_inode: ROOT_INO + 1,
            next_snap: 1,
        };

        let now = now_secs();
        fs.inodes.insert(
            ROOT_INO,
            Inode {
                mode: 0o755,
                uid: 0,
                gid: 0,
                nlink: 2,
                size: 0,
                atime: now,
                mtime: now,
                ctime: now,
                gen: ROOT_INO,
                ftype: FTYPE_DIR,
                parent: ROOT_INO,
            },
        )?;
        fs.dirs.insert(
            DirKey::new(ROOT_INO, b".")?,
            DirEnt {
                ino: ROOT_INO,
                typ: FTYPE_DIR,
            },
        )?;
        fs.dirs.insert(
            DirKey::new(ROOT_INO, b"..")?,
            DirEnt {
                ino: ROOT_INO,
                typ: FTYPE_DIR,
            },
        )?;

        fs.sb.generation = 1;
        fs.commit_to_slots()?;
        Ok(fs)
    }

    /// Open an existing image, locating all trees from the superblock.
    pub fn open(path: &Path) -> Result<Self, FsError> {
        let mut dev = FileDevice::open(path)?;
        let (sb, active_slot) = superblock::open(&dev)?;
        let next_inode = sb.next_inode;
        let next_snap = sb.next_snap;
        let bitmap = store::read_bitmap(
            &mut dev,
            sb.block_count,
            sb.bitmap_area_start(),
            sb.bitmap_blocks,
        )?;
        let shared = Rc::new(RefCell::new(Shared {
            dev,
            bitmap,
            pending_free: Vec::new(),
        }));

        let ia: Rc<RefCell<BlockArena<u64, Inode>>> =
            Rc::new(RefCell::new(BlockArena::new(Rc::clone(&shared), T_INODE)));
        let da: Rc<RefCell<BlockArena<DirKey, DirEnt>>> =
            Rc::new(RefCell::new(BlockArena::new(Rc::clone(&shared), T_DIR)));
        let ea: Rc<RefCell<BlockArena<ExtentKey, Extent>>> =
            Rc::new(RefCell::new(BlockArena::new(Rc::clone(&shared), T_EXTENT)));
        let sa: Rc<RefCell<BlockArena<u64, SnapRecord>>> =
            Rc::new(RefCell::new(BlockArena::new(Rc::clone(&shared), T_SNAP)));

        let inodes = InodeTree::open(
            Rc::clone(&ia),
            NodeId {
                idx: sb.inode_root,
                gen: sb.inode_root_gen,
            },
            sb.inode_len as usize,
        );
        let dirs = DirTree::open(
            Rc::clone(&da),
            NodeId {
                idx: sb.dir_root,
                gen: sb.dir_root_gen,
            },
            sb.dir_len as usize,
        );
        let extents = ExtentTree::open(
            Rc::clone(&ea),
            NodeId {
                idx: sb.extent_root,
                gen: sb.extent_root_gen,
            },
            sb.extent_len as usize,
        );
        let snaps = SnapTree::open(
            Rc::clone(&sa),
            NodeId {
                idx: sb.snap_root,
                gen: sb.snap_root_gen,
            },
            sb.snap_len as usize,
        );
        // Seed each arena's live-node count from the on-disk trees; without
        // this the first take/free after reopen underflows the counter.
        // (Counts are computed before any mutable borrow: count_reachable
        // borrows the same RefCell.)
        let n_inodes = inodes.count_reachable()?;
        let n_dirs = dirs.count_reachable()?;
        let n_extents = extents.count_reachable()?;
        let n_snaps = snaps.count_reachable()?;
        ia.borrow_mut().set_live(n_inodes);
        da.borrow_mut().set_live(n_dirs);
        ea.borrow_mut().set_live(n_extents);
        sa.borrow_mut().set_live(n_snaps);

        Ok(Fs {
            shared,
            sb,
            active_slot,
            inodes,
            dirs,
            extents,
            snaps,
            next_inode,
            next_snap,
        })
    }

    /// Filesystem UUID (for filehandles).
    pub fn uuid(&self) -> [u8; 16] {
        self.sb.uuid
    }

    pub fn block_count(&self) -> u64 {
        self.sb.block_count
    }

    pub fn generation(&self) -> u64 {
        self.sb.generation
    }

    // -- transactions ------------------------------------------------------

    fn flush_all(&self) -> Result<(), FsError> {
        self.inodes.flush()?;
        self.dirs.flush()?;
        self.extents.flush()?;
        self.snaps.flush()?;
        Ok(())
    }

    /// Apply deferred frees, then write the bitmap to the area starting at
    /// `area_start` (the inactive area on commit; the active area on mkfs).
    fn persist_bitmap(&mut self, area_start: u64) -> Result<(), FsError> {
        {
            let mut sh = self.shared.borrow_mut();
            let freed = std::mem::take(&mut sh.pending_free);
            for b in freed {
                sh.bitmap.clear(b);
            }
        }
        let blocks = self.sb.bitmap_blocks;
        // Serialize first (immutable borrow ends before the device write).
        let raw = self.shared.borrow().bitmap.to_bytes();
        let mut sh = self.shared.borrow_mut();
        let mut buf = vec![0u8; blocks as usize * BLOCK_SIZE];
        let n = raw.len().min(buf.len());
        buf[..n].copy_from_slice(&raw[..n]);
        for (i, chunk) in buf.chunks_exact(BLOCK_SIZE).enumerate() {
            let mut blk = [0u8; BLOCK_SIZE];
            blk.copy_from_slice(chunk);
            sh.dev.write_block(area_start + i as u64, &blk)?;
        }
        Ok(())
    }

    fn sync_roots(&mut self) {
        let r = self.inodes.root_id();
        self.sb.inode_root = r.idx;
        self.sb.inode_root_gen = r.gen;
        self.sb.inode_len = self.inodes.len() as u64;
        let r = self.dirs.root_id();
        self.sb.dir_root = r.idx;
        self.sb.dir_root_gen = r.gen;
        self.sb.dir_len = self.dirs.len() as u64;
        let r = self.extents.root_id();
        self.sb.extent_root = r.idx;
        self.sb.extent_root_gen = r.gen;
        self.sb.extent_len = self.extents.len() as u64;
        let r = self.snaps.root_id();
        self.sb.snap_root = r.idx;
        self.sb.snap_root_gen = r.gen;
        self.sb.snap_len = self.snaps.len() as u64;
        self.sb.next_inode = self.next_inode;
        self.sb.next_snap = self.next_snap;
    }

    /// Write both superblock slots as-is (mkfs path). The bitmap goes to
    /// the current area; no flip (generation 1 lives with area 0).
    fn commit_to_slots(&mut self) -> Result<(), FsError> {
        self.flush_all()?;
        let area = self.sb.bitmap_area_start();
        self.persist_bitmap(area)?;
        self.shared.borrow_mut().dev.sync()?;
        self.sync_roots();
        let mut sh = self.shared.borrow_mut();
        superblock::write_slots(&mut sh.dev, &self.sb)?;
        Ok(())
    }

    /// Atomically commit the current transaction.
    ///
    /// Ordering: flush dirty nodes (freezing them against in-place
    /// rewrites) -> apply deferred frees and write the bitmap to the
    /// *inactive* area -> fsync -> flip `bitmap_area`, write the inactive
    /// superblock slot, fsync. A crash at any point before the slot write
    /// leaves the previous slot pointing at the previous bitmap area: the
    /// previous generation stays fully consistent.
    pub fn commit(&mut self) -> Result<(), FsError> {
        self.flush_all()?;
        let inactive = self.sb.bitmap_start + (1 - self.sb.bitmap_area) * self.sb.bitmap_blocks;
        self.persist_bitmap(inactive)?;
        // The new generation's blocks must be on stable storage *before*
        // any superblock slot points at them.
        self.shared.borrow_mut().dev.sync()?;
        self.sync_roots();
        self.sb.bitmap_area = 1 - self.sb.bitmap_area;
        let mut sh = self.shared.borrow_mut();
        superblock::commit_generation(&mut sh.dev, &mut self.sb, &mut self.active_slot)?;
        Ok(())
    }

    /// Full consistency check of the committed state.
    ///
    /// Verifies all four trees (node checksums and generations are checked
    /// on load; key order and link resolvability are checked by the walk),
    /// then reconciles every reachable block against the active bitmap
    /// area: each reachable block must be marked allocated, and each
    /// allocated non-reserved block must be reachable.
    pub fn check(&self) -> Result<CheckReport, FsError> {
        use std::collections::HashSet;

        let mut reachable: HashSet<u64> = HashSet::new();
        for ids in [
            self.inodes.verify()?,
            self.dirs.verify()?,
            self.extents.verify()?,
            self.snaps.verify()?,
        ] {
            reachable.extend(ids.iter().map(|id| id.idx));
        }
        let meta_blocks = reachable.len() as u64;

        // Data blocks referenced by extents.
        let mut data_blocks = 0u64;
        for (_, ext) in self.extents.to_sorted_vec()? {
            for b in ext.blk..ext.blk + ext.len as u64 {
                if !reachable.insert(b) {
                    return Err(FsError::Invalid(format!("data block {b} referenced twice")));
                }
                data_blocks += 1;
            }
        }

        // Reconcile against the active bitmap area.
        let sh = self.shared.borrow();
        let reserved = 2 + 2 * self.sb.bitmap_blocks;
        let mut allocated_blocks = 0u64;
        for b in 0..self.sb.block_count {
            if !sh.bitmap.test(b) {
                continue;
            }
            allocated_blocks += 1;
            if b >= reserved && !reachable.contains(&b) {
                return Err(FsError::Invalid(format!(
                    "block {b} marked allocated but unreachable"
                )));
            }
        }
        for &b in &reachable {
            if !sh.bitmap.test(b) {
                return Err(FsError::Invalid(format!(
                    "reachable block {b} not marked allocated"
                )));
            }
        }

        Ok(CheckReport {
            meta_blocks,
            data_blocks,
            allocated_blocks,
        })
    }

    // -- block helpers -----------------------------------------------------

    fn alloc_block(&mut self) -> Result<u64, FsError> {
        self.shared
            .borrow_mut()
            .bitmap
            .alloc()
            .ok_or(FsError::NoSpace)
    }

    /// Free a data block. Deferred like metadata frees: the bitmap bit is
    /// cleared at commit, so the block cannot be reallocated while the
    /// committed generation still references it.
    fn free_block(&mut self, blk: u64) {
        self.shared.borrow_mut().pending_free.push(blk);
    }

    fn read_block(&self, blk: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
        self.shared.borrow_mut().dev.read_block(blk, buf)?;
        Ok(())
    }

    fn write_block(&mut self, blk: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), FsError> {
        self.shared.borrow_mut().dev.write_block(blk, buf)?;
        Ok(())
    }

    // -- metadata ops ------------------------------------------------------

    pub fn getattr(&self, ino: u64) -> Result<Inode, FsError> {
        self.inodes.get(&ino)?.ok_or(FsError::NotFound)
    }

    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<Option<(u64, u8)>, FsError> {
        let p = self.getattr(parent)?;
        if p.ftype != FTYPE_DIR {
            return Err(FsError::NotDir);
        }
        Ok(self
            .dirs
            .get(&DirKey::new(parent, name)?)?
            .map(|e| (e.ino, e.typ)))
    }

    fn mknode(
        &mut self,
        parent: u64,
        name: &[u8],
        ftype: u8,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, FsError> {
        let p = self.getattr(parent)?;
        if p.ftype != FTYPE_DIR {
            return Err(FsError::NotDir);
        }
        let key = DirKey::new(parent, name)?;
        if self.dirs.get(&key)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        let now = now_secs();
        self.inodes.insert(
            ino,
            Inode {
                mode: mode & 0o7777,
                uid,
                gid,
                nlink: if ftype == FTYPE_DIR { 2 } else { 1 },
                size: 0,
                atime: now,
                mtime: now,
                ctime: now,
                gen: ino,
                ftype,
                parent,
            },
        )?;
        self.dirs.insert(key, DirEnt { ino, typ: ftype })?;
        if ftype == FTYPE_DIR {
            self.dirs.insert(
                DirKey::new(ino, b".")?,
                DirEnt {
                    ino,
                    typ: FTYPE_DIR,
                },
            )?;
            self.dirs.insert(
                DirKey::new(ino, b"..")?,
                DirEnt {
                    ino: parent,
                    typ: FTYPE_DIR,
                },
            )?;
        }
        let mut pp = p;
        pp.mtime = now;
        pp.ctime = now;
        self.inodes.insert(parent, pp)?;
        Ok(ino)
    }

    pub fn create(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, FsError> {
        self.mknode(parent, name, FTYPE_FILE, mode, uid, gid)
    }

    pub fn mkdir(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, FsError> {
        self.mknode(parent, name, FTYPE_DIR, mode, uid, gid)
    }

    pub fn symlink(
        &mut self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<u64, FsError> {
        let ino = self.mknode(parent, name, FTYPE_SYMLINK, 0o777, uid, gid)?;
        self.write(ino, 0, target)?;
        Ok(ino)
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, FsError> {
        let inode = self.getattr(ino)?;
        if inode.ftype != FTYPE_SYMLINK {
            return Err(FsError::Invalid("not a symlink".into()));
        }
        self.read(ino, 0, inode.size as usize)
    }

    /// Remove a name. Frees the inode (and its blocks) at the last link.
    pub fn unlink(&mut self, parent: u64, name: &[u8]) -> Result<(), FsError> {
        let key = DirKey::new(parent, name)?;
        let ent = self.dirs.remove(&key)?.ok_or(FsError::NotFound)?;
        if ent.typ == FTYPE_DIR {
            // Put it back: rmdir() is the way to remove directories.
            self.dirs.insert(key, ent)?;
            return Err(FsError::Invalid("is a directory".into()));
        }
        self.drop_link(ent.ino)?;
        let now = now_secs();
        let mut p = self.getattr(parent)?;
        p.mtime = now;
        p.ctime = now;
        self.inodes.insert(parent, p)?;
        Ok(())
    }

    pub fn rmdir(&mut self, parent: u64, name: &[u8]) -> Result<(), FsError> {
        let key = DirKey::new(parent, name)?;
        let ent = self.dirs.get(&key)?.ok_or(FsError::NotFound)?;
        if ent.typ != FTYPE_DIR {
            return Err(FsError::NotDir);
        }
        if ent.ino == ROOT_INO {
            return Err(FsError::Invalid("cannot remove root".into()));
        }
        let (lo, hi) = DirKey::range_for(ent.ino);
        // Only "." and ".." may remain.
        if self.dirs.range(&lo, &hi)?.len() != 2 {
            return Err(FsError::NotEmpty);
        }
        self.dirs.remove(&DirKey::new(ent.ino, b".")?)?;
        self.dirs.remove(&DirKey::new(ent.ino, b"..")?)?;
        self.dirs.remove(&key)?;
        // Directory nlink drops by 2 (".." of the child pointed here, plus
        // the named link); v1 keeps it simple: remove the inode directly.
        self.inodes.remove(&ent.ino)?;
        let now = now_secs();
        let mut p = self.getattr(parent)?;
        p.mtime = now;
        p.ctime = now;
        self.inodes.insert(parent, p)?;
        Ok(())
    }

    /// Decrement `nlink`; at zero, free extents and the inode.
    fn drop_link(&mut self, ino: u64) -> Result<(), FsError> {
        let mut inode = self.getattr(ino)?;
        inode.nlink -= 1;
        if inode.nlink > 0 {
            self.inodes.insert(ino, inode)?;
            return Ok(());
        }
        let nblocks = inode.size.div_ceil(BLOCK_SIZE as u64);
        for blk_off in 0..nblocks {
            let k = ExtentKey { ino, off: blk_off };
            if let Some(ext) = self.extents.remove(&k)? {
                self.free_block(ext.blk);
            }
        }
        self.inodes.remove(&ino)?;
        Ok(())
    }

    pub fn rename(&mut self, sp: u64, sn: &[u8], dp: u64, dn: &[u8]) -> Result<(), FsError> {
        let skey = DirKey::new(sp, sn)?;
        let ent = self.dirs.get(&skey)?.ok_or(FsError::NotFound)?;
        let d = self.getattr(dp)?;
        if d.ftype != FTYPE_DIR {
            return Err(FsError::NotDir);
        }
        let dkey = DirKey::new(dp, dn)?;
        if skey == dkey {
            return Ok(()); // no-op
        }
        // Refuse to move a directory into its own subtree.
        if ent.typ == FTYPE_DIR {
            let mut cur = dp;
            loop {
                if cur == ent.ino {
                    return Err(FsError::Invalid("cannot move dir into itself".into()));
                }
                if cur == ROOT_INO {
                    break;
                }
                cur = self.getattr(cur)?.parent;
            }
        }
        if let Some(existing) = self.dirs.get(&dkey)? {
            if existing.typ == FTYPE_DIR {
                let (lo, hi) = DirKey::range_for(existing.ino);
                if self.dirs.range(&lo, &hi)?.len() != 2 {
                    return Err(FsError::NotEmpty);
                }
                self.dirs.remove(&DirKey::new(existing.ino, b".")?)?;
                self.dirs.remove(&DirKey::new(existing.ino, b"..")?)?;
                self.inodes.remove(&existing.ino)?;
            } else {
                self.dirs.remove(&dkey)?;
                self.drop_link(existing.ino)?;
            }
        }
        self.dirs.remove(&skey)?;
        self.dirs.insert(
            dkey,
            DirEnt {
                ino: ent.ino,
                typ: ent.typ,
            },
        )?;
        // The moved inode's parent always changes (single-parent model).
        let mut moved = self.getattr(ent.ino)?;
        moved.parent = dp;
        self.inodes.insert(ent.ino, moved)?;
        if ent.typ == FTYPE_DIR {
            // Re-point ".." at the new parent.
            self.dirs.insert(
                DirKey::new(ent.ino, b"..")?,
                DirEnt {
                    ino: dp,
                    typ: FTYPE_DIR,
                },
            )?;
        }
        let now = now_secs();
        for p in [sp, dp] {
            let mut pin = self.getattr(p)?;
            pin.mtime = now;
            pin.ctime = now;
            self.inodes.insert(p, pin)?;
        }
        Ok(())
    }

    /// `(name, inode, type)` sorted by name. Excludes "." and "..".
    pub fn readdir(&self, ino: u64) -> Result<Vec<(Vec<u8>, u64, u8)>, FsError> {
        let inode = self.getattr(ino)?;
        if inode.ftype != FTYPE_DIR {
            return Err(FsError::NotDir);
        }
        let (lo, hi) = DirKey::range_for(ino);
        let mut out = Vec::new();
        for (k, e) in self.dirs.range(&lo, &hi)? {
            let name = k.name_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            out.push((name.to_vec(), e.ino, e.typ));
        }
        Ok(out)
    }

    pub fn setattr(&mut self, ino: u64, attrs: &SetAttrs) -> Result<(), FsError> {
        if let Some(size) = attrs.size {
            self.truncate(ino, size)?;
        }
        let mut inode = self.getattr(ino)?;
        if let Some(m) = attrs.mode {
            inode.mode = m & 0o7777;
        }
        if let Some(u) = attrs.uid {
            inode.uid = u;
        }
        if let Some(g) = attrs.gid {
            inode.gid = g;
        }
        let now = now_secs();
        inode.atime = attrs.atime.unwrap_or(inode.atime);
        inode.mtime = attrs.mtime.unwrap_or(now);
        inode.ctime = now;
        self.inodes.insert(ino, inode)?;
        Ok(())
    }

    // -- data ops ----------------------------------------------------------

    pub fn read(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let inode = self.getattr(ino)?;
        if inode.ftype != FTYPE_FILE && inode.ftype != FTYPE_SYMLINK {
            return Err(FsError::NotFile);
        }
        let end = (offset + len as u64).min(inode.size);
        if offset >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut buf = [0u8; BLOCK_SIZE];
        let mut foff = offset;
        while foff < end {
            let blk_off = foff / BLOCK_SIZE as u64;
            let in_blk = (foff % BLOCK_SIZE as u64) as usize;
            let n = ((BLOCK_SIZE - in_blk) as u64).min(end - foff) as usize;
            match self.extents.get(&ExtentKey { ino, off: blk_off })? {
                Some(ext) => {
                    self.read_block(ext.blk, &mut buf)?;
                    out.extend_from_slice(&buf[in_blk..in_blk + n]);
                }
                None => out.extend(std::iter::repeat(0).take(n)),
            }
            foff += n as u64;
        }
        Ok(out)
    }

    /// Copy-on-write write: touched blocks are always freshly allocated;
    /// old blocks are freed (P3: unless a snapshot still references them).
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<(), FsError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut inode = self.getattr(ino)?;
        if inode.ftype != FTYPE_FILE && inode.ftype != FTYPE_SYMLINK {
            return Err(FsError::NotFile);
        }
        let mut pos = 0usize;
        while pos < data.len() {
            let foff = offset + pos as u64;
            let blk_off = foff / BLOCK_SIZE as u64;
            let in_blk = (foff % BLOCK_SIZE as u64) as usize;
            let n = (BLOCK_SIZE - in_blk).min(data.len() - pos);
            let key = ExtentKey { ino, off: blk_off };
            let old = self.extents.get(&key)?;
            let new_blk = self.alloc_block()?;
            let mut buf = [0u8; BLOCK_SIZE];
            if let Some(ext) = old {
                self.read_block(ext.blk, &mut buf)?;
                self.free_block(ext.blk);
            }
            buf[in_blk..in_blk + n].copy_from_slice(&data[pos..pos + n]);
            self.write_block(new_blk, &buf)?;
            self.extents.insert(
                key,
                Extent {
                    blk: new_blk,
                    len: 1,
                },
            )?;
            pos += n;
        }
        let end = offset + data.len() as u64;
        if end > inode.size {
            inode.size = end;
        }
        let now = now_secs();
        inode.mtime = now;
        inode.ctime = now;
        self.inodes.insert(ino, inode)?;
        Ok(())
    }

    pub fn truncate(&mut self, ino: u64, size: u64) -> Result<(), FsError> {
        let mut inode = self.getattr(ino)?;
        if inode.ftype != FTYPE_FILE && inode.ftype != FTYPE_SYMLINK {
            return Err(FsError::NotFile);
        }
        if size < inode.size {
            let first_gone = size.div_ceil(BLOCK_SIZE as u64);
            let last = inode.size.div_ceil(BLOCK_SIZE as u64);
            for blk_off in first_gone..last {
                let k = ExtentKey { ino, off: blk_off };
                if let Some(ext) = self.extents.remove(&k)? {
                    self.free_block(ext.blk);
                }
            }
            if size % BLOCK_SIZE as u64 != 0 && first_gone > 0 {
                // Zero the tail of the new last block (CoW: never modify a
                // live data block in place).
                let blk_off = first_gone - 1;
                if let Some(ext) = self.extents.get(&ExtentKey { ino, off: blk_off })? {
                    let mut buf = [0u8; BLOCK_SIZE];
                    self.read_block(ext.blk, &mut buf)?;
                    buf[(size % BLOCK_SIZE as u64) as usize..].fill(0);
                    let nb = self.alloc_block()?;
                    self.write_block(nb, &buf)?;
                    self.extents
                        .insert(ExtentKey { ino, off: blk_off }, Extent { blk: nb, len: 1 })?;
                    self.free_block(ext.blk);
                }
            }
            inode.size = size;
        } else if size > inode.size {
            inode.size = size; // holes read as zeros
        }
        let now = now_secs();
        inode.mtime = now;
        inode.ctime = now;
        self.inodes.insert(ino, inode)?;
        Ok(())
    }

    // -- P3: snapshots -----------------------------------------------------
    // (Snapshot *management* lands in P3; the snap tree root is already
    // carried in the superblock so images stay forward-compatible.)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fs(blocks: u64) -> (Fs, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "cownfs-engine-test-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fs = Fs::format(&path, blocks).unwrap();
        (fs, path)
    }

    #[test]
    fn format_open_roundtrip() {
        let (mut fs, path) = test_fs(512);
        let ino = fs
            .create(ROOT_INO, b"hello.txt", 0o644, 1000, 1000)
            .unwrap();
        fs.write(ino, 0, b"hello, cow").unwrap();
        let gen = fs.generation();
        fs.commit().unwrap();
        assert_eq!(fs.generation(), gen + 1);
        drop(fs);

        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.generation(), gen + 1);
        let (found, typ) = fs2.lookup(ROOT_INO, b"hello.txt").unwrap().unwrap();
        assert_eq!((found, typ), (ino, FTYPE_FILE));
        assert_eq!(fs2.read(ino, 0, 99).unwrap(), b"hello, cow");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn uncommitted_changes_are_lost() {
        let (mut fs, path) = test_fs(512);
        let ino = fs.create(ROOT_INO, b"a", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, b"committed").unwrap();
        fs.commit().unwrap();
        // Uncommitted changes must not survive reopen.
        let ino2 = fs.create(ROOT_INO, b"b", 0o644, 0, 0).unwrap();
        fs.write(ino2, 0, b"uncommitted").unwrap();
        drop(fs);

        let fs2 = Fs::open(&path).unwrap();
        assert!(fs2.lookup(ROOT_INO, b"a").unwrap().is_some());
        assert!(fs2.lookup(ROOT_INO, b"b").unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dirs_and_rename() {
        let (mut fs, path) = test_fs(512);
        let d = fs.mkdir(ROOT_INO, b"sub", 0o755, 0, 0).unwrap();
        let f = fs.create(d, b"f", 0o644, 0, 0).unwrap();
        fs.write(f, 0, b"data").unwrap();
        // non-empty rmdir fails while f is still inside
        assert!(matches!(fs.rmdir(ROOT_INO, b"sub"), Err(FsError::NotEmpty)));
        fs.rename(d, b"f", ROOT_INO, b"g").unwrap();
        assert!(fs.lookup(d, b"f").unwrap().is_none());
        let (ino, _) = fs.lookup(ROOT_INO, b"g").unwrap().unwrap();
        assert_eq!(ino, f);
        assert_eq!(fs.read(f, 0, 10).unwrap(), b"data");
        // moved file's parent updated
        assert_eq!(fs.getattr(f).unwrap().parent, ROOT_INO);
        fs.unlink(ROOT_INO, b"g").unwrap();
        fs.rmdir(ROOT_INO, b"sub").unwrap();
        assert!(fs.lookup(ROOT_INO, b"sub").unwrap().is_none());
        fs.commit().unwrap();
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        assert!(fs2.lookup(ROOT_INO, b"g").unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn large_file_multi_block() {
        let (mut fs, path) = test_fs(2048);
        let ino = fs.create(ROOT_INO, b"big", 0o644, 0, 0).unwrap();
        // Pseudo-random data spanning many blocks, unaligned.
        let mut data = Vec::new();
        let mut x = 0x12345678u64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.push((x >> 11) as u8);
        }
        fs.write(ino, 123, &data).unwrap();
        fs.write(ino, 0, b"head").unwrap();
        // Holes read as zeros.
        let back = fs.read(ino, 0, 123 + data.len()).unwrap();
        assert_eq!(&back[0..4], b"head");
        assert!(back[4..123].iter().all(|&b| b == 0));
        assert_eq!(&back[123..], &data[..]);
        assert_eq!(fs.getattr(ino).unwrap().size, 123 + data.len() as u64);
        fs.commit().unwrap();
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.read(ino, 0, 123 + data.len()).unwrap(), back);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn truncate_shrink_grow() {
        let (mut fs, path) = test_fs(512);
        let ino = fs.create(ROOT_INO, b"t", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, &[7u8; 9000]).unwrap();
        fs.truncate(ino, 100).unwrap();
        assert_eq!(fs.getattr(ino).unwrap().size, 100);
        assert_eq!(fs.read(ino, 0, 200).unwrap(), vec![7u8; 100]);
        // Grow: hole reads as zeros.
        fs.truncate(ino, 5000).unwrap();
        let back = fs.read(ino, 0, 5000).unwrap();
        assert_eq!(&back[..100], &[7u8; 100]);
        assert!(back[100..].iter().all(|&b| b == 0));
        fs.commit().unwrap();
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.getattr(ino).unwrap().size, 5000);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn symlink_roundtrip() {
        let (mut fs, path) = test_fs(512);
        let ino = fs
            .symlink(ROOT_INO, b"link", b"/some/target", 0, 0)
            .unwrap();
        assert_eq!(fs.readlink(ino).unwrap(), b"/some/target");
        let (_, typ) = fs.lookup(ROOT_INO, b"link").unwrap().unwrap();
        assert_eq!(typ, FTYPE_SYMLINK);
        fs.commit().unwrap();
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.readlink(ino).unwrap(), b"/some/target");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn setattr_and_errors() {
        let (mut fs, path) = test_fs(512);
        let ino = fs.create(ROOT_INO, b"s", 0o644, 1000, 1000).unwrap();
        fs.setattr(
            ino,
            &SetAttrs {
                mode: Some(0o600),
                size: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        let a = fs.getattr(ino).unwrap();
        assert_eq!(a.mode, 0o600);
        assert_eq!(a.size, 10);
        assert!(matches!(
            fs.create(ROOT_INO, b"s", 0o644, 0, 0),
            Err(FsError::AlreadyExists)
        ));
        assert!(matches!(
            fs.create(999, b"x", 0o644, 0, 0),
            Err(FsError::NotFound)
        ));
        assert!(matches!(
            fs.create(ino, b"x", 0o644, 0, 0),
            Err(FsError::NotDir)
        ));
        assert!(matches!(fs.lookup(ROOT_INO, b"nope").unwrap(), None));
        assert!(matches!(
            fs.unlink(ROOT_INO, b"nope"),
            Err(FsError::NotFound)
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn readdir_lists_names() {
        let (mut fs, path) = test_fs(512);
        for n in [b"zz".as_slice(), b"aa", b"mm"] {
            fs.create(ROOT_INO, n, 0o644, 0, 0).unwrap();
        }
        let names: Vec<Vec<u8>> = fs
            .readdir(ROOT_INO)
            .unwrap()
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(names, vec![b"aa".to_vec(), b"mm".to_vec(), b"zz".to_vec()]);
        std::fs::remove_file(&path).unwrap();
    }

    // -- crash-consistency tests -----------------------------------------

    use crate::block::{BlockDevice, FileDevice};
    use crate::superblock::SLOT_BLOCKS;

    fn read_slot_block(path: &std::path::Path, slot: usize) -> [u8; BLOCK_SIZE] {
        let dev = FileDevice::open(path).unwrap();
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(SLOT_BLOCKS[slot], &mut blk).unwrap();
        blk
    }

    fn write_slot_block(path: &std::path::Path, slot: usize, blk: &[u8; BLOCK_SIZE]) {
        let mut dev = FileDevice::open(path).unwrap();
        dev.write_block(SLOT_BLOCKS[slot], blk).unwrap();
        dev.sync().unwrap();
    }

    /// Ping-pong fallback in both directions: after a commit, corrupting the
    /// slot that holds the new generation falls back to the older one, and
    /// corrupting the older slot keeps the new generation.
    #[test]
    fn slot_corruption_falls_back() {
        let (mut fs, path) = test_fs(512);
        let f1 = fs.create(ROOT_INO, b"f1", 0o644, 0, 0).unwrap();
        fs.write(f1, 0, b"gen2").unwrap();
        fs.commit().unwrap();
        let f2 = fs.create(ROOT_INO, b"f2", 0o644, 0, 0).unwrap();
        fs.write(f2, 0, b"gen3").unwrap();
        fs.commit().unwrap();
        assert_eq!(fs.generation(), 3);
        drop(fs);

        let gen_of = |slot: usize| {
            let dev = FileDevice::open(&path).unwrap();
            crate::superblock::read_slot(&dev, slot).map(|sb| sb.generation)
        };
        let g0 = gen_of(0).unwrap();
        let g1 = gen_of(1).unwrap();
        assert!(
            g0 == 3 || g1 == 3,
            "one slot must hold gen 3, got {g0}/{g1}"
        );
        let (newer, older) = if g0 > g1 { (0, 1) } else { (1, 0) };
        let good_newer = read_slot_block(&path, newer);
        let good_older = read_slot_block(&path, older);

        // Corrupt the newer slot -> fall back to the older generation.
        let mut bad = good_newer;
        bad[16] ^= 0xff;
        bad[100] ^= 0x01;
        write_slot_block(&path, newer, &bad);
        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.generation(), 2);
        assert_eq!(fs2.read(f1, 0, 10).unwrap(), b"gen2");
        assert!(fs2.lookup(ROOT_INO, b"f2").unwrap().is_none());
        drop(fs2);

        // Restore the newer slot, corrupt the older -> new generation wins.
        write_slot_block(&path, newer, &good_newer);
        let mut bad_old = good_older;
        bad_old[16] ^= 0xff;
        write_slot_block(&path, older, &bad_old);
        let fs3 = Fs::open(&path).unwrap();
        assert_eq!(fs3.generation(), 3);
        assert_eq!(fs3.read(f2, 0, 10).unwrap(), b"gen3");
        drop(fs3);

        std::fs::remove_file(&path).unwrap();
    }

    /// If the newest slot is corrupt, open falls back to the older
    /// generation with its data fully intact.
    #[test]
    fn crash_new_slot_corrupt_falls_back() {
        let (mut fs, path) = test_fs(512);
        let f1 = fs.create(ROOT_INO, b"f1", 0o644, 0, 0).unwrap();
        fs.write(f1, 0, b"stable").unwrap();
        fs.commit().unwrap();
        let f2 = fs.create(ROOT_INO, b"f2", 0o644, 0, 0).unwrap();
        fs.write(f2, 0, b"doomed").unwrap();
        fs.commit().unwrap();
        assert_eq!(fs.generation(), 3);
        drop(fs);

        // Corrupt both slots' headers (bit flips in the generation field).
        for slot in 0..2 {
            let mut blk = read_slot_block(&path, slot);
            blk[16] ^= 0xff;
            blk[100] ^= 0x01;
            write_slot_block(&path, slot, &blk);
        }
        // Both slots corrupt -> open must fail loudly, not silently.
        assert!(Fs::open(&path).is_err());

        std::fs::remove_file(&path).unwrap();
    }

    /// Randomized crash test: random file ops with random commits and
    /// simulated crashes (drop without commit). After every crash the
    /// filesystem must match the last committed model exactly.
    #[test]
    fn crash_random_ops() {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_F491_4F6C_DD1D)
            }
        }

        let (mut fs, path) = test_fs(4096);
        let mut rng = Rng(0xC0FFEE);
        let mut committed: std::collections::BTreeMap<Vec<u8>, Vec<u8>> =
            std::collections::BTreeMap::new();
        let mut working = committed.clone();
        // name -> ino for files existing in `working`
        let mut inos: std::collections::HashMap<Vec<u8>, u64> = Default::default();

        let check = |fs: &Fs, model: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>| {
            let mut names: Vec<Vec<u8>> = fs
                .readdir(ROOT_INO)
                .unwrap()
                .into_iter()
                .map(|(n, _, _)| n)
                .collect();
            names.sort();
            let mut want: Vec<Vec<u8>> = model.keys().cloned().collect();
            want.sort();
            assert_eq!(names, want, "directory listing diverged");
            for (name, data) in model {
                let (ino, _) = fs.lookup(ROOT_INO, name).unwrap().unwrap();
                assert_eq!(
                    &fs.read(ino, 0, 1 << 20).unwrap(),
                    data,
                    "data diverged for {name:?}"
                );
            }
        };

        for step in 0..400 {
            let name = format!("f{}", rng.next() % 40).into_bytes();
            match rng.next() % 5 {
                0 => {
                    // create or overwrite
                    let len = (rng.next() % 5000) as usize;
                    let mut data = vec![0u8; len];
                    for (i, b) in data.iter_mut().enumerate() {
                        *b = (rng.next() >> (i % 8)) as u8;
                    }
                    let ino = match inos.get(&name) {
                        Some(&ino) => ino,
                        None => {
                            let ino = fs.create(ROOT_INO, &name, 0o644, 0, 0).unwrap();
                            inos.insert(name.clone(), ino);
                            ino
                        }
                    };
                    // random overwrite at random offset (may extend)
                    let off = if data.is_empty() {
                        0
                    } else {
                        rng.next() % (data.len() as u64 + 64)
                    };
                    fs.write(ino, off, &data).unwrap();
                    let entry = working.entry(name.clone()).or_default();
                    let end = off as usize + data.len();
                    if entry.len() < end {
                        entry.resize(end, 0);
                    }
                    entry[off as usize..end].copy_from_slice(&data);
                }
                1 => {
                    // truncate
                    if let Some(&ino) = inos.get(&name) {
                        let cur = working.get(&name).map(|v| v.len()).unwrap_or(0);
                        let new_len = (rng.next() % (cur as u64 + 100)) as usize;
                        fs.truncate(ino, new_len as u64).unwrap();
                        let e = working.get_mut(&name).unwrap();
                        e.resize(new_len, 0);
                    }
                }
                2 => {
                    // unlink
                    if inos.remove(&name).is_some() {
                        fs.unlink(ROOT_INO, &name).unwrap();
                        working.remove(&name);
                    }
                }
                _ => {
                    // read/verify a random file against `working`
                    if let Some(want) = working.get(&name) {
                        let (ino, _) = fs.lookup(ROOT_INO, &name).unwrap().unwrap();
                        assert_eq!(
                            &fs.read(ino, 0, 1 << 20).unwrap(),
                            want,
                            "uncommitted read diverged at step {step}"
                        );
                    }
                }
            }
            match rng.next() % 10 {
                0..=5 => {
                    fs.commit().unwrap();
                    committed = working.clone();
                }
                6..=7 => {
                    // crash: drop without committing
                    drop(fs);
                    fs = Fs::open(&path).unwrap();
                    working = committed.clone();
                    inos.clear();
                    for (n, _, _) in fs.readdir(ROOT_INO).unwrap() {
                        let (ino, _) = fs.lookup(ROOT_INO, &n).unwrap().unwrap();
                        inos.insert(n, ino);
                    }
                    check(&fs, &committed);
                }
                _ => {}
            }
            if step % 50 == 0 {
                check(&fs, &working);
            }
        }
        fs.commit().unwrap();
        check(&fs, &working);
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        check(&fs2, &working);
        std::fs::remove_file(&path).unwrap();
    }

    /// Filling the device returns NoSpace; the fs stays consistent and
    /// previously committed data remains readable.
    #[test]
    fn no_space_stays_consistent() {
        let (mut fs, path) = test_fs(256);
        // "precious" lives in its own file so the fill below can't clobber it.
        let keep = fs.create(ROOT_INO, b"keep", 0o644, 0, 0).unwrap();
        fs.write(keep, 0, b"precious").unwrap();
        fs.commit().unwrap();

        let ino = fs.create(ROOT_INO, b"fill", 0o644, 0, 0).unwrap();

        // Write 1 MiB chunks until NoSpace.
        let chunk = vec![0xABu8; 1 << 20];
        let mut off = 0u64;
        let mut hit_no_space = false;
        for _ in 0..32 {
            match fs.write(ino, off, &chunk) {
                Ok(()) => off += chunk.len() as u64,
                Err(FsError::NoSpace) | Err(FsError::Store(_)) => {
                    hit_no_space = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(hit_no_space, "expected to fill the 256-block image");
        // Commit still works after the failed write; old data intact.
        fs.commit().unwrap();
        drop(fs);
        let fs2 = Fs::open(&path).unwrap();
        assert_eq!(fs2.read(keep, 0, 8).unwrap(), b"precious");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn check_passes_on_healthy_fs() {
        let (mut fs, path) = test_fs(512);
        let ino = fs.create(ROOT_INO, b"a", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, b"data").unwrap();
        fs.write(ino, 5000, b"more").unwrap(); // second extent
        fs.mkdir(ROOT_INO, b"sub", 0o755, 0, 0).unwrap();
        fs.commit().unwrap();
        let rep = fs.check().unwrap();
        assert!(rep.meta_blocks > 0, "expected metadata blocks");
        assert_eq!(rep.data_blocks, 2);
        assert!(rep.allocated_blocks >= rep.meta_blocks + rep.data_blocks);
        drop(fs);
        // Still clean after a reopen.
        let fs2 = Fs::open(&path).unwrap();
        fs2.check().unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn check_catches_stray_allocated_bit() {
        use crate::block::{BlockDevice, FileDevice};
        use crate::superblock;
        use crate::BLOCK_SIZE;

        let (mut fs, path) = test_fs(512);
        let ino = fs.create(ROOT_INO, b"a", 0o644, 0, 0).unwrap();
        fs.write(ino, 0, b"data").unwrap();
        fs.commit().unwrap();
        drop(fs);

        // Mark block 500 allocated in the active bitmap area; nothing
        // references it.
        let mut dev = FileDevice::open(&path).unwrap();
        let (sb, _) = superblock::open(&dev).unwrap();
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(sb.bitmap_area_start(), &mut blk).unwrap();
        blk[500 / 8] |= 1 << (500 % 8);
        dev.write_block(sb.bitmap_area_start(), &blk).unwrap();
        dev.sync().unwrap();
        drop(dev);

        let fs2 = Fs::open(&path).unwrap();
        let err = fs2.check().unwrap_err();
        assert!(
            format!("{err}").contains("unreachable"),
            "unexpected error: {err}"
        );
        std::fs::remove_file(&path).unwrap();
    }
}
