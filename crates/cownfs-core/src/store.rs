//! Block-backed node storage for the CoW B-tree.
//!
//! [`BlockArena`] implements [`NodeStore`] on top of 4 KiB disk blocks:
//! nodes serialize to individual blocks with a self-checksummed header
//! carrying the refcount and a generation counter, child links are
//! `(block, generation)` [`NodeId`]s, and a write-back cache batches dirty
//! nodes for the transaction commit.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use crate::bitmap::Bitmap;
use crate::block::BlockDevice;
use crate::btree::{Node, NodeId, NodeStore};
use crate::checksum::checksum;
use crate::BLOCK_SIZE;

/// Errors from the storage layer.
#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    /// The device/bitmap has no free block.
    NoSpace,
    /// Checksum or format validation failed on `block`.
    Corrupt {
        block: u64,
        what: &'static str,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "I/O error: {e}"),
            StoreError::NoSpace => write!(f, "no space left on device"),
            StoreError::Corrupt { block, what } => {
                write!(f, "corrupt block {block}: {what}")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Fixed-size value codec for B-tree keys and values stored in node blocks.
pub trait BlockCodec: Sized {
    const SIZE: usize;
    fn encode(&self, out: &mut [u8]);
    fn decode(raw: &[u8]) -> Self;
}

impl BlockCodec for u64 {
    const SIZE: usize = 8;
    fn encode(&self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_le_bytes());
    }
    fn decode(raw: &[u8]) -> Self {
        u64::from_le_bytes(raw[..8].try_into().unwrap())
    }
}

impl BlockCodec for u32 {
    const SIZE: usize = 4;
    fn encode(&self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_le_bytes());
    }
    fn decode(raw: &[u8]) -> Self {
        u32::from_le_bytes(raw[..4].try_into().unwrap())
    }
}

impl BlockCodec for u8 {
    const SIZE: usize = 1;
    fn encode(&self, out: &mut [u8]) {
        out[0] = *self;
    }
    fn decode(raw: &[u8]) -> Self {
        raw[0]
    }
}

impl<const N: usize> BlockCodec for [u8; N] {
    const SIZE: usize = N;
    fn encode(&self, out: &mut [u8]) {
        out.copy_from_slice(self);
    }
    fn decode(raw: &[u8]) -> Self {
        raw[..N].try_into().unwrap()
    }
}

/// Device + free-space bitmap shared by every tree of a filesystem.
/// Each tree owns a [`BlockArena`] over the same `Shared`.
pub struct Shared {
    pub dev: crate::block::FileDevice,
    pub bitmap: Bitmap,
    /// Blocks freed in the current (uncommitted) transaction. Their bitmap
    /// bits stay set until commit: the blocks may still be reachable from
    /// the last committed generation, so they must not be reallocated
    /// before the new generation is durable.
    pub pending_free: Vec<u64>,
    /// Armed deterministic crash point (P7). Checked at commit boundaries.
    pub fault_point: Option<crate::engine::FaultPoint>,
}

/// Number of blocks needed to persist a bitmap of `nbits` bits starting at
/// `start` — i.e. the bitmap occupies `[start, start + blocks)`.
pub fn bitmap_blocks_for(nbits: u64) -> u64 {
    crate::bitmap::blocks_needed(nbits)
}

/// Persist the bitmap to `[start, start + blocks)`.
pub fn write_bitmap(
    dev: &mut crate::block::FileDevice,
    bitmap: &Bitmap,
    start: u64,
    blocks: u64,
) -> io::Result<()> {
    let bytes = bitmap.to_bytes();
    let mut blk = [0u8; BLOCK_SIZE];
    for i in 0..blocks {
        blk.fill(0);
        let off = i as usize * BLOCK_SIZE;
        let end = ((i + 1) as usize * BLOCK_SIZE).min(bytes.len());
        if off < bytes.len() {
            blk[..end - off].copy_from_slice(&bytes[off..end]);
        }
        dev.write_block(start + i, &blk)?;
    }
    Ok(())
}

/// Load the bitmap from `[start, start + blocks)`.
pub fn read_bitmap(
    dev: &mut crate::block::FileDevice,
    nbits: u64,
    start: u64,
    blocks: u64,
) -> io::Result<Bitmap> {
    let mut bytes = Vec::with_capacity(blocks as usize * BLOCK_SIZE);
    let mut blk = [0u8; BLOCK_SIZE];
    for i in 0..blocks {
        dev.read_block(start + i, &mut blk)?;
        bytes.extend_from_slice(&blk);
    }
    Ok(Bitmap::from_bytes(nbits, &bytes))
}

