//! T2: proptest stateful model test for Fs.
//!
//! Generates random operation sequences (create/mkdir/symlink/hardlink,
//! write/truncate, unlink/rmdir/rename, xattrs, quotas, chown, snapshots,
//! commit, reopen) and applies each op to both the real `Fs` and an
//! in-memory model. Before applying, the model predicts the exact outcome
//! (Ok or the `FsError` discriminant); any divergence fails the test and
//! proptest shrinks the sequence to a minimal repro.
//!
//! After every op the full observable state is cross-checked: namespace,
//! file bytes, symlink targets, xattrs, quota usage, and snapshot contents.
//!
//! Tiers: `T2_CASES` env var selects the case count (default 256 = PR tier;
//! nightly uses 100000). `T2_MAX_OPS` caps ops per case (default 60).

use cownfs_core::engine::{Fs, FsError, SetAttrs, FTYPE_DIR, FTYPE_FILE, FTYPE_SYMLINK, ROOT_INO};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

const BLK: u64 = 4096;

/// Blocks charged to quota for a file of `size` bytes (mirrors the engine).
fn blocks_for_size(s: u64) -> u64 {
    s.div_ceil(BLK) + 1
}

/// Stable discriminant name for an FsError (messages are not compared).
fn disc(e: &FsError) -> &'static str {
    match e {
        FsError::Store(_) => "Store",
        FsError::NotFound => "NotFound",
        FsError::AlreadyExists => "AlreadyExists",
        FsError::NotDir => "NotDir",
        FsError::NotFile => "NotFile",
        FsError::NotEmpty => "NotEmpty",
        FsError::BadName => "BadName",
        FsError::NoSpace => "NoSpace",
        FsError::Invalid(_) => "Invalid",
        FsError::Corrupt(_) => "Corrupt",
        FsError::InjectedFault(_) => "InjectedFault",
        FsError::QuotaExceeded => "QuotaExceeded",
        FsError::BitmapCorrupt => "BitmapCorrupt",
        FsError::Fenced => "Fenced",
        FsError::IncompatibleFeature(_) => "IncompatibleFeature",
        FsError::ReadOnly => "ReadOnly",
    }
}

/// Mirror of the engine's DirKey name validation.
fn valid_name(n: &[u8]) -> bool {
    !n.is_empty() && n.len() <= 255 && !n.contains(&b'/') && !n.contains(&0)
}

