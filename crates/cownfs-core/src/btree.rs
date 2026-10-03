//! Copy-on-write B-tree generic over its node storage.
//!
//! The tree algorithm is storage-agnostic: [`NodeStore`] abstracts how nodes
//! are addressed and persisted. [`MemArena`] is the pure in-memory store used
//! for algorithm validation; the block-backed store (`store::BlockArena`)
//! lets the *same* tested algorithm run directly on 4 KiB disk blocks, with
//! child links as `(block, generation)` [`NodeId`]s.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use crate::store::StoreError;

/// Generational node reference.
///
/// For [`MemArena`], `idx` is a slot index; for `BlockArena`, `idx` is a
/// block number. `gen` must match the slot/block generation: a stale id
/// panics (debug) instead of silently corrupting — the in-memory equivalent
/// of a checksum failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    pub idx: u64,
    pub gen: u32,
}

/// Storage backend for B-tree nodes.
///
/// Reads take `&mut self`: a block-backed store may need to fill its cache
/// (i.e. do I/O) on a miss, so reads are honestly mutable. Fallible because
/// block I/O, checksums, and space exhaustion are real errors.
pub trait NodeStore<K, V> {
    /// Fetch a node. Panics on a stale id (programmer error).
    fn get(&mut self, id: NodeId) -> Result<&Node<K, V>, StoreError>;
    /// Fetch a node for mutation. Marks it dirty in block-backed stores.
    fn get_mut(&mut self, id: NodeId) -> Result<&mut Node<K, V>, StoreError>;
    /// Whether the node's block belongs to the last committed generation.
    /// Committed nodes must never be rewritten in place: mutating them
    /// requires a copy-on-write. Always false for in-memory stores.
    fn is_committed(&mut self, id: NodeId) -> Result<bool, StoreError>;
    /// Store a fresh node with refcount 1 and return its id.
    /// Fails with [`StoreError::NoSpace`] when the device is full.
    fn alloc(&mut self, node: Node<K, V>) -> Result<NodeId, StoreError>;
    /// Remove a node and hand back ownership. The caller must own the only
    /// reference; child references transfer to the caller (used by merge).
    fn take(&mut self, id: NodeId) -> Result<Node<K, V>, StoreError>;
    /// Increment a node's reference count.
    fn inc_ref(&mut self, id: NodeId) -> Result<(), StoreError>;
    /// Decrement a node's reference count, freeing it (and, recursively,
    /// unreachable children) when it reaches zero.
    fn dec_ref(&mut self, id: NodeId) -> Result<(), StoreError>;
    /// Persist dirty state. No-op for in-memory stores.
    fn flush(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
    /// Nodes currently allocated (for leak checks).
    fn live(&mut self) -> usize;
    /// Nodes reachable from `root` (for leak checks).
    fn reachable(&mut self, root: NodeId) -> Result<usize, StoreError>;
}

/// A B-tree node. Internal nodes interleave `children` around `keys`
/// (`children.len() == keys.len() + 1`); leaf nodes have no children.
/// Child links are [`NodeId`]s resolved by the store.
#[derive(Debug, Clone)]
pub struct Node<K, V> {
    pub keys: Vec<K>,
    pub vals: Vec<V>,
    pub children: Vec<NodeId>,
    pub refcount: u32,
}

impl<K, V> Node<K, V> {
    pub fn leaf() -> Self {
        Self {
            keys: Vec::new(),
            vals: Vec::new(),
            children: Vec::new(),
            refcount: 1,
        }
    }

    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// In-memory node store with generational ids and refcounted nodes.
pub struct MemArena<K, V> {
    slots: Vec<Slot<K, V>>,
    free: Vec<u64>,
}

struct Slot<K, V> {
    gen: u32,
    node: Option<Node<K, V>>,
}

impl<K, V> MemArena<K, V> {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn check(&self, id: NodeId) -> &Slot<K, V> {
        let slot = &self.slots[id.idx as usize];
        debug_assert_eq!(slot.gen, id.gen, "stale NodeId");
        slot
    }
}

impl<K, V> Default for MemArena<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> NodeStore<K, V> for MemArena<K, V> {
    fn get(&mut self, id: NodeId) -> Result<&Node<K, V>, StoreError> {
        Ok(self.check(id).node.as_ref().expect("dangling NodeId"))
    }

    fn get_mut(&mut self, id: NodeId) -> Result<&mut Node<K, V>, StoreError> {
        let slot = &mut self.slots[id.idx as usize];
        debug_assert_eq!(slot.gen, id.gen, "stale NodeId");
        Ok(slot.node.as_mut().expect("dangling NodeId"))
    }

    fn is_committed(&mut self, _id: NodeId) -> Result<bool, StoreError> {
        // In-memory trees have no generations; everything is mutable.
        Ok(false)
    }