// ---------------------------------------------------------------------------
// Node block format (4096 bytes):
//   [0..2]   magic u16 = 0xB72E
//   [2]      flags: bit 0 = leaf
//   [3]      reserved
//   [4..8]   refcount u32
//   [8..12]  generation u32 (bumped every time the block is reused)
//   [12..16] key count u32
//   [16..24] checksum u64 (CRC32C over the block with this field zeroed)
//   [24..32] reserved
//   [32..]   entries:
//     leaf:     [key][val] * count
//     internal: ([child blk u64][child gen u32][pad u32][key][val]) * count
//               + [child blk u64][child gen u32][pad u32]
// ---------------------------------------------------------------------------

const NODE_MAGIC: u16 = 0xB72E;
const NODE_HDR: usize = 32;
const CHILD_SIZE: usize = 16; // blk u64 + gen u32 + pad u32

/// Maximum keys per node for the given key/value sizes.
pub fn max_keys_per_node(ksize: usize, vsize: usize) -> usize {
    (BLOCK_SIZE - NODE_HDR - CHILD_SIZE) / (ksize + vsize + CHILD_SIZE)
}

fn encode_node<K: BlockCodec, V: BlockCodec>(node: &Node<K, V>, gen: u32) -> [u8; BLOCK_SIZE] {
    let mut buf = [0u8; BLOCK_SIZE];
    buf[0..2].copy_from_slice(&NODE_MAGIC.to_le_bytes());
    buf[2] = if node.is_leaf() { 1 } else { 0 };
    buf[4..8].copy_from_slice(&node.refcount.to_le_bytes());
    buf[8..12].copy_from_slice(&gen.to_le_bytes());
    buf[12..16].copy_from_slice(&(node.keys.len() as u32).to_le_bytes());
    let mut pos = NODE_HDR;
    let put_child = |buf: &mut [u8; BLOCK_SIZE], pos: &mut usize, c: NodeId| {
        buf[*pos..*pos + 8].copy_from_slice(&c.idx.to_le_bytes());
        *pos += 8;
        buf[*pos..*pos + 4].copy_from_slice(&c.gen.to_le_bytes());
        *pos += 8; // gen + pad
    };
    if node.is_leaf() {
        for (k, v) in node.keys.iter().zip(node.vals.iter()) {
            k.encode(&mut buf[pos..pos + K::SIZE]);
            pos += K::SIZE;
            v.encode(&mut buf[pos..pos + V::SIZE]);
            pos += V::SIZE;
        }
    } else {
        for (i, (k, v)) in node.keys.iter().zip(node.vals.iter()).enumerate() {
            put_child(&mut buf, &mut pos, node.children[i]);
            k.encode(&mut buf[pos..pos + K::SIZE]);
            pos += K::SIZE;
            v.encode(&mut buf[pos..pos + V::SIZE]);
            pos += V::SIZE;
        }
        put_child(&mut buf, &mut pos, *node.children.last().unwrap());
    }
    debug_assert!(pos <= BLOCK_SIZE, "node overflows block");
    let sum = checksum(&buf);
    buf[16..24].copy_from_slice(&sum.to_le_bytes());
    buf
}

fn decode_node<K: BlockCodec, V: BlockCodec>(
    block: u64,
    buf: &[u8; BLOCK_SIZE],
) -> Result<(Node<K, V>, u32), StoreError> {
    let corrupt = |what: &'static str| StoreError::Corrupt { block, what };
    if u16::from_le_bytes([buf[0], buf[1]]) != NODE_MAGIC {
        return Err(corrupt("bad node magic"));
    }
    let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
    let mut tmp = *buf;
    tmp[16..24].fill(0);
    if checksum(&tmp) != stored {
        return Err(corrupt("node checksum mismatch"));
    }
    let leaf = buf[2] == 1;
    let refcount = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let gen = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let count = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    let mut pos = NODE_HDR;
    let mut keys = Vec::with_capacity(count);
    let mut vals = Vec::with_capacity(count);
    let mut children = Vec::new();
    let get_child = |buf: &[u8; BLOCK_SIZE], pos: &mut usize| {
        let blk = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
        let gen = u32::from_le_bytes(buf[*pos + 8..*pos + 12].try_into().unwrap());
        *pos += CHILD_SIZE;
        NodeId { idx: blk, gen }
    };
    // Bound the entry region to detect truncated garbage.
    let end_limit = BLOCK_SIZE;
    for _ in 0..count {
        if !leaf {
            if pos + CHILD_SIZE > end_limit {
                return Err(corrupt("child pointer past end of block"));
            }
            children.push(get_child(buf, &mut pos));
        }
        if pos + K::SIZE + V::SIZE > end_limit {
            return Err(corrupt("entry past end of block"));
        }
        keys.push(K::decode(&buf[pos..pos + K::SIZE]));
        pos += K::SIZE;
        vals.push(V::decode(&buf[pos..pos + V::SIZE]));
        pos += V::SIZE;
    }
    if !leaf {
        if pos + CHILD_SIZE > end_limit {
            return Err(corrupt("final child past end of block"));
        }
        children.push(get_child(buf, &mut pos));
    }
    Ok((
        Node {
            keys,
            vals,
            children,
            refcount,
        },
        gen,
    ))
}

