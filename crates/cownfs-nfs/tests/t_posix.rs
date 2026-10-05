//! T_POSIX: POSIX compliance suite, exercised over NFSv4.0.
//!
//! cownfs is exported only over NFS (no FUSE/local mount), so POSIX
//! conformance is tested the way a POSIX client sees it: a client-side
//! path resolver — mirroring the Linux kernel NFS client — turns POSIX
//! paths into NFS compounds, and each test asserts POSIX-specified
//! outcomes (errno semantics, symlink following, atomic rename, link
//! counts, ...).
//!
//! This complements the NFS protocol suites (p8–p12, T5/pynfs): those
//! check wire conformance; this checks that the filesystem *behaves*
//! like POSIX. Permission (EACCES) tests are out of scope: the server
//! uses AUTH_SYS and does not enforce mode bits.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server, NfsClient, Ops, Reply};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::*;
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// POSIX emulation layer
// ---------------------------------------------------------------------------

/// POSIX errno equivalents relevant to the suite.
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Errno {
    ENOENT,
    EEXIST,
    ENOTDIR,
    EISDIR,
    ENOTEMPTY,
    ELOOP,
    EINVAL,
    ENAMETOOLONG,
}

fn nfs_err(st: u32) -> Errno {
    match st {
        NFS4ERR_NOENT => Errno::ENOENT,
        NFS4ERR_EXIST => Errno::EEXIST,
        NFS4ERR_NOTDIR => Errno::ENOTDIR,
        NFS4ERR_ISDIR => Errno::EISDIR,
        NFS4ERR_INVAL => Errno::EINVAL,
        // NFSv4.0 has no NOTEMPTY code; the server uses NOTSUPP for it.
        NFS4ERR_NOTSUPP => Errno::ENOTEMPTY,
        _ => Errno::EINVAL,
    }
}

#[derive(Debug)]
struct Stat {
    ftype: u32,
    size: u64,
    nlink: u32,
    mode: u32,
}

/// Client-side POSIX path resolution over NFS, mirroring the kernel
/// client's behavior: symlinks are followed by the client (up to 40,
/// then ELOOP), `..` walks up, trailing slashes require directories.
struct Px<'a> {
    c: &'a mut NfsClient,
    uuid: [u8; 16],
    clientid: u64,
}

impl<'a> Px<'a> {
    fn new(c: &'a mut NfsClient, uuid: [u8; 16], clientid: u64) -> Self {
        Px { c, uuid, clientid }
    }