    fn alloc(&mut self, node: Node<K, V>) -> Result<NodeId, StoreError> {
        debug_assert_eq!(node.refcount, 1);
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(slot.node.is_none());
            slot.node = Some(node);
            Ok(NodeId { idx, gen: slot.gen })
        } else {
            let idx = self.slots.len() as u64;
            self.slots.push(Slot {
                gen: 0,
                node: Some(node),
            });
            Ok(NodeId { idx, gen: 0 })
        }
    }

    fn take(&mut self, id: NodeId) -> Result<Node<K, V>, StoreError> {
        debug_assert_eq!(self.get(id)?.refcount, 1, "take of shared node");
        let slot = &mut self.slots[id.idx as usize];
        slot.gen += 1;
        self.free.push(id.idx);
        Ok(slot.node.take().expect("dangling NodeId"))
    }

    fn inc_ref(&mut self, id: NodeId) -> Result<(), StoreError> {
        self.get_mut(id)?.refcount += 1;
        Ok(())
    }

    fn dec_ref(&mut self, id: NodeId) -> Result<(), StoreError> {
        let do_free = {
            let n = self.get_mut(id)?;
            n.refcount -= 1;
            n.refcount == 0
        };
        if do_free {
            let slot = &mut self.slots[id.idx as usize];
            slot.gen += 1;
            self.free.push(id.idx);
            let node = slot.node.take().expect("dangling NodeId");
            for c in node.children {
                self.dec_ref(c)?;
            }
        }
        Ok(())
    }

    fn live(&mut self) -> usize {
        self.slots.iter().filter(|s| s.node.is_some()).count()
    }

    fn reachable(&mut self, root: NodeId) -> Result<usize, StoreError> {
        let mut count = 0;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = self.get(id)?;
            count += 1;
            stack.extend(n.children.iter().copied());
        }
        Ok(count)
    }
}

/// Returns an id for a node the caller may mutate: the node itself if its
/// refcount is 1, otherwise a fresh copy sharing children (their refcounts
/// bumped) with the old node dereferenced.
fn ensure_unique<K: Clone, V: Clone, S: NodeStore<K, V>>(
    a: &mut S,
    id: NodeId,
) -> Result<NodeId, StoreError> {
    // A node is mutated in place only when it is uniquely owned *and* was
    // allocated in the current (uncommitted) transaction. Committed nodes
    // are immutable: rewriting one would corrupt the fallback generation.
    if !a.is_committed(id)? && a.get(id)?.refcount == 1 {
        return Ok(id);
    }
    let node = {
        let src = a.get(id)?;
        Node {
            keys: src.keys.clone(),
            vals: src.vals.clone(),
            children: src.children.clone(),
            refcount: 1,
        }
    };
    let new_id = a.alloc(node)?;
    let children = a.get(new_id)?.children.clone();
    for c in children {
        a.inc_ref(c)?;
    }
    a.dec_ref(id)?;
    Ok(new_id)
}

/// A copy-on-write B-tree. Clone-on-write snapshots share structure with the
/// original; dropping a tree releases its references.
///
/// `T` is the minimum degree (nodes hold up to `2T-1` keys); `S` is the node
/// storage backend.
pub struct BTree<K, V, S: NodeStore<K, V>, const T: usize> {
    store: Arc<Mutex<S>>,
    root: NodeId,
    len: usize,
    /// Whether this handle owns a reference on `root` (i.e. it allocated
    /// the root or incremented its refcount). `open` handles borrow the
    /// root from elsewhere (e.g. the superblock) and must not release it.
    owned: bool,
    _types: std::marker::PhantomData<(K, V)>,
}

/// In-memory B-tree (the P1-tested configuration).
pub type MemBTree<K, V, const T: usize = 4> = BTree<K, V, MemArena<K, V>, T>;

impl<K, V, S: NodeStore<K, V>, const T: usize> BTree<K, V, S, T> {
    /// Create a tree from an existing store handle, root id and length
    /// (used when opening block-backed trees from disk).
    pub fn open(store: Arc<Mutex<S>>, root: NodeId, len: usize) -> Self {
        assert!(T >= 2, "minimum degree T must be >= 2");
        BTree {
            store,
            root,
            len,
            owned: false,
            _types: std::marker::PhantomData,
        }
    }

    /// Current root id (changes across CoW mutations; recorded at commit).
    pub fn root_id(&self) -> NodeId {
        self.root
    }

    /// Share an arbitrary node (used to pin snapshot roots).
    pub fn share(&self, id: NodeId) -> Result<(), StoreError> {
        self.store.lock().unwrap().inc_ref(id)
    }

    /// Release an arbitrary node, reclaiming unreachable blocks
    /// (used to drop snapshot roots).
    pub fn release(&self, id: NodeId) -> Result<(), StoreError> {
        self.store.lock().unwrap().dec_ref(id)
    }