struct CacheEntry<K, V> {
    node: Node<K, V>,
    gen: u32,
    dirty: bool,
    /// True once the entry's block belongs to the last committed
    /// generation. Frozen blocks are never rewritten in place (their
    /// keys/children are still reachable from the committed roots);
    /// mutation goes through copy-on-write. Set on disk load and by
    /// `flush` (which runs at commit).
    frozen: bool,
}

/// Block-backed [`NodeStore`]: nodes live in 4 KiB blocks, cached in memory
/// and written back on [`flush`](NodeStore::flush).
pub struct BlockArena<K, V> {
    shared: Arc<Mutex<Shared>>,
    cache: HashMap<u64, CacheEntry<K, V>>,
    /// Blocks currently allocated to this arena's nodes (leak-check aid).
    alloc_count: u64,
}

impl<K, V> BlockArena<K, V> {
    /// Create an arena over `shared`. `t` is the tree's minimum degree;
    /// panics in debug builds when `2t-1` keys cannot fit in a block.
    pub fn new(shared: Arc<Mutex<Shared>>, t: usize) -> Self
    where
        K: BlockCodec,
        V: BlockCodec,
    {
        debug_assert!(
            2 * t - 1 <= max_keys_per_node(K::SIZE, V::SIZE),
            "degree {t} does not fit: max {}",
            max_keys_per_node(K::SIZE, V::SIZE)
        );
        BlockArena {
            shared,
            cache: HashMap::new(),
            alloc_count: 0,
        }
    }

    /// Allocate a fresh empty root and return the arena in an `Arc<Mutex>`.
    pub fn new_tree(
        shared: Arc<Mutex<Shared>>,
        t: usize,
    ) -> Result<(Arc<Mutex<Self>>, NodeId), StoreError>
    where
        K: BlockCodec + Ord + Clone,
        V: BlockCodec + Clone,
    {
        let mut arena = Self::new(shared, t);
        let root = arena.alloc(Node::leaf())?;
        Ok((Arc::new(Mutex::new(arena)), root))
    }

    /// Load a node into the cache (verifying magic/checksum/generation).
    fn load(&mut self, id: NodeId) -> Result<&CacheEntry<K, V>, StoreError>
    where
        K: BlockCodec,
        V: BlockCodec,
    {
        if !self.cache.contains_key(&id.idx) {
            let mut buf = [0u8; BLOCK_SIZE];
            self.shared
                .lock()
                .unwrap()
                .dev
                .read_block(id.idx, &mut buf)?;
            let (node, gen) = decode_node::<K, V>(id.idx, &buf)?;
            // A stale id (wrong generation) means a use-after-free bug: the
            // block was recycled for another node. Always fatal, like a
            // checksum failure.
            assert_eq!(
                gen, id.gen,
                "stale NodeId {:?}: block has generation {gen}",
                id
            );
            self.cache.insert(
                id.idx,
                CacheEntry {
                    node,
                    gen,
                    dirty: false,
                    frozen: true,
                },
            );
        } else {
            // The generation check applies on cache hits too: the block may
            // have been freed and reallocated since `id` was issued, in which
            // case the cached entry belongs to a different node generation.
            let gen = self.cache[&id.idx].gen;
            assert_eq!(
                gen, id.gen,
                "stale NodeId {:?}: cached block has generation {gen}",
                id
            );
        }
        Ok(&self.cache[&id.idx])
    }