fn valid_xname(n: &[u8]) -> bool {
    !n.is_empty() && n.len() <= 255
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NT {
    File,
    Dir,
    Symlink,
}

fn ftype_of(nt: NT) -> u8 {
    match nt {
        NT::File => FTYPE_FILE,
        NT::Dir => FTYPE_DIR,
        NT::Symlink => FTYPE_SYMLINK,
    }
}

#[derive(Clone, Debug)]
struct Node {
    nt: NT,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    /// File bytes; symlink target bytes for symlinks.
    data: Vec<u8>,
    xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
struct Snap {
    name: Vec<u8>,
    nodes: BTreeMap<u64, Node>,
    children: BTreeMap<(u64, Vec<u8>), u64>,
}

/// The model world. `committed` is the last committed namespace (None until
/// the first commit); a reopen rolls back to it. Quota *limits* are not
/// persisted by the engine, so they are cleared on reopen.
#[derive(Clone, Debug)]
struct World {
    nodes: BTreeMap<u64, Node>,
    /// (parent ino, name) -> child ino. Live namespace only.
    children: BTreeMap<(u64, Vec<u8>), u64>,
    /// Creation order of all inos (including deleted ones, for CRef).
    order: Vec<u64>,
    /// uid -> blocks charged.
    usage: HashMap<u32, u64>,
    /// uid -> quota limit (not persisted across reopen).
    quotas: HashMap<u32, u64>,
    /// fs snap id -> snapshot.
    snaps: BTreeMap<u64, Snap>,
    committed: Option<Box<World>>,
}

impl World {
    fn fresh(root_mode: u32) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                nt: NT::Dir,
                mode: root_mode,
                uid: 0,
                gid: 0,
                nlink: 2,
                data: Vec::new(),
                xattrs: BTreeMap::new(),
            },
        );
        World {
            nodes,
            children: BTreeMap::new(),
            order: Vec::new(),
            usage: HashMap::new(),
            quotas: HashMap::new(),
            snaps: BTreeMap::new(),
            committed: None,
        }
    }

    /// Strip the committed chain for storage inside `committed`.
    fn without_committed(&self) -> World {
        let mut w = self.clone();
        w.committed = None;
        w
    }

    fn live_dirs(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.nt == NT::Dir)
            .map(|(i, _)| *i)
            .collect()
    }

    fn resolve_parent(&self, r: u8) -> u64 {
        let dirs = self.live_dirs();
        debug_assert!(!dirs.is_empty());
        dirs[r as usize % dirs.len()]
    }

    /// Resolve a creation-order reference; may point at a deleted node.
    /// Returns None only when nothing has ever been created.
    fn resolve_ref(&self, r: u8) -> Option<u64> {
        if self.order.is_empty() {
            return None;
        }
        Some(self.order[r as usize % self.order.len()])
    }

    fn check_quota(&self, uid: u32, new_blocks: u64) -> Result<(), &'static str> {
        if let Some(&limit) = self.quotas.get(&uid) {
            let used = self.usage.get(&uid).copied().unwrap_or(0);
            if used + new_blocks > limit {
                return Err("QuotaExceeded");
            }
        }
        Ok(())
    }

    fn add_usage(&mut self, uid: u32, delta: u64) {
        *self.usage.entry(uid).or_insert(0) += delta;
    }

    fn sub_usage(&mut self, uid: u32, delta: u64) {
        let e = self.usage.entry(uid).or_insert(0);
        *e = e.saturating_sub(delta);
    }

    /// Is `anc` an ancestor of (or equal to) `ino` in the live tree?
    fn is_ancestor_or_self(&self, anc: u64, mut ino: u64) -> bool {
        loop {
            if ino == anc {
                return true;
            }
            if ino == ROOT_INO {
                return false;
            }
            let mut found = None;
            for ((p, _), c) in &self.children {
                if *c == ino {
                    found = Some(*p);
                    break;
                }
            }
            match found {
                Some(p) => ino = p,
                None => return false,
            }
        }
    }

    /// Is `dir_ino` empty (no children)? Caller guarantees it is a dir.
    fn dir_is_empty(&self, dir_ino: u64) -> bool {
        !self.children.keys().any(|(p, _)| *p == dir_ino)
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    CreateFile {
        parent: u8,
        name: u8,
        uid: u8,
        mode: u8,
    },
    CreateDir {
        parent: u8,
        name: u8,
        uid: u8,
        mode: u8,
    },
    Symlink {
        parent: u8,
        name: u8,
        target: u8,
        uid: u8,
    },
    HardLink {
        target: u8,
        parent: u8,
        name: u8,
    },
    Write {
        target: u8,
        offset: u16,
        data: Vec<u8>,
    },
    Truncate {
        target: u8,
        size: u16,
    },
    Unlink {
        parent: u8,
        name: u8,
    },
    Rmdir {
        parent: u8,
        name: u8,
    },
    Rename {
        sp: u8,
        sn: u8,
        dp: u8,
        dn: u8,
    },
    SetXattr {
        target: u8,
        xname: u8,
        value: Vec<u8>,
    },
    RemoveXattr {
        target: u8,
        xname: u8,
    },
    SetQuota {
        uid: u8,
        limit: u8,
    },
    Chown {
        target: u8,
        uid: u8,
    },
    SnapCreate {
        name: u8,
    },
    SnapDelete {
        idx: u8,
    },
    Commit,
    Reopen,
}

/// Small name pool (8 names) so collisions exercise AlreadyExists; a few
/// indices produce invalid names for BadName coverage.
fn name_bytes(n: u8) -> Vec<u8> {
    match n % 24 {
        16 => Vec::new(),
        17 => vec![b'x'; 300],
        18 => b"a/b".to_vec(),
        19 => b"a\x00b".to_vec(),
        v => format!("n{}", v % 8).into_bytes(),
    }
}

fn uid_of(n: u8) -> u32 {
    1000 + (n % 4) as u32
}

fn mode_of(n: u8) -> u32 {
    [0o644, 0o755, 0o600, 0o777][n as usize % 4]
}

fn snap_name(n: u8) -> Vec<u8> {
    match n % 8 {
        6 => Vec::new(),      // BadName
        7 => vec![b's'; 100], // BadName (>64)
        v => format!("s{}", v % 4).into_bytes(),
    }
}

fn xname_of(n: u8) -> Vec<u8> {
    match n % 8 {
        6 => Vec::new(),
        7 => vec![b'y'; 300],
        v => format!("user.x{}", v % 4).into_bytes(),
    }
}

/// Quota limits: 0 removes the quota; tiny limits trigger QuotaExceeded.
fn quota_limit_of(n: u8) -> u64 {
    [0, 1, 2, 5, 100, 100_000][n as usize % 6]
}