    /// Share the store handle (used to open snapshot views).
    pub fn store_handle(&self) -> Arc<Mutex<S>> {
        Arc::clone(&self.store)
    }

    /// Nodes reachable from the current root (for seeding a reopened
    /// store's live-node count, and for leak checks).
    pub fn count_reachable(&self) -> Result<usize, StoreError> {
        self.store.lock().unwrap().reachable(self.root)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `(allocated nodes, nodes reachable from root)` — leak-check aid.
    pub fn store_stats(&self) -> Result<(usize, usize), StoreError> {
        let live = self.store.lock().unwrap().live();
        Ok((live, self.reachable_node_count()))
    }

    /// Nodes reachable from this tree's root — for per-store leak checks
    /// when several trees share one store (tests).
    pub fn reachable_node_count(&self) -> usize {
        let mut s = self.store.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if !seen.insert((id.idx, id.gen)) {
                continue;
            }
            let n = s.get(id).expect("reachable walk on a consistent tree");
            let kids = n.children.clone();
            stack.extend(kids);
        }
        seen.len()
    }

    /// Verify the tree: every node must load (the store checks checksums
    /// and generations on load), keys must be strictly increasing in
    /// traversal order, and every child link must resolve. Shared
    /// subtrees (snapshots) are verified once. Returns all reachable
    /// node ids; for block-backed stores `id.idx` is the block number.
    pub fn verify(&self) -> Result<Vec<NodeId>, StoreError>
    where
        K: Ord + Clone,
    {
        fn walk<K: Ord + Clone, V, S: NodeStore<K, V>>(
            st: &mut S,
            id: NodeId,
            ids: &mut Vec<NodeId>,
            seen: &mut std::collections::HashSet<(u64, u32)>,
            last: &mut Option<K>,
        ) -> Result<(), StoreError> {
            if !seen.insert((id.idx, id.gen)) {
                return Ok(()); // shared subtree; already verified
            }
            ids.push(id);
            // Clone to release the store borrow before recursing.
            let (keys, children, is_leaf) = {
                let n = st.get(id)?;
                (n.keys.clone(), n.children.clone(), n.is_leaf())
            };
            for (i, key) in keys.iter().enumerate() {
                if !is_leaf {
                    walk(st, children[i], ids, seen, last)?;
                }
                if let Some(prev) = last {
                    if *prev >= *key {
                        return Err(StoreError::Corrupt {
                            block: id.idx,
                            what: "key order violated",
                        });
                    }
                }
                *last = Some(key.clone());
            }
            if !is_leaf {
                walk(st, children[keys.len()], ids, seen, last)?;
            }
            Ok(())
        }

        let mut s = self.store.lock().unwrap();
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut last = None;
        walk(&mut *s, self.root, &mut ids, &mut seen, &mut last)?;
        Ok(ids)
    }

    /// Flush dirty state in block-backed stores.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.store.lock().unwrap().flush()
    }
}

impl<K, V, S: NodeStore<K, V> + Default, const T: usize> BTree<K, V, S, T> {
    pub fn new() -> Self {
        debug_assert!(T >= 2, "minimum degree must be >= 2");
        let mut store = S::default();
        let root = store.alloc(Node::leaf()).expect("alloc cannot fail here");
        Self {
            store: Arc::new(Mutex::new(store)),
            root,
            len: 0,
            owned: true,
            _types: std::marker::PhantomData,
        }
    }
    /// Create an empty tree on an *existing* shared store (tests that run
    /// many trees against one store for cross-tree leak checks).
    pub fn new_on(store: Arc<Mutex<S>>) -> Result<Self, StoreError> {
        let root = store.lock().unwrap().alloc(Node::leaf())?;
        Ok(BTree {
            store,
            root,
            len: 0,
            owned: true,
            _types: std::marker::PhantomData,
        })
    }
}

impl<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize> BTree<K, V, S, T> {
    /// Shares the root with a new tree handle (refcount + 1).
    pub fn snapshot(&self) -> Self {
        let mut a = self.store.lock().unwrap();
        a.inc_ref(self.root).expect("inc_ref cannot fail here");
        Self {
            store: Arc::clone(&self.store),
            root: self.root,
            len: self.len,
            owned: true,
            _types: std::marker::PhantomData,
        }
    }

    /// Fetch a node by id (for diff/walk utilities).
    pub fn get_node(&self, id: NodeId) -> Result<Node<K, V>, StoreError> {
        let mut store = self.store.lock().unwrap();
        let node = store.get(id)?;
        Ok(node.clone())
    }

