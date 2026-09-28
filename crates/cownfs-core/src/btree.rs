//! Copy-on-write B-tree with refcounted nodes.
//!
//! P1 is in-memory. The design maps 1:1 onto the block-backed tree of P2:
//! replace [`NodeId`] with a block number and [`Arena`] with the block
//! cache, and the CoW/refcount logic below is unchanged.
//!
//! Design notes:
//! - Nodes live in an [`Arena`] and are referenced by generational [`NodeId`]s.
//!   A stale id (wrong generation) panics in debug builds instead of silently
//!   corrupting — the in-memory equivalent of a checksum failure.
//! - Every node carries a refcount. `insert`/`remove` clone nodes on the
//!   mutation path whose refcount > 1 ([`ensure_unique`]), decrementing the
//!   old node. This is the exact discipline P2 will apply to blocks.
//! - [`BTree::snapshot`] shares the root (refcount + 1). Dropping a tree
//!   decrements its root; nodes reaching zero are recursively freed.
//! - `T` is the B-tree minimum degree: nodes hold up to `2T-1` keys.
//!   Small `T` (2) is used in tests to stress splits and merges.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

/// Generational node reference. `gen` must match the slot's generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeId {
    idx: u32,
    gen: u32,
}

struct Node<K, V> {
    keys: Vec<K>,
    vals: Vec<V>,
    /// Empty for leaves; `keys.len() + 1` entries for internal nodes.
    children: Vec<NodeId>,
    refcount: u32,
}

impl<K, V> Node<K, V> {
    fn leaf() -> Self {
        Self {
            keys: Vec::new(),
            vals: Vec::new(),
            children: Vec::new(),
            refcount: 1,
        }
    }

    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

struct Slot<K, V> {
    gen: u32,
    node: Option<Node<K, V>>,
}

struct Arena<K, V> {
    slots: Vec<Slot<K, V>>,
    free: Vec<u32>,
}

impl<K, V> Arena<K, V> {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn get(&self, id: NodeId) -> &Node<K, V> {
        let slot = &self.slots[id.idx as usize];
        debug_assert_eq!(slot.gen, id.gen, "stale NodeId");
        slot.node.as_ref().expect("dangling NodeId")
    }

    fn get_mut(&mut self, id: NodeId) -> &mut Node<K, V> {
        let slot = &mut self.slots[id.idx as usize];
        debug_assert_eq!(slot.gen, id.gen, "stale NodeId");
        slot.node.as_mut().expect("dangling NodeId")
    }

    fn alloc(&mut self, node: Node<K, V>) -> NodeId {
        debug_assert_eq!(node.refcount, 1);
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(slot.node.is_none());
            slot.node = Some(node);
            NodeId { idx, gen: slot.gen }
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot { gen: 0, node: Some(node) });
            NodeId { idx, gen: 0 }
        }
    }

    /// Removes the node from its slot *without* touching child refcounts.
    /// The caller must own the only reference; child references are
    /// transferred to the caller. Used by merge.
    fn take(&mut self, id: NodeId) -> Node<K, V> {
        debug_assert_eq!(self.get(id).refcount, 1, "take of shared node");
        let slot = &mut self.slots[id.idx as usize];
        slot.gen += 1;
        self.free.push(id.idx);
        slot.node.take().expect("dangling NodeId")
    }

    fn inc_ref(&mut self, id: NodeId) {
        self.get_mut(id).refcount += 1;
    }

    /// Drops one reference; recursively frees the subtree at zero.
    fn dec_ref(&mut self, id: NodeId) {
        let do_free = {
            let n = self.get_mut(id);
            n.refcount -= 1;
            n.refcount == 0
        };
        if do_free {
            let slot = &mut self.slots[id.idx as usize];
            slot.gen += 1;
            self.free.push(id.idx);
            let node = slot.node.take().expect("dangling NodeId");
            for c in node.children {
                self.dec_ref(c);
            }
        }
    }

    /// Allocated slots (for leak checks).
    fn live(&self) -> usize {
        self.slots.iter().filter(|s| s.node.is_some()).count()
    }

    /// Nodes reachable from `root` (for leak checks).
    fn reachable(&self, root: NodeId) -> usize {
        let mut count = 0;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let n = self.get(id);
            count += 1;
            stack.extend(n.children.iter().copied());
        }
        count
    }
}