    fn load_mut(&mut self, id: NodeId) -> Result<&mut CacheEntry<K, V>, StoreError>
    where
        K: BlockCodec,
        V: BlockCodec,
    {
        self.load(id)?;
        let e = self.cache.get_mut(&id.idx).expect("just loaded");
        e.dirty = true;
        Ok(e)
    }

    fn free_block(&mut self, block: u64) {
        self.cache.remove(&block);
        // Deferred: the bit is cleared at commit, after the new bitmap
        // area is written. See `Shared::pending_free`.
        self.shared.lock().unwrap().pending_free.push(block);
        self.alloc_count -= 1;
    }
    /// Seed the live-node count after opening an existing image. The arena
    /// starts empty; the nodes already on disk must be counted (via a
    /// reachability walk) or the first `take`/`free_block` after reopen
    /// underflows the counter and panics.
    pub fn set_live(&mut self, n: usize) {
        self.alloc_count = n as u64;
    }

    /// Nodes reachable from any of `roots`, counted once (union).
    /// Snapshot records pin tree roots that are no longer reachable from
    /// the live trees (e.g. a root the live tree CoW-cloned away from
    /// after the snapshot was taken); those blocks are allocated and must
    /// be seeded, or releasing the snapshot later underflows the counter.
    pub fn reachable_multi(&mut self, roots: &[NodeId]) -> Result<usize, StoreError>
    where
        K: BlockCodec,
        V: BlockCodec,
    {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<NodeId> = roots.to_vec();
        while let Some(id) = stack.pop() {
            if !seen.insert((id.idx, id.gen)) {
                continue;
            }
            // Clone child ids to end the borrow before recursing.
            let children = self.load(id)?.node.children.clone();
            stack.extend(children);
        }
        Ok(seen.len())
    }
}

impl<K: BlockCodec, V: BlockCodec> NodeStore<K, V> for BlockArena<K, V> {
    fn get(&mut self, id: NodeId) -> Result<&Node<K, V>, StoreError> {
        Ok(&self.load(id)?.node)
    }

    fn get_mut(&mut self, id: NodeId) -> Result<&mut Node<K, V>, StoreError> {
        Ok(&mut self.load_mut(id)?.node)
    }

    fn is_committed(&mut self, id: NodeId) -> Result<bool, StoreError> {
        Ok(self.load(id)?.frozen)
    }

    fn alloc(&mut self, node: Node<K, V>) -> Result<NodeId, StoreError> {
        debug_assert_eq!(node.refcount, 1);
        let block = self
            .shared
            .lock()
            .unwrap()
            .bitmap
            .alloc()
            .ok_or(StoreError::NoSpace)?;
        // Fresh blocks start a new generation. Reused blocks keep bumping it
        // so stale in-memory ids can never alias the new node.
        let gen = {
            let mut buf = [0u8; BLOCK_SIZE];
            // Best-effort: read the previous header for its generation.
            // (A short read on a never-written block yields zeros.)
            let _ = self.shared.lock().unwrap().dev.read_block(block, &mut buf);
            if u16::from_le_bytes([buf[0], buf[1]]) == NODE_MAGIC {
                u32::from_le_bytes(buf[8..12].try_into().unwrap()).wrapping_add(1)
            } else {
                1
            }
        };
        self.cache.insert(
            block,
            CacheEntry {
                node,
                gen,
                dirty: true,
                frozen: false,
            },
        );
        self.alloc_count += 1;
        Ok(NodeId { idx: block, gen })
    }

    fn take(&mut self, id: NodeId) -> Result<Node<K, V>, StoreError> {
        let rc = self.load(id)?.node.refcount;
        debug_assert_eq!(rc, 1, "take of shared node");
        let entry = self.cache.remove(&id.idx).expect("just loaded");
        self.shared.lock().unwrap().pending_free.push(id.idx);
        self.alloc_count -= 1;
        Ok(entry.node)
    }

    fn inc_ref(&mut self, id: NodeId) -> Result<(), StoreError> {
        self.load_mut(id)?.node.refcount += 1;
        Ok(())
    }