    /// Node ids changed between two roots of the same tree.
    ///
    /// Walks both trees in lockstep. CoW guarantees that unchanged
    /// subtrees share NodeIds (same idx AND generation) and are skipped
    /// without descent, so this is O(changed nodes), not O(tree size).
    /// When a node's child structure differs (split/merge), the entire
    /// new subtree is collected.
    ///
    /// Both roots must belong to this tree's store.
    pub fn diff_roots(
        &self,
        old_root: NodeId,
        new_root: NodeId,
    ) -> Result<Vec<NodeId>, StoreError> {
        let mut changed = Vec::new();
        // (old, new); old == None means "collect entire new subtree".
        let mut stack: Vec<(Option<NodeId>, NodeId)> = vec![(Some(old_root), new_root)];
        let mut store = self.store.lock().unwrap();
        while let Some((old_opt, new_id)) = stack.pop() {
            if old_opt == Some(new_id) {
                continue; // Shared subtree — unchanged.
            }
            changed.push(new_id);
            let new_node = store.get(new_id)?.clone();
            match old_opt {
                None => {
                    // Collect entire subtree.
                    for c in new_node.children {
                        stack.push((None, c));
                    }
                }
                Some(old_id) => {
                    let old_node = store.get(old_id)?.clone();
                    if old_node.children.len() == new_node.children.len() {
                        for (o, n) in old_node.children.iter().zip(new_node.children.iter()) {
                            stack.push((Some(*o), *n));
                        }
                    } else {
                        // Structure changed — collect the whole new subtree.
                        for c in new_node.children {
                            stack.push((None, c));
                        }
                    }
                }
            }
        }
        Ok(changed)
    }

    pub fn get(&self, k: &K) -> Result<Option<V>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let mut id = self.root;
        loop {
            let n = a.get(id)?;
            match n.keys.binary_search(k) {
                Ok(pos) => return Ok(Some(n.vals[pos].clone())),
                Err(pos) => {
                    if n.is_leaf() {
                        return Ok(None);
                    }
                    id = n.children[pos];
                }
            }
        }
    }

    /// Inserts `k -> v`, returning the old value if `k` was present.
    pub fn insert(&mut self, k: K, v: V) -> Result<Option<V>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let root = ensure_unique(&mut *a, self.root)?;
        self.root = root;
        let old;
        if a.get(root)?.keys.len() == 2 * T - 1 {
            // Root is full: grow the tree. The old root's single reference
            // moves from this handle into the new root's child slot.
            let new_root = a.alloc(Node {
                keys: Vec::new(),
                vals: Vec::new(),
                children: vec![root],
                refcount: 1,
            })?;
            self.root = new_root;
            split_child::<K, V, S, T>(&mut *a, new_root, 0)?;
            old = insert_nonfull::<K, V, S, T>(&mut *a, new_root, k, v)?;
        } else {
            old = insert_nonfull::<K, V, S, T>(&mut *a, root, k, v)?;
        }
        if old.is_none() {
            self.len += 1;
        }
        Ok(old)
    }

    /// Removes `k`, returning its value if present.
    pub fn remove(&mut self, k: &K) -> Result<Option<V>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let root = ensure_unique(&mut *a, self.root)?;
        self.root = root;
        let out = remove_from::<K, V, S, T>(&mut *a, self.root, k)?;
        // Shrink: an empty internal root is replaced by its only child.
        let shrink = {
            let r = a.get(self.root)?;
            !r.is_leaf() && r.keys.is_empty()
        };
        if shrink {
            let new_root = a.get(self.root)?.children[0];
            a.inc_ref(new_root)?;
            a.dec_ref(self.root)?; // frees old root, balancing the inc above
            self.root = new_root;
        }
        if out.is_some() {
            self.len -= 1;
        }
        Ok(out)
    }

    /// Sorted contents, for tests and scans.
    pub fn to_sorted_vec(&self) -> Result<Vec<(K, V)>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let mut out = Vec::with_capacity(self.len);
        collect(&mut *a, self.root, &mut out)?;
        Ok(out)
    }

    /// Sorted contents within `[lo, hi]`, for scans (e.g. readdir).
    pub fn range(&self, lo: &K, hi: &K) -> Result<Vec<(K, V)>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let mut out = Vec::new();
        collect_range(&mut *a, self.root, lo, hi, &mut out, usize::MAX)?;
        Ok(out)
    }

    /// Range with a limit on the number of entries returned.
    /// For C2: cursor-based readdir without materializing huge dirs.
    pub fn range_limit(&self, lo: &K, hi: &K, limit: usize) -> Result<Vec<(K, V)>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let mut out = Vec::new();
        collect_range(&mut *a, self.root, lo, hi, &mut out, limit)?;
        Ok(out)
    }

    /// Largest key, if any.
    pub fn max_key(&self) -> Result<Option<K>, StoreError> {
        let mut a = self.store.lock().unwrap();
        let mut id = self.root;
        loop {
            let n = a.get(id)?;
            if n.is_leaf() {
                return Ok(n.keys.last().cloned());
            }
            id = *n.children.last().expect("internal node has children");
        }
    }
}