/// Returns an id for a node the caller may mutate: the node itself if its
/// refcount is 1, otherwise a fresh copy sharing children (their refcounts
/// bumped) with the old node dereferenced.
fn ensure_unique<K: Clone, V: Clone>(a: &mut Arena<K, V>, id: NodeId) -> NodeId {
    if a.get(id).refcount == 1 {
        return id;
    }
    let src = a.get(id);
    let node = Node {
        keys: src.keys.clone(),
        vals: src.vals.clone(),
        children: src.children.clone(),
        refcount: 1,
    };
    let new_id = a.alloc(node);
    for c in a.get(new_id).children.clone() {
        a.inc_ref(c);
    }
    a.dec_ref(id);
    new_id
}

/// A copy-on-write B-tree. Clone-on-write snapshots share structure with the
/// original; dropping a tree releases its references.
pub struct BTree<K, V, const T: usize = 4> {
    store: Rc<RefCell<Arena<K, V>>>,
    root: NodeId,
    len: usize,
}

impl<K: Ord + Clone, V: Clone, const T: usize> BTree<K, V, T> {
    pub fn new() -> Self {
        debug_assert!(T >= 2, "minimum degree must be >= 2");
        let mut arena = Arena::new();
        let root = arena.alloc(Node::leaf());
        Self {
            store: Rc::new(RefCell::new(arena)),
            root,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Shares the root with a new tree handle (refcount + 1).
    pub fn snapshot(&self) -> Self {
        let mut a = self.store.borrow_mut();
        a.inc_ref(self.root);
        Self {
            store: Rc::clone(&self.store),
            root: self.root,
            len: self.len,
        }
    }

    pub fn get(&self, k: &K) -> Option<V> {
        let a = self.store.borrow();
        let mut id = self.root;
        loop {
            let n = a.get(id);
            match n.keys.binary_search(k) {
                Ok(pos) => return Some(n.vals[pos].clone()),
                Err(pos) => {
                    if n.is_leaf() {
                        return None;
                    }
                    id = n.children[pos];
                }
            }
        }
    }

    /// Inserts `k -> v`, returning the old value if `k` was present.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let mut a = self.store.borrow_mut();
        let root = ensure_unique(&mut a, self.root);
        self.root = root;
        let old;
        if a.get(root).keys.len() == 2 * T - 1 {
            // Root is full: grow the tree. The old root's single reference
            // moves from this handle into the new root's child slot.
            let new_root = a.alloc(Node {
                keys: Vec::new(),
                vals: Vec::new(),
                children: vec![root],
                refcount: 1,
            });
            self.root = new_root;
            split_child::<K, V, T>(&mut a, new_root, 0);
            old = insert_nonfull::<K, V, T>(&mut a, new_root, k, v);
        } else {
            old = insert_nonfull::<K, V, T>(&mut a, root, k, v);
        }
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// Removes `k`, returning its value if present.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let mut a = self.store.borrow_mut();
        let root = ensure_unique(&mut a, self.root);
        self.root = root;
        let out = remove_from::<K, V, T>(&mut a, self.root, k);
        // Shrink: an empty internal root is replaced by its only child.
        let shrink = {
            let r = a.get(self.root);
            !r.is_leaf() && r.keys.is_empty()
        };
        if shrink {
            let new_root = a.get(self.root).children[0];
            a.inc_ref(new_root);
            a.dec_ref(self.root); // frees old root, balancing the inc above
            self.root = new_root;
        }
        if out.is_some() {
            self.len -= 1;
        }
        out
    }

    /// Sorted contents, for tests and debugging.
    pub fn to_sorted_vec(&self) -> Vec<(K, V)> {
        let a = self.store.borrow();
        let mut out = Vec::with_capacity(self.len);
        collect(&a, self.root, &mut out);
        out
    }

    /// `(allocated slots, nodes reachable from root)` — leak-check aid.
    pub fn debug_stats(&self) -> (usize, usize) {
        let a = self.store.borrow();
        (a.live(), a.reachable(self.root))
    }
}

impl<K, V, const T: usize> Drop for BTree<K, V, T> {
    fn drop(&mut self) {
        // Last handle: the arena is dropped with its slots, refcounts moot.
        if Rc::strong_count(&self.store) > 1 {
            self.store.borrow_mut().dec_ref(self.root);
        }
    }
}

fn collect<K: Clone, V: Clone>(a: &Arena<K, V>, id: NodeId, out: &mut Vec<(K, V)>) {
    let n = a.get(id);
    for i in 0..n.keys.len() {
        if !n.is_leaf() {
            collect(a, n.children[i], out);
        }
        out.push((n.keys[i].clone(), n.vals[i].clone()));
    }
    if !n.is_leaf() {
        collect(a, *n.children.last().unwrap(), out);
    }
}

/// Splits the full child `parent.children[i]`. Parent must be unique.
fn split_child<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    parent: NodeId,
    i: usize,
) {
    let child_id = {
        let c = a.get(parent).children[i];
        ensure_unique(a, c)
    };
    a.get_mut(parent).children[i] = child_id;

    let leaf = a.get(child_id).is_leaf();
    let mut z_keys = Vec::with_capacity(T - 1);
    let mut z_vals = Vec::with_capacity(T - 1);
    let mut z_children = Vec::new();
    {
        let child = a.get_mut(child_id);
        z_keys.extend(child.keys.drain(T..));
        z_vals.extend(child.vals.drain(T..));
        if !leaf {
            z_children.extend(child.children.drain(T..));
        }
    }
    let (mk, mv) = {
        let child = a.get_mut(child_id);
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
    });
    let p = a.get_mut(parent);
    p.keys.insert(i, mk);
    p.vals.insert(i, mv);
    p.children.insert(i + 1, z_id);
}