fn data_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..64)
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(parent, name, uid, mode)| Op::CreateFile { parent, name, uid, mode }),
        6 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(parent, name, uid, mode)| Op::CreateDir { parent, name, uid, mode }),
        4 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(parent, name, target, uid)| Op::Symlink { parent, name, target, uid }),
        4 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(target, parent, name)| Op::HardLink { target, parent, name }),
        12 => (any::<u8>(), any::<u16>(), data_strategy())
            .prop_map(|(target, offset, data)| Op::Write {
                target,
                offset: offset % 2048,
                data,
            }),
        5 => (any::<u8>(), any::<u16>())
            .prop_map(|(target, size)| Op::Truncate { target, size: size % 8192 }),
        6 => (any::<u8>(), any::<u8>())
            .prop_map(|(parent, name)| Op::Unlink { parent, name }),
        4 => (any::<u8>(), any::<u8>())
            .prop_map(|(parent, name)| Op::Rmdir { parent, name }),
        6 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(sp, sn, dp, dn)| Op::Rename { sp, sn, dp, dn }),
        5 => (any::<u8>(), any::<u8>(), data_strategy())
            .prop_map(|(target, xname, value)| Op::SetXattr { target, xname, value }),
        3 => (any::<u8>(), any::<u8>())
            .prop_map(|(target, xname)| Op::RemoveXattr { target, xname }),
        3 => (any::<u8>(), any::<u8>())
            .prop_map(|(uid, limit)| Op::SetQuota { uid, limit }),
        3 => (any::<u8>(), any::<u8>())
            .prop_map(|(target, uid)| Op::Chown { target, uid }),
        4 => any::<u8>().prop_map(|name| Op::SnapCreate { name }),
        3 => any::<u8>().prop_map(|idx| Op::SnapDelete { idx }),
        2 => Just(Op::Commit),
        2 => Just(Op::Reopen),
    ]
}

// ---------------------------------------------------------------------------
// Driver: applies ops to both Fs and model, comparing exact outcomes.
// ---------------------------------------------------------------------------

struct Driver {
    fs: Fs,
    img: PathBuf,
    w: World,
}

/// Compare a model prediction against the fs result.
fn check<T>(
    expect: Result<(), &'static str>,
    actual: Result<T, FsError>,
    ctx: &str,
) -> Result<Option<T>, String> {
    match (expect, actual) {
        (Ok(()), Ok(v)) => Ok(Some(v)),
        (Err(e), Err(fe)) if e == disc(&fe) => Ok(None),
        (Ok(()), Err(fe)) => Err(format!(
            "{ctx}: model predicted Ok, fs failed with {} ({fe:?})",
            disc(&fe)
        )),
        (Err(e), Ok(_)) => Err(format!("{ctx}: model predicted {e}, fs succeeded")),
        (Err(e), Err(fe)) => Err(format!(
            "{ctx}: model predicted {e}, fs failed with {} ({fe:?})",
            disc(&fe)
        )),
    }
}