impl<K, V, S: NodeStore<K, V>, const T: usize> Drop for BTree<K, V, S, T> {
    fn drop(&mut self) {
        // Only handles that own a reference on the root release it. `open`
        // handles borrow their root (e.g. from the superblock); the
        // store's refcounts describe on-disk generations, not handle
        // lifetimes. (A previous strong_count heuristic misfired during
        // unwinding and on borrowed roots — hence the explicit flag.)
        if self.owned {
            let _ = self.store.lock().unwrap().dec_ref(self.root);
        }
    }
}

fn collect<K: Clone, V: Clone, S: NodeStore<K, V>>(
    a: &mut S,
    id: NodeId,
    out: &mut Vec<(K, V)>,
) -> Result<(), StoreError> {
    let n = a.get(id)?;
    let keys = n.keys.clone();
    let vals = n.vals.clone();
    let children = n.children.clone();
    let leaf = n.is_leaf();
    for i in 0..keys.len() {
        if !leaf {
            collect(a, children[i], out)?;
        }
        out.push((keys[i].clone(), vals[i].clone()));
    }
    if !leaf {
        collect(
            a,
            *children.last().expect("internal node has children"),
            out,
        )?;
    }
    Ok(())
}

fn collect_range<K: Ord + Clone, V: Clone, S: NodeStore<K, V>>(
    a: &mut S,
    id: NodeId,
    lo: &K,
    hi: &K,
    out: &mut Vec<(K, V)>,
    limit: usize,
) -> Result<(), StoreError> {
    if out.len() >= limit {
        return Ok(());
    }
    let n = a.get(id)?;
    let keys = n.keys.clone();
    let vals = n.vals.clone();
    let children = n.children.clone();
    let leaf = n.is_leaf();
    let start = keys.partition_point(|k| k < lo);
    let end = keys.partition_point(|k| k <= hi);
    if !leaf {
        collect_range(a, children[start], lo, hi, out, limit)?;
    }
    for i in start..end {
        if out.len() >= limit {
            break;
        }
        out.push((keys[i].clone(), vals[i].clone()));
        if !leaf {
            collect_range(a, children[i + 1], lo, hi, out, limit)?;
        }
    }
    Ok(())
}

/// Splits the full child `parent.children[i]`. Parent must be unique.
fn split_child<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    parent: NodeId,
    i: usize,
) -> Result<(), StoreError> {
    let child_id = {
        let c = a.get(parent)?.children[i];
        ensure_unique(a, c)?
    };
    a.get_mut(parent)?.children[i] = child_id;

    let leaf = a.get(child_id)?.is_leaf();
    let mut z_keys = Vec::with_capacity(T - 1);
    let mut z_vals = Vec::with_capacity(T - 1);
    let mut z_children = Vec::new();
    {
        let child = a.get_mut(child_id)?;
        z_keys.extend(child.keys.drain(T..));
        z_vals.extend(child.vals.drain(T..));
        if !leaf {
            z_children.extend(child.children.drain(T..));
        }
    }
    let (mk, mv) = {
        let child = a.get_mut(child_id)?;
        (
            child.keys.pop().expect("full child has median"),
            child.vals.pop().expect("full child has median"),
        )
    };
    let z_id = a.alloc(Node {
        keys: z_keys,
        vals: z_vals,
        children: z_children,
        refcount: 1,
    })?;
    let p = a.get_mut(parent)?;
    p.keys.insert(i, mk);
    p.vals.insert(i, mv);
    p.children.insert(i + 1, z_id);
    Ok(())
}

/// Inserts into a non-full, unique node. Returns the old value on replace.
fn insert_nonfull<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    k: K,
    v: V,
) -> Result<Option<V>, StoreError> {
    let pos = a.get(node)?.keys.partition_point(|ek| ek < &k);
    if a.get(node)?.is_leaf() {
        let n = a.get_mut(node)?;
        if pos < n.keys.len() && n.keys[pos] == k {
            return Ok(Some(std::mem::replace(&mut n.vals[pos], v)));
        }
        n.keys.insert(pos, k);
        n.vals.insert(pos, v);
        return Ok(None);
    }
    if pos < a.get(node)?.keys.len() && a.get(node)?.keys[pos] == k {
        let n = a.get_mut(node)?;
        return Ok(Some(std::mem::replace(&mut n.vals[pos], v)));
    }
    let mut child_pos = pos;
    let mut child = {
        let c = a.get(node)?.children[pos];
        ensure_unique(a, c)?
    };
    a.get_mut(node)?.children[pos] = child;
    if a.get(child)?.keys.len() == 2 * T - 1 {
        split_child::<K, V, S, T>(a, node, pos)?;
        // The median moved up to node.keys[pos]; decide where k goes.
        // (Bind first: the match scrutinee borrow must end before mutation.)
        let ord = {
            let n = a.get(node)?;
            k.cmp(&n.keys[pos])
        };
        match ord {
            Ordering::Equal => {
                let n = a.get_mut(node)?;
                return Ok(Some(std::mem::replace(&mut n.vals[pos], v)));
            }
            Ordering::Greater => child_pos = pos + 1,
            Ordering::Less => {}
        }
        // Both split products are unique; no ensure needed.
        child = a.get(node)?.children[child_pos];
    }
    insert_nonfull::<K, V, S, T>(a, child, k, v)
}