/// Inserts into a non-full, unique node. Returns the old value on replace.
fn insert_nonfull<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    k: K,
    v: V,
) -> Option<V> {
    let pos = a.get(node).keys.partition_point(|ek| ek < &k);
    if a.get(node).is_leaf() {
        let n = a.get_mut(node);
        if pos < n.keys.len() && n.keys[pos] == k {
            return Some(std::mem::replace(&mut n.vals[pos], v));
        }
        n.keys.insert(pos, k);
        n.vals.insert(pos, v);
        return None;
    }
    if pos < a.get(node).keys.len() && a.get(node).keys[pos] == k {
        let n = a.get_mut(node);
        return Some(std::mem::replace(&mut n.vals[pos], v));
    }
    let mut child_pos = pos;
    let mut child = {
        let c = a.get(node).children[pos];
        ensure_unique(a, c)
    };
    a.get_mut(node).children[pos] = child;
    if a.get(child).keys.len() == 2 * T - 1 {
        split_child::<K, V, T>(a, node, pos);
        // The median moved up to node.keys[pos]; decide where k goes.
        // (Bind first: the match scrutinee borrow must end before mutation.)
        let ord = {
            let n = a.get(node);
            k.cmp(&n.keys[pos])
        };
        match ord {
            Ordering::Equal => {
                let n = a.get_mut(node);
                return Some(std::mem::replace(&mut n.vals[pos], v));
            }
            Ordering::Greater => child_pos = pos + 1,
            Ordering::Less => {}
        }
        // Both split products are unique; no ensure needed.
        child = a.get(node).children[child_pos];
    }
    insert_nonfull::<K, V, T>(a, child, k, v)
}

fn max_key<K: Clone, V>(a: &Arena<K, V>, mut id: NodeId) -> K {
    loop {
        let n = a.get(id);
        if n.is_leaf() {
            return n.keys.last().expect("non-empty subtree").clone();
        }
        id = *n.children.last().unwrap();
    }
}

fn min_key<K: Clone, V>(a: &Arena<K, V>, mut id: NodeId) -> K {
    loop {
        let n = a.get(id);
        if n.is_leaf() {
            return n.keys.first().expect("non-empty subtree").clone();
        }
        id = n.children[0];
    }
}

/// Merges `node.children[i]`, `node.keys[i]`, `node.children[i+1]` into
/// `children[i]`'s slot. Node must be unique; children are made unique first.
fn merge_children<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    i: usize,
) {
    let left_id = {
        let c = a.get(node).children[i];
        ensure_unique(a, c)
    };
    let right_id = {
        let c = a.get(node).children[i + 1];
        ensure_unique(a, c)
    };
    {
        let n = a.get_mut(node);
        n.children[i] = left_id;
        n.children[i + 1] = right_id;
    }
    // take() frees right's slot; its child references transfer to left.
    let right = a.take(right_id);
    let (mk, mv) = {
        let n = a.get_mut(node);
        (n.keys.remove(i), n.vals.remove(i))
    };
    a.get_mut(node).children.remove(i + 1);
    let left = a.get_mut(left_id);
    left.keys.push(mk);
    left.vals.push(mv);
    left.keys.extend(right.keys);
    left.vals.extend(right.vals);
    left.children.extend(right.children);
}

/// Moves sibling's last key through the parent to the front of
/// `node.children[pos]`. All nodes involved are unique.
fn borrow_from_left<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    pos: usize,
) {
    let sib_id = a.get(node).children[pos - 1];
    let child_id = a.get(node).children[pos];
    let (sk, sv, sc) = {
        let sib = a.get_mut(sib_id);
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
        let n = a.get_mut(node);
        (
            std::mem::replace(&mut n.keys[pos - 1], sk),
            std::mem::replace(&mut n.vals[pos - 1], sv),
        )
    };
    let child = a.get_mut(child_id);
    child.keys.insert(0, dk);
    child.vals.insert(0, dv);
    if let Some(c) = sc {
        child.children.insert(0, c);
    }
}