impl Driver {
    fn apply(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::CreateFile {
                parent,
                name,
                uid,
                mode,
            } => {
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                let u = uid_of(*uid);
                let m = mode_of(*mode) & 0o7777;
                // Engine order: parent NotFound/NotDir, BadName,
                // AlreadyExists, QuotaExceeded.
                let expect = if !self.w.nodes.contains_key(&p) {
                    Err("NotFound")
                } else if self.w.nodes[&p].nt != NT::Dir {
                    Err("NotDir")
                } else if !valid_name(&nm) {
                    Err("BadName")
                } else if self.w.children.contains_key(&(p, nm.clone())) {
                    Err("AlreadyExists")
                } else {
                    self.w.check_quota(u, 1)
                };
                let r = check(expect, self.fs.create(p, &nm, m, u, u), "create")?;
                if let Some(ino) = r {
                    self.w.nodes.insert(
                        ino,
                        Node {
                            nt: NT::File,
                            mode: m,
                            uid: u,
                            gid: u,
                            nlink: 1,
                            data: Vec::new(),
                            xattrs: BTreeMap::new(),
                        },
                    );
                    self.w.children.insert((p, nm), ino);
                    self.w.order.push(ino);
                    self.w.add_usage(u, 1);
                }
            }
            Op::CreateDir {
                parent,
                name,
                uid,
                mode,
            } => {
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                let u = uid_of(*uid);
                let m = mode_of(*mode) & 0o7777;
                let expect = if !self.w.nodes.contains_key(&p) {
                    Err("NotFound")
                } else if self.w.nodes[&p].nt != NT::Dir {
                    Err("NotDir")
                } else if !valid_name(&nm) {
                    Err("BadName")
                } else if self.w.children.contains_key(&(p, nm.clone())) {
                    Err("AlreadyExists")
                } else {
                    self.w.check_quota(u, 1)
                };
                let r = check(expect, self.fs.mkdir(p, &nm, m, u, u), "mkdir")?;
                if let Some(ino) = r {
                    self.w.nodes.insert(
                        ino,
                        Node {
                            nt: NT::Dir,
                            mode: m,
                            uid: u,
                            gid: u,
                            nlink: 2,
                            data: Vec::new(),
                            xattrs: BTreeMap::new(),
                        },
                    );
                    self.w.children.insert((p, nm), ino);
                    self.w.order.push(ino);
                    self.w.add_usage(u, 1);
                }
            }
            Op::Symlink {
                parent,
                name,
                target,
                uid,
            } => {
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                let u = uid_of(*uid);
                let tgt = format!("/s{}", target % 4).into_bytes();
                // Engine: symlink() = mknode (1 block) + write(target);
                // total charge is blocks_for_size(target.len()).
                let tblks = blocks_for_size(tgt.len() as u64);
                let expect = if !self.w.nodes.contains_key(&p) {
                    Err("NotFound")
                } else if self.w.nodes[&p].nt != NT::Dir {
                    Err("NotDir")
                } else if !valid_name(&nm) {
                    Err("BadName")
                } else if self.w.children.contains_key(&(p, nm.clone())) {
                    Err("AlreadyExists")
                } else {
                    self.w.check_quota(u, tblks)
                };
                let r = check(expect, self.fs.symlink(p, &nm, &tgt, u, u), "symlink")?;
                if let Some(ino) = r {
                    self.w.nodes.insert(
                        ino,
                        Node {
                            nt: NT::Symlink,
                            mode: 0o777,
                            uid: u,
                            gid: u,
                            nlink: 1,
                            data: tgt,
                            xattrs: BTreeMap::new(),
                        },
                    );
                    self.w.children.insert((p, nm), ino);
                    self.w.order.push(ino);
                    self.w.add_usage(u, tblks);
                }
            }
            Op::HardLink {
                target,
                parent,
                name,
            } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                // Engine order: target NotFound, target-is-dir Invalid,
                // BadName, AlreadyExists.
                let expect = match self.w.nodes.get(&t) {
                    None => Err("NotFound"),
                    Some(n) if n.nt == NT::Dir => Err("Invalid"),
                    Some(_) if !valid_name(&nm) => Err("BadName"),
                    Some(_) if self.w.children.contains_key(&(p, nm.clone())) => {
                        Err("AlreadyExists")
                    }
                    Some(_) => Ok(()),
                };
                let r = check(expect, self.fs.link(t, p, &nm), "link")?;
                if r.is_some() {
                    // Fs succeeded: target is a live non-dir, name is valid
                    // and fresh.
                    if let Some(n) = self.w.nodes.get_mut(&t) {
                        if n.nt != NT::Dir {
                            n.nlink += 1;
                            self.w.children.insert((p, nm), t);
                        }
                    }
                }
            }
            Op::Write {
                target,
                offset,
                data,
            } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                // Engine: empty write is Ok even for missing ino; then
                // NotFound, NotFile (dir), quota on growth.
                let expect = if data.is_empty() {
                    Ok(())
                } else {
                    match self.w.nodes.get(&t) {
                        None => Err("NotFound"),
                        Some(n) if n.nt != NT::File => Err("NotFile"),
                        Some(n) => {
                            let new_size =
                                (*offset as u64 + data.len() as u64).max(n.data.len() as u64);
                            let growth = blocks_for_size(new_size)
                                .saturating_sub(blocks_for_size(n.data.len() as u64));
                            if growth > 0 {
                                self.w.check_quota(n.uid, growth)
                            } else {
                                Ok(())
                            }
                        }
                    }
                };
                let r = check(expect, self.fs.write(t, *offset as u64, data), "write")?;
                if r.is_some() && !data.is_empty() {
                    // Fs succeeded: mirror the overlay. The target is a live
                    // file (otherwise the prediction would not be Ok).
                    if let Some(n) = self.w.nodes.get_mut(&t) {
                        if n.nt == NT::File {
                            let new_size =
                                (*offset as u64 + data.len() as u64).max(n.data.len() as u64);
                            let growth = blocks_for_size(new_size)
                                .saturating_sub(blocks_for_size(n.data.len() as u64));
                            let end = *offset as usize + data.len();
                            if n.data.len() < end {
                                n.data.resize(end, 0);
                            }
                            n.data[*offset as usize..end].copy_from_slice(data);
                            let uid = n.uid;
                            self.w.add_usage(uid, growth);
                        }
                    }
                }
            }
            Op::Truncate { target, size } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                let expect = match self.w.nodes.get(&t) {
                    None => Err("NotFound"),
                    Some(n) if n.nt != NT::File => Err("NotFile"),
                    Some(n) => {
                        let old = n.data.len() as u64;
                        let (ob, nb) = (blocks_for_size(old), blocks_for_size(*size as u64));
                        if nb > ob {
                            self.w.check_quota(n.uid, nb - ob)
                        } else {
                            Ok(())
                        }
                    }
                };
                let r = check(expect, self.fs.truncate(t, *size as u64), "truncate")?;
                if r.is_some() {
                    if let Some(n) = self.w.nodes.get_mut(&t) {
                        if n.nt == NT::File {
                            let old = n.data.len() as u64;
                            let (ob, nb) = (blocks_for_size(old), blocks_for_size(*size as u64));
                            n.data.resize(*size as usize, 0);
                            let uid = n.uid;
                            let d = nb as i64 - ob as i64;
                            if d > 0 {
                                self.w.add_usage(uid, d as u64);
                            } else if d < 0 {
                                self.w.sub_usage(uid, (-d) as u64);
                            }
                        }
                    }
                }
            }
            Op::Unlink { parent, name } => {
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                // Engine order: BadName, NotFound, Invalid (is a dir).
                let child = self.w.children.get(&(p, nm.clone())).copied();
                let expect = if !valid_name(&nm) {
                    Err("BadName")
                } else {
                    match child {
                        None => Err("NotFound"),
                        Some(c) if self.w.nodes[&c].nt == NT::Dir => Err("Invalid"),
                        Some(_) => Ok(()),
                    }
                };
                check(expect, self.fs.unlink(p, &nm), "unlink")?;
                if let Some(c) = child {
                    if valid_name(&nm) && self.w.nodes[&c].nt != NT::Dir {
                        self.w.children.remove(&(p, nm));
                        let dead = {
                            let n = self.w.nodes.get_mut(&c).unwrap();
                            n.nlink -= 1;
                            n.nlink == 0
                        };
                        if dead {
                            let n = self.w.nodes.remove(&c).unwrap();
                            self.w
                                .sub_usage(n.uid, blocks_for_size(n.data.len() as u64));
                        }
                    }
                }
            }
            Op::Rmdir { parent, name } => {
                let p = self.w.resolve_parent(*parent);
                let nm = name_bytes(*name);
                // Engine order: BadName, NotFound, NotDir, NotEmpty.
                let child = self.w.children.get(&(p, nm.clone())).copied();
                let expect = if !valid_name(&nm) {
                    Err("BadName")
                } else {
                    match child {
                        None => Err("NotFound"),
                        Some(c) if self.w.nodes[&c].nt != NT::Dir => Err("NotDir"),
                        Some(c) if !self.w.dir_is_empty(c) => Err("NotEmpty"),
                        Some(_) => Ok(()),
                    }
                };
                check(expect, self.fs.rmdir(p, &nm), "rmdir")?;
                if let Some(c) = child {
                    if valid_name(&nm) && self.w.nodes[&c].nt == NT::Dir && self.w.dir_is_empty(c) {
                        self.w.children.remove(&(p, nm));
                        let n = self.w.nodes.remove(&c).unwrap();
                        self.w
                            .sub_usage(n.uid, blocks_for_size(n.data.len() as u64));
                    }
                }
            }
            Op::Rename { sp, sn, dp, dn } => {
                let src_p = self.w.resolve_parent(*sp);
                let dst_p = self.w.resolve_parent(*dp);
                let snm = name_bytes(*sn);
                let dnm = name_bytes(*dn);
                let src_child = self.w.children.get(&(src_p, snm.clone())).copied();
                let dst_child = self.w.children.get(&(dst_p, dnm.clone())).copied();
                // Engine order: src BadName, src NotFound, dst BadName,
                // same-key no-op, dir-into-own-subtree Invalid, dst victim
                // NotEmpty check, then move.
                let expect: Result<(), &'static str> = if !valid_name(&snm) {
                    Err("BadName")
                } else if src_child.is_none() {
                    Err("NotFound")
                } else if !valid_name(&dnm) {
                    Err("BadName")
                } else if src_p == dst_p && snm == dnm {
                    Ok(())
                } else {
                    let sc = src_child.unwrap();
                    if self.w.nodes[&sc].nt == NT::Dir && self.w.is_ancestor_or_self(sc, dst_p) {
                        Err("Invalid")
                    } else if let Some(dc) = dst_child {
                        if self.w.nodes[&dc].nt == NT::Dir {
                            if self.w.dir_is_empty(dc) {
                                Ok(())
                            } else {
                                Err("NotEmpty")
                            }
                        } else {
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                };
                let r = check(expect, self.fs.rename(src_p, &snm, dst_p, &dnm), "rename")?;
                if r.is_some() {
                    // Fs succeeded: mirror the move. Same-key renames are
                    // Ok no-ops; they still return Some(()) from check.
                    if src_p == dst_p && snm == dnm {
                        // no-op
                    } else if let Some(sc) = src_child {
                        // Remove any victim at the destination.
                        if let Some(dc) = dst_child {
                            self.w.children.remove(&(dst_p, dnm.clone()));
                            let victim = self.w.nodes.get(&dc).map(|n| n.nt);
                            if victim == Some(NT::Dir) {
                                let n = self.w.nodes.remove(&dc).unwrap();
                                // Correct semantics: the victim dir's block is
                                // freed. (The engine currently leaks it; the
                                // model test will catch the divergence.)
                                self.w
                                    .sub_usage(n.uid, blocks_for_size(n.data.len() as u64));
                            } else if let Some(n) = self.w.nodes.get_mut(&dc) {
                                n.nlink -= 1;
                                if n.nlink == 0 {
                                    let n = self.w.nodes.remove(&dc).unwrap();
                                    self.w
                                        .sub_usage(n.uid, blocks_for_size(n.data.len() as u64));
                                }
                            }
                        }
                        self.w.children.remove(&(src_p, snm));
                        self.w.children.insert((dst_p, dnm), sc);
                    }
                }
            }
            Op::SetXattr {
                target,
                xname,
                value,
            } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                let xn = xname_of(*xname);
                // Engine: getattr (NotFound), name Invalid, value Invalid.
                let expect = if !self.w.nodes.contains_key(&t) {
                    Err("NotFound")
                } else if !valid_xname(&xn) || value.len() > 65536 {
                    Err("Invalid")
                } else {
                    Ok(())
                };
                check(expect, self.fs.setxattr(t, &xn, value), "setxattr")?;
                if self.w.nodes.contains_key(&t) && valid_xname(&xn) && value.len() <= 65536 {
                    self.w
                        .nodes
                        .get_mut(&t)
                        .unwrap()
                        .xattrs
                        .insert(xn, value.clone());
                }
            }
            Op::RemoveXattr { target, xname } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                let xn = xname_of(*xname);
                // Engine: getattr (NotFound), then remove (bool).
                // xname validity: removexattr on invalid name?
                let expect = if !self.w.nodes.contains_key(&t) {
                    Err("NotFound")
                } else {
                    Ok(())
                };
                let r = check(expect, self.fs.removexattr(t, &xn), "removexattr")?;
                if let Some(existed) = r {
                    let mn = self.w.nodes.get_mut(&t).unwrap().xattrs.remove(&xn);
                    if mn.is_some() != existed {
                        return Err(format!(
                            "removexattr({t}): model existed={} fs existed={existed}",
                            mn.is_some()
                        ));
                    }
                }
            }
            Op::SetQuota { uid, limit } => {
                let u = uid_of(*uid);
                let l = quota_limit_of(*limit);
                self.fs.set_quota(u, l).unwrap();
                if l == 0 {
                    self.w.quotas.remove(&u);
                } else {
                    self.w.quotas.insert(u, l);
                }
            }
            Op::Chown { target, uid } => {
                let t = match self.w.resolve_ref(*target) {
                    None => return Ok(()),
                    Some(t) => t,
                };
                let u = uid_of(*uid);
                // Engine: getattr (NotFound); if uid changes, transfer quota
                // (correct semantics: atomic — check new owner's quota first).
                let expect = match self.w.nodes.get(&t) {
                    None => Err("NotFound"),
                    Some(n) if n.uid == u => Ok(()),
                    Some(n) => {
                        let b = blocks_for_size(n.data.len() as u64);
                        self.w.check_quota(u, b)
                    }
                };
                let attrs = SetAttrs {
                    mode: None,
                    uid: Some(u),
                    gid: None,
                    size: None,
                    atime: None,
                    mtime: None,
                };
                let r = check(expect, self.fs.setattr(t, &attrs), "chown")?;
                // NOTE: the engine's uid transfer is non-atomic (it debits
                // the old owner before checking the new owner's quota); the
                // model deliberately predicts atomic semantics, so a
                // divergence here is a real engine bug, not a model bug.
                if r.is_some() {
                    if let Some(n) = self.w.nodes.get_mut(&t) {
                        if n.uid != u {
                            let b = blocks_for_size(n.data.len() as u64);
                            let old = n.uid;
                            n.uid = u;
                            self.w.sub_usage(old, b);
                            self.w.add_usage(u, b);
                        }
                    }
                }
            }
            Op::SnapCreate { name } => {
                let nm = snap_name(*name);
                let expect = if nm.is_empty() || nm.len() > 64 {
                    Err("BadName")
                } else {
                    Ok(())
                };
                let r = check(expect, self.fs.snapshot_create(&nm), "snap_create")?;
                if let Some(id) = r {
                    self.w.snaps.insert(
                        id,
                        Snap {
                            name: nm,
                            nodes: self.w.nodes.clone(),
                            children: self.w.children.clone(),
                        },
                    );
                }
            }
            Op::SnapDelete { idx } => {
                let ids: Vec<u64> = self.w.snaps.keys().copied().collect();
                let (expect, id) = if ids.is_empty() {
                    (Err("NotFound"), 9999u64)
                } else {
                    (Ok(()), ids[*idx as usize % ids.len()])
                };
                check(expect, self.fs.snapshot_delete(id), "snap_delete")?;
                if self.w.snaps.contains_key(&id) {
                    self.w.snaps.remove(&id);
                }
            }
            Op::Commit => {
                self.fs
                    .commit()
                    .map_err(|e| format!("commit failed: {e:?}"))?;
                self.w.committed = Some(Box::new(self.w.without_committed()));
                self.verify()?;
            }
            Op::Reopen => {
                let new_fs = Fs::open(&self.img).map_err(|e| format!("reopen failed: {e:?}"))?;
                let _ = std::mem::replace(&mut self.fs, new_fs);
                // Roll back the model to the last commit. Quota limits are
                // not persisted by the engine, so they are cleared.
                match self.w.committed.clone() {
                    Some(c) => {
                        let mut nw = *c;
                        // Keep rollback idempotent across consecutive reopens.
                        nw.committed = Some(Box::new(nw.without_committed()));
                        self.w = nw;
                    }
                    None => {
                        let root_mode = self
                            .fs
                            .getattr(ROOT_INO)
                            .map_err(|e| format!("getattr root: {e:?}"))?
                            .mode;
                        self.w = World::fresh(root_mode);
                    }
                }
                self.w.quotas.clear();
                self.fs
                    .check()
                    .map_err(|e| format!("check after reopen failed: {e:?}"))?;
                self.verify()?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

impl Driver {
    /// Cross-check the full observable state: namespace, file bytes,
    /// symlink targets, xattrs, quota usage, and snapshot contents.
    fn verify(&mut self) -> Result<(), String> {
        for (ino, node) in &self.w.nodes {
            let attr = self
                .fs
                .getattr(*ino)
                .map_err(|e| format!("verify: getattr({ino}) failed: {e:?}"))?;
            let ck = |what: &str, a: u64, b: u64| -> Result<(), String> {
                if a != b {
                    return Err(format!(
                        "verify: ino {ino} {what}: fs={a} model={b} (node={node:?})"
                    ));
                }
                Ok(())
            };
            ck("ftype", attr.ftype as u64, ftype_of(node.nt) as u64)?;
            ck("mode", attr.mode as u64, node.mode as u64)?;
            ck("uid", attr.uid as u64, node.uid as u64)?;
            ck("gid", attr.gid as u64, node.gid as u64)?;
            ck("nlink", attr.nlink as u64, node.nlink as u64)?;
            ck("size", attr.size, node.data.len() as u64)?;
            match node.nt {
                NT::File => {
                    let d = self
                        .fs
                        .read(*ino, 0, node.data.len())
                        .map_err(|e| format!("verify: read({ino}) failed: {e:?}"))?;
                    if d != node.data {
                        return Err(format!("verify: ino {ino} data mismatch"));
                    }
                }
                NT::Symlink => {
                    let t = self
                        .fs
                        .readlink(*ino)
                        .map_err(|e| format!("verify: readlink({ino}) failed: {e:?}"))?;
                    if t != node.data {
                        return Err(format!("verify: ino {ino} symlink target mismatch"));
                    }
                }
                NT::Dir => {
                    let mut ents = self
                        .fs
                        .readdir(*ino)
                        .map_err(|e| format!("verify: readdir({ino}) failed: {e:?}"))?;
                    ents.sort();
                    let mut expected: Vec<(Vec<u8>, u64, u8)> = self
                        .w
                        .children
                        .iter()
                        .filter(|((p, _), _)| *p == *ino)
                        .map(|((_, name), c)| (name.clone(), *c, ftype_of(self.w.nodes[c].nt)))
                        .collect();
                    expected.sort();
                    if ents != expected {
                        return Err(format!(
                            "verify: ino {ino} readdir mismatch:\n fs={ents:?}\n model={expected:?}"
                        ));
                    }
                }
            }
            let mut xl = self
                .fs
                .listxattrs(*ino)
                .map_err(|e| format!("verify: listxattrs({ino}) failed: {e:?}"))?;
            xl.sort();
            let mut mk: Vec<Vec<u8>> = node.xattrs.keys().cloned().collect();
            mk.sort();
            if xl != mk {
                return Err(format!(
                    "verify: ino {ino} xattr list mismatch: fs={xl:?} model={mk:?}"
                ));
            }
            for (k, v) in &node.xattrs {
                let gv = self
                    .fs
                    .getxattr(*ino, k)
                    .map_err(|e| format!("verify: getxattr({ino}) failed: {e:?}"))?;
                if gv.as_ref() != Some(v) {
                    return Err(format!("verify: ino {ino} xattr value mismatch"));
                }
            }
        }
        // Quota usage for every uid the model knows about.
        for u in self.w.usage.keys().chain(self.w.quotas.keys()) {
            let mu = self.w.usage.get(u).copied().unwrap_or(0);
            let fu = self.fs.quota_usage(*u);
            if mu != fu {
                return Err(format!("verify: quota_usage({u}): fs={fu} model={mu}"));
            }
        }
        // Snapshots: id set, names, and spot-check contents.
        let mut sl = self
            .fs
            .snapshot_list()
            .map_err(|e| format!("verify: snapshot_list failed: {e:?}"))?;
        sl.sort();
        let mut ml: Vec<(u64, Vec<u8>)> = self
            .w
            .snaps
            .iter()
            .map(|(id, s)| (*id, s.name.clone()))
            .collect();
        ml.sort();
        if sl != ml {
            return Err(format!(
                "verify: snapshot_list mismatch: fs={sl:?} model={ml:?}"
            ));
        }
        for (id, snap) in &self.w.snaps {
            let mut checked = 0;
            for (ino, node) in &snap.nodes {
                if checked >= 3 {
                    break;
                }
                match node.nt {
                    NT::File if !node.data.is_empty() => {
                        let d = self
                            .fs
                            .snapshot_read(*id, *ino, 0, node.data.len())
                            .map_err(|e| {
                                format!("verify: snapshot_read({id},{ino}) failed: {e:?}")
                            })?;
                        if d != node.data {
                            return Err(format!("verify: snapshot {id} ino {ino} data mismatch"));
                        }
                        checked += 1;
                    }
                    _ => {}
                }
            }
            // Snapshot directory listings.
            let mut dirs_checked = 0;
            for (ino, node) in &snap.nodes {
                if node.nt != NT::Dir || dirs_checked >= 4 {
                    continue;
                }
                dirs_checked += 1;
                let mut ents = self
                    .fs
                    .snapshot_readdir(*id, *ino)
                    .map_err(|e| format!("verify: snapshot_readdir({id},{ino}) failed: {e:?}"))?;
                ents.sort();
                let mut expected: Vec<(Vec<u8>, u64, u8)> = snap
                    .children
                    .iter()
                    .filter(|((p, _), _)| *p == *ino)
                    .map(|((_, name), c)| (name.clone(), *c, ftype_of(snap.nodes[c].nt)))
                    .collect();
                expected.sort();
                if ents != expected {
                    return Err(format!("verify: snapshot {id} ino {ino} readdir mismatch"));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

static CASE_SEQ: AtomicU64 = AtomicU64::new(0);

fn t2_config() -> ProptestConfig {
    let cases: u32 = std::env::var("T2_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    ProptestConfig::with_cases(cases)
}

fn t2_max_ops() -> usize {
    std::env::var("T2_MAX_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

fn run_case(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let seq = CASE_SEQ.fetch_add(1, Ordering::Relaxed);
    let img: PathBuf =
        std::env::temp_dir().join(format!("cownfs-t2-{}-{}.img", std::process::id(), seq));
    let _ = std::fs::remove_file(&img);
    let fs = Fs::format(&img, 1024).map_err(|e| TestCaseError::fail(format!("format: {e:?}")))?;
    let root_mode = fs
        .getattr(ROOT_INO)
        .map_err(|e| TestCaseError::fail(format!("getattr root: {e:?}")))?
        .mode;
    let mut drv = Driver {
        fs,
        img: img.clone(),
        w: World::fresh(root_mode),
    };
    for (i, op) in ops.iter().enumerate() {
        drv.apply(op)
            .map_err(|e| TestCaseError::fail(format!("op #{i} {op:?}: {e}")))?;
        drv.verify()
            .map_err(|e| TestCaseError::fail(format!("op #{i} {op:?}: {e}")))?;
    }
    let _ = std::fs::remove_file(&img);
    Ok(())
}

proptest! {
    #![proptest_config(t2_config())]
    #[test]
    fn t2_model_proptest(ops in prop::collection::vec(op_strategy(), 1..t2_max_ops())) {
        run_case(ops)?;
    }
}