fn max_key<K: Clone, V, S: NodeStore<K, V>>(a: &mut S, mut id: NodeId) -> Result<K, StoreError> {
    loop {
        let n = a.get(id)?;
        if n.is_leaf() {
            return Ok(n.keys.last().expect("non-empty subtree").clone());
        }
        id = *n.children.last().expect("internal node has children");
    }
}

fn min_key<K: Clone, V, S: NodeStore<K, V>>(a: &mut S, mut id: NodeId) -> Result<K, StoreError> {
    loop {
        let n = a.get(id)?;
        if n.is_leaf() {
            return Ok(n.keys.first().expect("non-empty subtree").clone());
        }
        id = n.children[0];
    }
}

/// Merges `node.children[i]`, `node.keys[i]`, `node.children[i+1]` into
/// `children[i]`'s slot. Node must be unique; children are made unique first.
fn merge_children<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    i: usize,
) -> Result<(), StoreError> {
    let left_id = {
        let c = a.get(node)?.children[i];
        ensure_unique(a, c)?
    };
    let right_id = {
        let c = a.get(node)?.children[i + 1];
        ensure_unique(a, c)?
    };
    {
        let n = a.get_mut(node)?;
        n.children[i] = left_id;
        n.children[i + 1] = right_id;
    }
    // take() frees right's slot; its child references transfer to left.
    let right = a.take(right_id)?;
    let (mk, mv) = {
        let n = a.get_mut(node)?;
        (n.keys.remove(i), n.vals.remove(i))
    };
    a.get_mut(node)?.children.remove(i + 1);
    let left = a.get_mut(left_id)?;
    left.keys.push(mk);
    left.vals.push(mv);
    left.keys.extend(right.keys);
    left.vals.extend(right.vals);
    left.children.extend(right.children);
    Ok(())
}

/// Moves sibling's last key through the parent to the front of
/// `node.children[pos]`. All nodes involved are unique.
fn borrow_from_left<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    pos: usize,
) -> Result<(), StoreError> {
    let sib_id = a.get(node)?.children[pos - 1];
    let child_id = a.get(node)?.children[pos];
    let (sk, sv, sc) = {
        let sib = a.get_mut(sib_id)?;
        let k = sib.keys.pop().expect("sibling has key to lend");
        let v = sib.vals.pop().expect("sibling has key to lend");
        let c = if sib.is_leaf() {
            None
        } else {
            Some(sib.children.pop().expect("sibling has child to lend"))
        };
        (k, v, c)
    };
    let (dk, dv) = {
        let n = a.get_mut(node)?;
        (
            std::mem::replace(&mut n.keys[pos - 1], sk),
            std::mem::replace(&mut n.vals[pos - 1], sv),
        )
    };
    let child = a.get_mut(child_id)?;
    child.keys.insert(0, dk);
    child.vals.insert(0, dv);
    if let Some(c) = sc {
        child.children.insert(0, c);
    }
    Ok(())
}

/// Moves sibling's first key through the parent to the end of
/// `node.children[pos]`. All nodes involved are unique.
fn borrow_from_right<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    pos: usize,
) -> Result<(), StoreError> {
    let sib_id = a.get(node)?.children[pos + 1];
    let child_id = a.get(node)?.children[pos];
    let (sk, sv, sc) = {
        let sib = a.get_mut(sib_id)?;
        let k = sib.keys.remove(0);
        let v = sib.vals.remove(0);
        let c = if sib.is_leaf() {
            None
        } else {
            Some(sib.children.remove(0))
        };
        (k, v, c)
    };
    let (dk, dv) = {
        let n = a.get_mut(node)?;
        (
            std::mem::replace(&mut n.keys[pos], sk),
            std::mem::replace(&mut n.vals[pos], sv),
        )
    };
    let child = a.get_mut(child_id)?;
    child.keys.push(dk);
    child.vals.push(dv);
    if let Some(c) = sc {
        child.children.push(c);
    }
    Ok(())
}