    /// PUTFH(dir) + LOOKUP(name) + GETFH -> child ino.
    fn lookup_ino(&mut self, dir: u64, name: &[u8]) -> Result<u64, u32> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.lookup(name);
        ops.getfh();
        let (st, res) = self.c.call(b"px-lookup", ops);
        if st != NFS4_OK {
            return Err(st);
        }
        match &res[2] {
            Reply::Fh(fh) => Ok(fh.inode),
            r => panic!("px-lookup: unexpected reply {r:?}"),
        }
    }

    fn ftype(&mut self, ino: u64) -> Result<u32, u32> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.getattr(&[FATTR4_TYPE]);
        let (st, res) = self.c.call(b"px-type", ops);
        if st != NFS4_OK {
            return Err(st);
        }
        match &res[1] {
            Reply::Attrs(a) => Ok(common::attr_u32(a, FATTR4_TYPE)),
            r => panic!("px-type: unexpected reply {r:?}"),
        }
    }

    /// Read a symlink's target (the server serves it via READ).
    fn read_target(&mut self, ino: u64) -> Result<Vec<u8>, u32> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.read(0, 4096);
        let (st, res) = self.c.call(b"px-readlink", ops);
        if st != NFS4_OK {
            return Err(st);
        }
        match &res[1] {
            Reply::Read { data, .. } => Ok(data.clone()),
            r => panic!("px-readlink: unexpected reply {r:?}"),
        }
    }

    /// Resolve `path` to an ino. If `follow_final` is false, a trailing
    /// symlink resolves to the link itself (lstat/open(O_NOFOLLOW)).
    fn resolve(&mut self, path: &str, follow_final: bool) -> Result<u64, Errno> {
        if !path.starts_with('/') {
            return Err(Errno::EINVAL);
        }
        let mut wl: VecDeque<Vec<u8>> = path.split('/').map(|s| s.as_bytes().to_vec()).collect();
        let mut dir = ROOT_INO;
        let mut stack: Vec<u64> = Vec::new(); // ancestors for ".."
        let mut follows = 0u32;
        let trailing_slash = path.len() > 1 && path.ends_with('/');
        while let Some(comp) = wl.pop_front() {
            if comp.is_empty() || comp == b"." {
                continue;
            }
            if comp == b".." {
                dir = stack.pop().unwrap_or(ROOT_INO);
                continue;
            }
            if comp.len() > 255 {
                return Err(Errno::ENAMETOOLONG);
            }
            let is_last = wl.is_empty();
            let child = self.lookup_ino(dir, &comp).map_err(|st| {
                if st == NFS4ERR_NOENT {
                    Errno::ENOENT
                } else {
                    nfs_err(st)
                }
            })?;
            let t = self.ftype(child).map_err(nfs_err)?;
            if t == NF4LNK && (!is_last || follow_final) {
                follows += 1;
                if follows > 40 {
                    return Err(Errno::ELOOP);
                }
                let target = self.read_target(child).map_err(nfs_err)?;
                // Splice the target's components in front of the worklist.
                let mut tc: VecDeque<Vec<u8>> =
                    target.split(|&b| b == b'/').map(|s| s.to_vec()).collect();
                if target.first() == Some(&b'/') {
                    dir = ROOT_INO;
                    stack.clear();
                }
                while let Some(c) = tc.pop_back() {
                    wl.push_front(c);
                }
                continue;
            }
            if !is_last {
                if t != NF4DIR {
                    return Err(Errno::ENOTDIR);
                }
                stack.push(dir);
                dir = child;
            } else {
                if trailing_slash && t != NF4DIR {
                    return Err(Errno::ENOTDIR);
                }
                return Ok(child);
            }
        }
        Ok(dir)
    }

    /// Split into (parent path, basename).
    fn split(path: &str) -> (&str, &str) {
        let p = path.trim_end_matches('/');
        match p.rfind('/') {
            Some(i) => (&p[..i.max(1)], &p[i + 1..]),
            None => ("/", p),
        }
    }

    fn stat(&mut self, path: &str) -> Result<Stat, Errno> {
        let ino = self.resolve(path, true)?;
        self.stat_ino(ino)
    }

    fn lstat(&mut self, path: &str) -> Result<Stat, Errno> {
        let ino = self.resolve(path, false)?;
        self.stat_ino(ino)
    }

    fn stat_ino(&mut self, ino: u64) -> Result<Stat, Errno> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.getattr(&[FATTR4_TYPE, FATTR4_SIZE, FATTR4_NUMLINKS, FATTR4_MODE]);
        let (st, res) = self.c.call(b"px-stat", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        match &res[1] {
            Reply::Attrs(a) => Ok(Stat {
                ftype: common::attr_u32(a, FATTR4_TYPE),
                size: common::attr_u64(a, FATTR4_SIZE),
                nlink: common::attr_u32(a, FATTR4_NUMLINKS),
                mode: common::attr_u32(a, FATTR4_MODE),
            }),
            r => panic!("px-stat: unexpected reply {r:?}"),
        }
    }

    /// open(path, O_CREAT): NFS OPEN+CREATE (UNCHECKED4) — creates if
    /// missing, else opens existing. (NFSv4 has no CREATE for regular
    /// files; the server rejects CREATE(NF4REG) with BADTYPE.)
    fn creat(&mut self, path: &str) -> Result<u64, Errno> {
        let (parent, name) = Self::split(path);
        if name.is_empty() || name.len() > 255 {
            return Err(Errno::ENAMETOOLONG);
        }
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.open_create(self.clientid, b"px", 3, name.as_bytes(), 0o644);
        ops.getfh();
        let (st, res) = self.c.call(b"px-creat", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        match &res[2] {
            Reply::Fh(fh) => Ok(fh.inode),
            r => panic!("px-creat: unexpected reply {r:?}"),
        }
    }

    /// open(path, O_CREAT|O_EXCL): NFS OPEN+CREATE (GUARDED4).
    fn creat_excl(&mut self, path: &str) -> Result<u64, Errno> {
        let (parent, name) = Self::split(path);
        if name.is_empty() || name.len() > 255 {
            return Err(Errno::ENAMETOOLONG);
        }
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.open(
            self.clientid,
            b"px",
            3,
            OPEN4_CREATE,
            GUARDED4,
            &[(FATTR4_MODE, common::av_u32(0o644))],
            0,
            name.as_bytes(),
        );
        ops.getfh();
        let (st, res) = self.c.call(b"px-creat-excl", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        match &res[2] {
            Reply::Fh(fh) => Ok(fh.inode),
            r => panic!("px-creat-excl: unexpected reply {r:?}"),
        }
    }

    /// open(path, O_NOFOLLOW): ELOOP if the final component is a symlink.
    fn open_nofollow(&mut self, path: &str) -> Result<u64, Errno> {
        let ino = self.resolve(path, false)?;
        if self.stat_ino(ino)?.ftype == NF4LNK {
            return Err(Errno::ELOOP);
        }
        Ok(ino)
    }

    fn readlink(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
        let ino = self.resolve(path, false)?;
        if self.stat_ino(ino)?.ftype != NF4LNK {
            return Err(Errno::EINVAL);
        }
        self.read_target(ino).map_err(nfs_err)
    }

    fn read(&mut self, ino: u64, len: u32) -> Result<Vec<u8>, Errno> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.read(0, len);
        let (st, res) = self.c.call(b"px-read", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        match &res[1] {
            Reply::Read { data, .. } => Ok(data.clone()),
            r => panic!("px-read: unexpected reply {r:?}"),
        }
    }

    fn write(&mut self, ino: u64, data: &[u8]) -> Result<(), Errno> {
        self.write_at(ino, 0, data)
    }

    fn write_at(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.write(offset, FILE_SYNC4, data);
        let (st, _) = self.c.call(b"px-write", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn truncate(&mut self, path: &str, size: u64) -> Result<(), Errno> {
        let ino = self.resolve(path, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ino);
        ops.setattr(&[(FATTR4_SIZE, common::av_u64(size))]);
        let (st, _) = self.c.call(b"px-trunc", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn mkdir(&mut self, path: &str) -> Result<(), Errno> {
        let (parent, name) = Self::split(path);
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.create_dir(name.as_bytes(), 0o755);
        let (st, _) = self.c.call(b"px-mkdir", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn symlink(&mut self, target: &str, path: &str) -> Result<(), Errno> {
        let (parent, name) = Self::split(path);
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.create_symlink(name.as_bytes(), target.as_bytes());
        let (st, _) = self.c.call(b"px-symlink", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn unlink(&mut self, path: &str) -> Result<(), Errno> {
        // Kernel-client behavior: unlink() stats first and returns EISDIR
        // for directories without sending REMOVE (NFS REMOVE itself
        // removes empty dirs, i.e. it is the rmdir path too).
        let ino = self.resolve(path, false)?;
        if self.stat_ino(ino)?.ftype == NF4DIR {
            return Err(Errno::EISDIR);
        }
        let (parent, name) = Self::split(path);
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.remove(name.as_bytes());
        let (st, _) = self.c.call(b"px-unlink", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn rmdir(&mut self, path: &str) -> Result<(), Errno> {
        // Kernel-client behavior: rmdir() on a non-directory is ENOTDIR.
        let ino = self.resolve(path, false)?;
        if self.stat_ino(ino)?.ftype != NF4DIR {
            return Err(Errno::ENOTDIR);
        }
        let (parent, name) = Self::split(path);
        let dir = self.resolve(parent, true)?;
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.remove(name.as_bytes());
        let (st, _) = self.c.call(b"px-rmdir", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn link(&mut self, old: &str, new: &str) -> Result<(), Errno> {
        let src = self.resolve(old, true)?;
        let (parent, name) = Self::split(new);
        let dir = self.resolve(parent, true)?;
        // Server: cfh = existing file, saved_fh = dest dir.
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, dir);
        ops.savefh();
        ops.putfh(&self.uuid, src);
        ops.link(name.as_bytes());
        let (st, _) = self.c.call(b"px-link", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), Errno> {
        let (oparent, oname) = Self::split(old);
        let (nparent, nname) = Self::split(new);
        let odir = self.resolve(oparent, true)?;
        let ndir = self.resolve(nparent, true)?;
        // Server: cfh = source dir, saved_fh = dest dir.
        let mut ops = Ops::new();
        ops.putfh(&self.uuid, ndir);
        ops.savefh();
        ops.putfh(&self.uuid, odir);
        ops.rename(oname.as_bytes(), nname.as_bytes());
        let (st, _) = self.c.call(b"px-rename", ops);
        if st != NFS4_OK {
            return Err(nfs_err(st));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

macro_rules! px {
    ($name:ident) => {
        let srv = spawn_server(4096);
        let mut c = NfsClient::connect(&srv.addr);
        let cid = establish_client(&mut c, b"t_posix");
        let mut $name = Px::new(&mut c, srv.uuid, cid);
    };
}

#[test]
fn follow_symlink_on_open() {
    px!(px);
    let f = px.creat("/target").unwrap();
    px.write(f, b"hello").unwrap();
    px.symlink("/target", "/link").unwrap();
    // open("/link") follows: write via the link lands in the target.
    let via_link = px.resolve("/link", true).unwrap();
    px.write_at(via_link, 5, b" world").unwrap();
    assert_eq!(px.read(f, 64).unwrap(), b"hello world");
    // The link itself is untouched.
    assert_eq!(px.readlink("/link").unwrap(), b"/target");
}

#[test]
fn stat_vs_lstat() {
    px!(px);
    px.creat("/f").unwrap();
    px.symlink("/f", "/l").unwrap();
    assert_eq!(px.stat("/l").unwrap().ftype, NF4REG);
    assert_eq!(px.lstat("/l").unwrap().ftype, NF4LNK);
    assert_eq!(px.lstat("/l").unwrap().size as usize, b"/f".len());
}

#[test]
fn open_nofollow_eloop() {
    px!(px);
    px.creat("/f").unwrap();
    px.symlink("/f", "/l").unwrap();
    assert_eq!(px.open_nofollow("/l"), Err(Errno::ELOOP));
    // ...but on a regular file it succeeds.
    assert!(px.open_nofollow("/f").is_ok());
}

#[test]
fn symlink_loop_eloop() {
    px!(px);
    px.symlink("/b", "/a").unwrap();
    px.symlink("/a", "/b").unwrap();
    assert_eq!(px.resolve("/a", true), Err(Errno::ELOOP));
}

#[test]
fn symlink_chain_relative_and_absolute() {
    px!(px);
    let f = px.creat("/f").unwrap();
    px.write(f, b"chained").unwrap();
    px.symlink("f", "/l1").unwrap(); // relative
    px.symlink("/l1", "/l2").unwrap(); // absolute
    let via = px.resolve("/l2", true).unwrap();
    assert_eq!(via, f);
    assert_eq!(px.read(via, 64).unwrap(), b"chained");
}

#[test]
fn readlink_ok_and_einval() {
    px!(px);
    px.creat("/f").unwrap();
    px.symlink("some/where", "/l").unwrap();
    assert_eq!(px.readlink("/l").unwrap(), b"some/where");
    assert_eq!(px.readlink("/f"), Err(Errno::EINVAL));
    assert_eq!(px.readlink("/nope"), Err(Errno::ENOENT));
}

#[test]
fn trailing_slash_requires_dir() {
    px!(px);
    px.creat("/f").unwrap();
    px.mkdir("/d").unwrap();
    assert_eq!(px.resolve("/f/", true), Err(Errno::ENOTDIR));
    assert!(px.resolve("/d/", true).is_ok());
}

#[test]
fn dotdot_resolves() {
    px!(px);
    px.mkdir("/d1").unwrap();
    px.mkdir("/d1/d2").unwrap();
    let f = px.creat("/d1/f").unwrap();
    assert_eq!(px.resolve("/d1/d2/../f", true).unwrap(), f);
    assert_eq!(px.resolve("/d1/d2/../..", true).unwrap(), ROOT_INO);
}

#[test]
fn creat_excl_eexist() {
    px!(px);
    px.creat("/f").unwrap();
    assert_eq!(px.stat("/f").unwrap().mode & 0o777, 0o644);
    assert!(px.creat_excl("/f2").is_ok());
    assert_eq!(px.creat_excl("/f2"), Err(Errno::EEXIST));
    // O_CREAT without EXCL on an existing file just opens it.
    let a = px.creat("/f").unwrap();
    let b = px.creat("/f").unwrap();
    assert_eq!(a, b);
}

#[test]
fn o_trunc() {
    px!(px);
    let f = px.creat("/f").unwrap();
    px.write(f, b"12345678").unwrap();
    assert_eq!(px.stat("/f").unwrap().size, 8);
    px.truncate("/f", 0).unwrap();
    assert_eq!(px.stat("/f").unwrap().size, 0);
    assert!(px.read(f, 64).unwrap().is_empty());
}

#[test]
fn name_too_long() {
    px!(px);
    let long = format!("/{}", "x".repeat(256));
    assert_eq!(px.creat(&long), Err(Errno::ENAMETOOLONG));
    assert_eq!(px.resolve(&long, true), Err(Errno::ENAMETOOLONG));
}

#[test]
fn mkdir_rmdir_edges() {
    px!(px);
    px.mkdir("/d").unwrap();
    assert_eq!(px.mkdir("/d"), Err(Errno::EEXIST));
    px.creat("/d/f").unwrap();
    // rmdir on non-empty dir: POSIX ENOTEMPTY (server: NOTSUPP, no v4.0 code).
    assert_eq!(px.rmdir("/d"), Err(Errno::ENOTEMPTY));
    assert_eq!(px.rmdir("/nope"), Err(Errno::ENOENT));
    assert_eq!(px.rmdir("/d/f"), Err(Errno::ENOTDIR));
    px.unlink("/d/f").unwrap();
    px.rmdir("/d").unwrap();
    assert_eq!(px.resolve("/d", true), Err(Errno::ENOENT));
}

#[test]
fn unlink_dir_is_eisdir() {
    px!(px);
    px.mkdir("/d").unwrap();
    assert_eq!(px.unlink("/d"), Err(Errno::EISDIR));
    // Unlinking a missing name is ENOENT.
    assert_eq!(px.unlink("/nope"), Err(Errno::ENOENT));
}

#[test]
fn hardlink_nlink() {
    px!(px);
    let f = px.creat("/f").unwrap();
    px.write(f, b"shared").unwrap();
    px.link("/f", "/g").unwrap();
    assert_eq!(px.stat("/f").unwrap().nlink, 2);
    // Both names read the same data.
    let g = px.resolve("/g", true).unwrap();
    assert_eq!(g, f);
    assert_eq!(px.read(g, 64).unwrap(), b"shared");
    // Unlink one name; the other survives.
    px.unlink("/f").unwrap();
    assert_eq!(px.stat("/g").unwrap().nlink, 1);
    assert_eq!(px.read(g, 64).unwrap(), b"shared");
    // Linking a directory fails (POSIX EPERM; server INVAL).
    px.mkdir("/d").unwrap();
    assert_eq!(px.link("/d", "/dlink"), Err(Errno::EINVAL));
}

#[test]
fn rename_replace_file() {
    px!(px);
    let a = px.creat("/a").unwrap();
    px.write(a, b"A").unwrap();
    let b = px.creat("/b").unwrap();
    px.write(b, b"B").unwrap();
    px.rename("/a", "/b").unwrap();
    // /b now has A's data; /a is gone.
    assert_eq!(px.resolve("/b", true).unwrap(), a);
    assert_eq!(px.read(a, 64).unwrap(), b"A");
    assert_eq!(px.resolve("/a", true), Err(Errno::ENOENT));
}

#[test]
fn rename_dir_onto_empty_dir() {
    px!(px);
    px.mkdir("/d1").unwrap();
    px.mkdir("/d2").unwrap();
    let f = px.creat("/d1/f").unwrap();
    px.rename("/d1", "/d2").unwrap();
    assert_eq!(px.resolve("/d2/f", true).unwrap(), f);
    assert_eq!(px.resolve("/d1", true), Err(Errno::ENOENT));
}

#[test]
fn rename_onto_nonempty_dir_fails() {
    px!(px);
    px.mkdir("/d1").unwrap();
    px.mkdir("/d2").unwrap();
    px.creat("/d2/f").unwrap();
    assert_eq!(px.rename("/d1", "/d2"), Err(Errno::ENOTEMPTY));
    // Both still intact.
    assert!(px.resolve("/d1", true).is_ok());
    assert!(px.resolve("/d2/f", true).is_ok());
}

#[test]
fn rename_missing_enoent() {
    px!(px);
    assert_eq!(px.rename("/nope", "/x"), Err(Errno::ENOENT));
}

#[test]
fn rename_into_own_subtree_fails() {
    px!(px);
    px.mkdir("/d").unwrap();
    px.mkdir("/d/sub").unwrap();
    assert_eq!(px.rename("/d", "/d/sub/moved"), Err(Errno::EINVAL));
    assert!(px.resolve("/d/sub", true).is_ok());
}

#[test]
fn write_read_roundtrip_sizes() {
    px!(px);
    let f = px.creat("/f").unwrap();
    // Sparse write: offset beyond EOF.
    px.write_at(f, 8192, b"tail").unwrap();
    let st = px.stat("/f").unwrap();
    assert_eq!(st.size, 8196);
    let d = px.read(f, 9000).unwrap();
    assert_eq!(&d[8192..], b"tail");
    assert!(d[..8192].iter().all(|&b| b == 0));
}