    fn dec_ref(&mut self, id: NodeId) -> Result<(), StoreError> {
        // Iterative post-order release.
        let mut stack = vec![id];
        while let Some(nid) = stack.pop() {
            let rc = {
                let e = self.load_mut(nid)?;
                e.node.refcount -= 1;
                e.node.refcount
            };
            if rc == 0 {
                let entry = self.cache.remove(&nid.idx).expect("just loaded");
                // Queue children before freeing (their blocks are untouched).
                stack.extend(entry.node.children.iter().copied());
                self.free_block(nid.idx);
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        // Collect dirty blocks first so the device borrow is short.
        let dirty: Vec<u64> = self
            .cache
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(b, _)| *b)
            .collect();
        for block in dirty {
            let buf = {
                let e = &self.cache[&block];
                encode_node(&e.node, e.gen)
            };
            self.shared.lock().unwrap().dev.write_block(block, &buf)?;
            self.cache.get_mut(&block).expect("dirty listed").dirty = false;
        }
        // Commit point: every cached block now belongs to the new
        // committed generation and becomes immutable in place.
        for e in self.cache.values_mut() {
            e.frozen = true;
        }
        Ok(())
    }

    fn live(&mut self) -> usize {
        self.alloc_count as usize
    }

    fn reachable(&mut self, root: NodeId) -> Result<usize, StoreError> {
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !seen.insert((id.idx, id.gen)) {
                continue;
            }
            // Clone child ids to end the borrow before recursing.
            let children = self.load(id)?.node.children.clone();
            stack.extend(children);
        }
        Ok(seen.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::FileDevice;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Throwaway device + bitmap for store tests. File is removed on drop.
    pub struct TestDevice {
        pub shared: Arc<Mutex<Shared>>,
        path: std::path::PathBuf,
    }

    impl TestDevice {
        pub fn new(blocks: u64) -> Self {
            let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "cownfs-store-test-{}-{}.img",
                std::process::id(),
                n
            ));
            let mut dev = FileDevice::create(&path, blocks).unwrap();
            let mut bitmap = Bitmap::new(blocks);
            bitmap.set(0); // reserved
            write_bitmap(&mut dev, &bitmap, 1, bitmap_blocks_for(blocks)).unwrap();
            // Reserve the bitmap blocks themselves.
            for b in 1..1 + bitmap_blocks_for(blocks) {
                bitmap.set(b);
            }
            TestDevice {
                shared: Arc::new(Mutex::new(Shared {
                    dev,
                    bitmap,
                    pending_free: Vec::new(),
                    fault_point: None,
                })),
                path,
            }
        }

        pub fn arena<K: BlockCodec + Ord + Clone, V: BlockCodec + Clone>(
            &self,
            t: usize,
        ) -> Arc<Mutex<BlockArena<K, V>>> {
            Arc::new(Mutex::new(BlockArena::new(Arc::clone(&self.shared), t)))
        }
    }

    impl Drop for TestDevice {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn node_roundtrip() {
        let node = Node {
            keys: vec![7u64, 42u64],
            vals: vec![70u64, 420u64],
            children: vec![
                NodeId { idx: 11, gen: 3 },
                NodeId { idx: 12, gen: 1 },
                NodeId { idx: 13, gen: 9 },
            ],
            refcount: 2,
        };
        let buf = encode_node(&node, 5);
        let (back, gen) = decode_node::<u64, u64>(99, &buf).unwrap();
        assert_eq!(gen, 5);
        assert_eq!(back.keys, node.keys);
        assert_eq!(back.vals, node.vals);
        assert_eq!(back.children, node.children);
        assert_eq!(back.refcount, 2);

        // Corruption is detected.
        let mut bad = buf;
        bad[100] ^= 0xff;
        assert!(matches!(
            decode_node::<u64, u64>(99, &bad),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn arena_alloc_get_persists() {
        let td = TestDevice::new(256);
        let arena = td.arena::<u64, u64>(4);
        let mut a = arena.lock().unwrap();
        let id = a.alloc(Node::leaf()).unwrap();
        a.get_mut(id).unwrap().keys.push(1);
        a.get_mut(id).unwrap().vals.push(2);
        a.flush().unwrap();
        drop(a);

        // Re-read through a fresh arena over the same device: the node must
        // come back from disk with its refcount.
        let arena2 = td.arena::<u64, u64>(4);
        let mut b = arena2.lock().unwrap();
        let n = b.get(id).unwrap();
        assert_eq!(n.keys, vec![1u64]);
        assert_eq!(n.refcount, 1);
    }

    #[test]
    fn max_keys_sanity() {
        // (key size, value size, min-degree) for each engine tree.
        assert!(2 * 13 - 1 <= max_keys_per_node(8, 128)); // inodes
        assert!(2 * 7 - 1 <= max_keys_per_node(264, 16)); // dirs
        assert!(2 * 42 - 1 <= max_keys_per_node(16, 16)); // extents
        assert!(2 * 14 - 1 <= max_keys_per_node(8, 124)); // snapshots
    }
}