/// Ensures `node.children[pos]` has at least T keys, borrowing from a sibling
/// or merging. Returns the child index to descend into. Node must be unique;
/// the target child must already be unique.
fn fill_child<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    pos: usize,
) -> Result<usize, StoreError> {
    let nchildren = a.get(node)?.children.len();
    if pos > 0 {
        let sib = {
            let c = a.get(node)?.children[pos - 1];
            ensure_unique(a, c)?
        };
        a.get_mut(node)?.children[pos - 1] = sib;
        if a.get(sib)?.keys.len() >= T {
            borrow_from_left::<K, V, S, T>(a, node, pos)?;
            return Ok(pos);
        }
    }
    if pos + 1 < nchildren {
        let sib = {
            let c = a.get(node)?.children[pos + 1];
            ensure_unique(a, c)?
        };
        a.get_mut(node)?.children[pos + 1] = sib;
        if a.get(sib)?.keys.len() >= T {
            borrow_from_right::<K, V, S, T>(a, node, pos)?;
            return Ok(pos);
        }
    }
    if pos + 1 < nchildren {
        merge_children::<K, V, S, T>(a, node, pos)?;
        Ok(pos)
    } else {
        merge_children::<K, V, S, T>(a, node, pos - 1)?;
        Ok(pos - 1)
    }
}

/// Removes key at `node.keys[pos]` from an internal unique node.
fn remove_from_internal<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    pos: usize,
    k: &K,
) -> Result<Option<V>, StoreError> {
    let left = {
        let c = a.get(node)?.children[pos];
        ensure_unique(a, c)?
    };
    a.get_mut(node)?.children[pos] = left;
    if a.get(left)?.keys.len() >= T {
        let pk = max_key(a, left)?;
        let pv = remove_from::<K, V, S, T>(a, left, &pk)?.expect("predecessor exists");
        let n = a.get_mut(node)?;
        n.keys[pos] = pk;
        return Ok(Some(std::mem::replace(&mut n.vals[pos], pv)));
    }
    let right = {
        let c = a.get(node)?.children[pos + 1];
        ensure_unique(a, c)?
    };
    a.get_mut(node)?.children[pos + 1] = right;
    if a.get(right)?.keys.len() >= T {
        let sk = min_key(a, right)?;
        let sv = remove_from::<K, V, S, T>(a, right, &sk)?.expect("successor exists");
        let n = a.get_mut(node)?;
        n.keys[pos] = sk;
        return Ok(Some(std::mem::replace(&mut n.vals[pos], sv)));
    }
    merge_children::<K, V, S, T>(a, node, pos)?;
    let merged = a.get(node)?.children[pos];
    remove_from::<K, V, S, T>(a, merged, k)
}