/// Moves sibling's first key through the parent to the end of
/// `node.children[pos]`. All nodes involved are unique.
fn borrow_from_right<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    pos: usize,
) {
    let sib_id = a.get(node).children[pos + 1];
    let child_id = a.get(node).children[pos];
    let (sk, sv, sc) = {
        let sib = a.get_mut(sib_id);
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
        let n = a.get_mut(node);
        (
            std::mem::replace(&mut n.keys[pos], sk),
            std::mem::replace(&mut n.vals[pos], sv),
        )
    };
    let child = a.get_mut(child_id);
    child.keys.push(dk);
    child.vals.push(dv);
    if let Some(c) = sc {
        child.children.push(c);
    }
}

/// Ensures `node.children[pos]` has at least T keys, borrowing from a sibling
/// or merging. Returns the child index to descend into. Node must be unique;
/// the target child must already be unique.
fn fill_child<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    pos: usize,
) -> usize {
    let nchildren = a.get(node).children.len();
    if pos > 0 {
        let sib = {
            let c = a.get(node).children[pos - 1];
            ensure_unique(a, c)
        };
        a.get_mut(node).children[pos - 1] = sib;
        if a.get(sib).keys.len() >= T {
            borrow_from_left::<K, V, T>(a, node, pos);
            return pos;
        }
    }
    if pos + 1 < nchildren {
        let sib = {
            let c = a.get(node).children[pos + 1];
            ensure_unique(a, c)
        };
        a.get_mut(node).children[pos + 1] = sib;
        if a.get(sib).keys.len() >= T {
            borrow_from_right::<K, V, T>(a, node, pos);
            return pos;
        }
    }
    if pos + 1 < nchildren {
        merge_children::<K, V, T>(a, node, pos);
        pos
    } else {
        merge_children::<K, V, T>(a, node, pos - 1);
        pos - 1
    }
}

/// Removes key at `node.keys[pos]` from an internal unique node.
fn remove_from_internal<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    pos: usize,
    k: &K,
) -> Option<V> {
    let left = {
        let c = a.get(node).children[pos];
        ensure_unique(a, c)
    };
    a.get_mut(node).children[pos] = left;
    if a.get(left).keys.len() >= T {
        let pk = max_key(a, left);
        let pv = remove_from::<K, V, T>(a, left, &pk).expect("predecessor exists");
        let n = a.get_mut(node);
        n.keys[pos] = pk;
        return Some(std::mem::replace(&mut n.vals[pos], pv));
    }
    let right = {
        let c = a.get(node).children[pos + 1];
        ensure_unique(a, c)
    };
    a.get_mut(node).children[pos + 1] = right;
    if a.get(right).keys.len() >= T {
        let sk = min_key(a, right);
        let sv = remove_from::<K, V, T>(a, right, &sk).expect("successor exists");
        let n = a.get_mut(node);
        n.keys[pos] = sk;
        return Some(std::mem::replace(&mut n.vals[pos], sv));
    }
    merge_children::<K, V, T>(a, node, pos);
    let merged = a.get(node).children[pos];
    remove_from::<K, V, T>(a, merged, k)
}

/// Removes `k` from the subtree at unique `node`. Returns the old value.
fn remove_from<K: Ord + Clone, V: Clone, const T: usize>(
    a: &mut Arena<K, V>,
    node: NodeId,
    k: &K,
) -> Option<V> {
    let pos = a.get(node).keys.partition_point(|ek| ek < k);
    let found = pos < a.get(node).keys.len() && a.get(node).keys[pos] == *k;
    if found {
        if a.get(node).is_leaf() {
            let n = a.get_mut(node);
            n.keys.remove(pos);
            return Some(n.vals.remove(pos));
        }
        return remove_from_internal::<K, V, T>(a, node, pos, k);
    }
    if a.get(node).is_leaf() {
        return None;
    }
    let mut child = {
        let c = a.get(node).children[pos];
        ensure_unique(a, c)
    };
    a.get_mut(node).children[pos] = child;
    if a.get(child).keys.len() == T - 1 {
        let idx = fill_child::<K, V, T>(a, node, pos);
        child = a.get(node).children[idx];
    }
    remove_from::<K, V, T>(a, child, k)
}