/// Removes `k` from the subtree at unique `node`. Returns the old value.
fn remove_from<K: Ord + Clone, V: Clone, S: NodeStore<K, V>, const T: usize>(
    a: &mut S,
    node: NodeId,
    k: &K,
) -> Result<Option<V>, StoreError> {
    let pos = a.get(node)?.keys.partition_point(|ek| ek < k);
    let found = pos < a.get(node)?.keys.len() && a.get(node)?.keys[pos] == *k;
    if found {
        if a.get(node)?.is_leaf() {
            let n = a.get_mut(node)?;
            n.keys.remove(pos);
            return Ok(Some(n.vals.remove(pos)));
        }
        return remove_from_internal::<K, V, S, T>(a, node, pos, k);
    }
    if a.get(node)?.is_leaf() {
        return Ok(None);
    }
    let mut child = {
        let c = a.get(node)?.children[pos];
        ensure_unique(a, c)?
    };
    a.get_mut(node)?.children[pos] = child;
    if a.get(child)?.keys.len() == T - 1 {
        let idx = fill_child::<K, V, S, T>(a, node, pos)?;
        child = a.get(node)?.children[idx];
    }
    remove_from::<K, V, S, T>(a, child, k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn insert_and_get() {
        let mut t = MemBTree::<u64, u64, 4>::new();
        for i in 0..100u64 {
            assert_eq!(t.insert(i, i * 10).unwrap(), None);
        }
        assert_eq!(t.len(), 100);
        for i in 0..100u64 {
            assert_eq!(t.get(&i).unwrap(), Some(i * 10));
        }
        assert_eq!(t.get(&1000).unwrap(), None);
        assert_eq!(t.insert(42, 999).unwrap(), Some(420));
        assert_eq!(t.get(&42).unwrap(), Some(999));
        assert_eq!(t.len(), 100);
    }

    #[test]
    fn remove_basic() {
        let mut t = MemBTree::<u64, u64, 4>::new();
        for i in 0..50u64 {
            t.insert(i, i).unwrap();
        }
        for i in (0..50u64).step_by(2) {
            assert_eq!(t.remove(&i).unwrap(), Some(i));
        }
        assert_eq!(t.len(), 25);
        for i in 0..50u64 {
            assert_eq!(t.get(&i).unwrap(), if i % 2 == 1 { Some(i) } else { None });
        }
        assert_eq!(t.remove(&999).unwrap(), None);
    }

    #[test]
    fn snapshot_isolation() {
        let mut t = MemBTree::<u64, u64, 4>::new();
        for i in 0..20u64 {
            t.insert(i, i).unwrap();
        }
        let snap = t.snapshot();
        for i in 0..20u64 {
            t.insert(i, i + 1000).unwrap();
            t.remove(&(i + 500)).unwrap();
        }
        for i in 0..20u64 {
            assert_eq!(snap.get(&i).unwrap(), Some(i));
        }
        assert_eq!(snap.len(), 20);
        assert_eq!(t.len(), 20);
    }

    #[test]
    fn range_scan() {
        let mut t = MemBTree::<u64, u64, 4>::new();
        for i in 0..100u64 {
            t.insert(i, i * 2).unwrap();
        }
        let r = t.range(&20, &29).unwrap();
        assert_eq!(r.len(), 10);
        assert_eq!(r[0], (20, 40));
        assert_eq!(r[9], (29, 58));
        assert!(t.range(&200, &300).unwrap().is_empty());
    }

    /// Deterministic xorshift64 PRNG (no external crates in core).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn run_model<S, F, const TT: usize>(seed: u64, ops: usize, mk: &F)
    where
        S: NodeStore<u64, u64>,
        F: Fn() -> S,
    {
        let store = Arc::new(Mutex::new(mk()));
        let root = store.lock().unwrap().alloc(Node::leaf()).unwrap();
        let mut t = BTree::<u64, u64, S, TT>::open(store, root, 0);
        let mut m = BTreeMap::<u64, u64>::new();
        let mut snaps: Vec<BTree<u64, u64, S, TT>> = Vec::new();
        let mut snap_models: Vec<BTreeMap<u64, u64>> = Vec::new();
        let mut rng = Rng(seed);

        for _ in 0..ops {
            match rng.next() % 10 {
                0..=4 => {
                    let k = rng.next() % 200;
                    let v = rng.next() % 1000;
                    assert_eq!(t.insert(k, v).unwrap(), m.insert(k, v));
                }
                5..=7 => {
                    let k = rng.next() % 200;
                    assert_eq!(t.remove(&k).unwrap(), m.remove(&k));
                }
                8 => {
                    snaps.push(t.snapshot());
                    snap_models.push(m.clone());
                    if snaps.len() > 4 {
                        snaps.remove(0);
                        snap_models.remove(0);
                    }
                }
                _ => {
                    let k = rng.next() % 200;
                    assert_eq!(t.get(&k).unwrap(), m.get(&k).copied());
                }
            }
            if !snaps.is_empty() && rng.next() % 25 == 0 {
                let i = (rng.next() as usize) % snaps.len();
                let got: BTreeMap<u64, u64> =
                    snaps[i].to_sorted_vec().unwrap().into_iter().collect();
                assert_eq!(got, snap_models[i]);
            }
        }
        let got: BTreeMap<u64, u64> = t.to_sorted_vec().unwrap().into_iter().collect();
        assert_eq!(got, m);
        assert_eq!(t.len(), m.len());
        for (s, sm) in snaps.iter().zip(snap_models.iter()) {
            let g: BTreeMap<u64, u64> = s.to_sorted_vec().unwrap().into_iter().collect();
            assert_eq!(&g, sm);
        }
        // Leak check: drop snapshots first; everything allocated must then
        // be reachable from the one live root.
        drop(snaps);
        let (live, reach) = t.store_stats().unwrap();
        assert_eq!(live, reach, "leak: live={live} reachable={reach}");
        drop(t);
    }

    #[test]
    fn model_mem_t4() {
        run_model::<MemArena<u64, u64>, _, 4>(0x1234_5678, 60_000, &MemArena::new);
    }

    #[test]
    fn model_mem_t2() {
        run_model::<MemArena<u64, u64>, _, 2>(0x9e37_79b9, 75_000, &MemArena::new);
    }

    #[test]
    fn ascending_descending_stress() {
        let mut t = MemBTree::<u64, u64, 4>::new();
        for i in 0..10_000u64 {
            t.insert(i, i).unwrap();
        }
        assert_eq!(t.len(), 10_000);
        for i in (0..10_000u64).rev() {
            assert_eq!(t.remove(&i).unwrap(), Some(i));
        }
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        assert_eq!(t.to_sorted_vec().unwrap(), vec![]);
        let (live, reach) = t.store_stats().unwrap();
        assert_eq!((live, reach), (1, 1)); // just the empty root
    }
}
