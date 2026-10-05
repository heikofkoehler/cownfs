//! cownfs-backup: snapshot backup and restore to/from portable files.
//!
//!   cownfs-backup create <image> <backup-file>          - export full image to backup
//!   cownfs-backup create-inc <image> <snap> <backup-file> - incremental since snapshot
//!   cownfs-backup restore <backup-file> <image>           - import backup to new image
//!   cownfs-backup restore-inc <image> <backup-file>        - apply incremental to image
//!   cownfs-backup verify <backup-file>                    - check backup integrity
//!   cownfs-backup list <backup-file>                      - show backup metadata
//!
//! Format: file-based, portable, checksummed. v1=full, v3=incremental.
//! (v2 was a pre-release incremental format without base roots; v3 adds
//! base/new roots, lens, and allocator state for exact-base restore.)

use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::checksum::checksum32;
use cownfs_core::engine::Fs;

const MAGIC: u64 = 0x434f574e42554b50; // "COWNBUKP"
const VERSION: u32 = 1;
const INC_VERSION: u32 = 3;
const BLOCK_SIZE: usize = 4096;

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  cownfs-backup create <image> <backup-file>");
    eprintln!("  cownfs-backup create-inc <image> <snap> <backup-file>");
    eprintln!("  cownfs-backup restore <backup-file> <image>");
    eprintln!("  cownfs-backup restore-inc <image> <backup-file>");
    eprintln!("  cownfs-backup verify <backup-file>");
    eprintln!("  cownfs-backup list <backup-file>");
    std::process::exit(1);
}

fn write_u64(f: &mut File, v: u64) -> std::io::Result<()> {
    f.write_all(&v.to_le_bytes())
}
fn write_u32(f: &mut File, v: u32) -> std::io::Result<()> {
    f.write_all(&v.to_le_bytes())
}
fn read_u64(f: &mut File) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_u32(f: &mut File) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn do_create(image: &Path, backup: &Path) -> Result<(), String> {
    let fs = Fs::open(image).map_err(|e| format!("open image: {e:?}"))?;
    let dev = FileDevice::open(image).map_err(|e| format!("open device: {e}"))?;

    let mut out = File::create(backup).map_err(|e| format!("create backup: {e}"))?;

    // Header.
    write_u64(&mut out, MAGIC).map_err(|e| e.to_string())?;
    write_u32(&mut out, VERSION).map_err(|e| e.to_string())?;
    let uuid = fs.uuid();
    out.write_all(&uuid).map_err(|e| e.to_string())?;
    write_u64(&mut out, fs.generation()).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_u64(&mut out, now).map_err(|e| e.to_string())?;

    // All allocated blocks.
    let blocks: Vec<u64> = {
        let mut v: Vec<u64> = fs.allocated_blocks().into_iter().collect();
        v.sort_unstable();
        v
    };
    write_u64(&mut out, blocks.len() as u64).map_err(|e| e.to_string())?;

    let mut buf = [0u8; BLOCK_SIZE];
    let mut count = 0;
    for &blk in &blocks {
        dev.read_block(blk, &mut buf)
            .map_err(|e| format!("read block {blk}: {e}"))?;
        let cksum = checksum32(&buf);
        write_u64(&mut out, blk).map_err(|e| e.to_string())?;
        write_u32(&mut out, cksum).map_err(|e| e.to_string())?;
        out.write_all(&buf).map_err(|e| e.to_string())?;
        count += 1;
        if count % 1000 == 0 {
            eprintln!("  {count}/{} blocks", blocks.len());
        }
    }
    out.flush().map_err(|e| e.to_string())?;
    println!("Backup complete: {count} blocks");
    Ok(())
}

fn read_header(f: &mut File) -> Result<([u8; 16], u64, u64), String> {
    let (uuid, gen, ts, _ver, _snap) = read_header_full(f)?;
    Ok((uuid, gen, ts))
}

/// Full header read: returns (uuid, generation, timestamp, version,
/// snapshot name if v2/v3).
fn read_header_full(f: &mut File) -> Result<([u8; 16], u64, u64, u32, Option<[u8; 64]>), String> {
    let magic = read_u64(f).map_err(|e| format!("read magic: {e}"))?;
    if magic != MAGIC {
        return Err("not a cownfs backup file".into());
    }
    let ver = read_u32(f).map_err(|e| format!("read version: {e}"))?;
    if ver != VERSION && ver != 2 && ver != INC_VERSION {
        return Err(format!("unsupported backup version {ver}"));
    }
    let mut uuid = [0u8; 16];
    f.read_exact(&mut uuid)
        .map_err(|e| format!("read uuid: {e}"))?;
    let gen = read_u64(f).map_err(|e| format!("read gen: {e}"))?;
    let ts = read_u64(f).map_err(|e| format!("read ts: {e}"))?;
    let snap = if ver == VERSION {
        None
    } else {
        let mut name = [0u8; 64];
        f.read_exact(&mut name)
            .map_err(|e| format!("read snap name: {e}"))?;
        Some(name)
    };
    Ok((uuid, gen, ts, ver, snap))
}

fn snap_name_str(raw: &[u8; 64]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(64);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

fn do_verify(backup: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open: {e}"))?;
    let (_uuid, gen, ts, ver, _snap) = read_header_full(&mut f)?;
    // Skip v3 incremental-specific header fields.
    if ver == INC_VERSION {
        let mut skip = [0u8; 48 + 48 + 32 + 8 + 8];
        f.read_exact(&mut skip)
            .map_err(|e| format!("read v3 header: {e}"))?;
    }
    let nblocks = read_u64(&mut f).map_err(|e| format!("read count: {e}"))?;

    let mut buf = [0u8; BLOCK_SIZE];
    for i in 0..nblocks {
        let blk = read_u64(&mut f).map_err(|e| format!("read blk {i}: {e}"))?;
        let cksum = read_u32(&mut f).map_err(|e| format!("read cksum {i}: {e}"))?;
        f.read_exact(&mut buf)
            .map_err(|e| format!("read data {i}: {e}"))?;
        let actual = checksum32(&buf);
        if actual != cksum {
            return Err(format!("block {blk} checksum mismatch"));
        }
    }
    println!("Backup OK: {nblocks} blocks, gen {gen}, ts {ts}");
    Ok(())
}

fn do_list(backup: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open: {e}"))?;
    let (_uuid, gen, ts, ver, snap) = read_header_full(&mut f)?;
    if ver == INC_VERSION {
        let mut skip = [0u8; 48 + 48 + 32 + 8 + 8];
        f.read_exact(&mut skip)
            .map_err(|e| format!("read v3 header: {e}"))?;
    }
    let nblocks = read_u64(&mut f).map_err(|e| format!("read count: {e}"))?;
    println!("version: {ver}");
    println!("generation: {gen}");
    println!("created: {ts}");
    if let Some(name) = snap {
        println!("base snapshot: {}", snap_name_str(&name));
    }
    println!("blocks: {nblocks}");
    println!("size: ~{} MiB", nblocks * 4096 / 1024 / 1024);
    Ok(())
}

fn do_restore(backup: &Path, image: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open backup: {e}"))?;
    let (uuid, _gen, _) = read_header(&mut f)?;
    let nblocks = read_u64(&mut f).map_err(|e| format!("read count: {e}"))?;

    // Determine image size from max block.
    // We need to read all blocks first to find the max, or just
    // create a large enough image. Simpler: read into memory then write.
    let mut blocks: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::with_capacity(nblocks as usize);
    let mut max_blk = 0u64;
    let mut buf = [0u8; BLOCK_SIZE];
    for i in 0..nblocks {
        let blk = read_u64(&mut f).map_err(|e| format!("read blk {i}: {e}"))?;
        let cksum = read_u32(&mut f).map_err(|e| format!("read cksum {i}: {e}"))?;
        f.read_exact(&mut buf)
            .map_err(|e| format!("read data {i}: {e}"))?;
        if checksum32(&buf) != cksum {
            return Err(format!("backup corrupt at block {blk}"));
        }
        max_blk = max_blk.max(blk);
        let mut data = [0u8; BLOCK_SIZE];
        data.copy_from_slice(&buf);
        blocks.push((blk, data));
    }

    // Create image sized to fit. Round up to a reasonable size.
    let nblocks_img = (max_blk + 1).next_power_of_two().max(256);
    if image.exists() {
        return Err("target image exists (refusing to overwrite)".into());
    }
    let fs = Fs::format(image, nblocks_img).map_err(|e| format!("format: {e:?}"))?;
    // The format created a fresh FS; we need to overwrite with backup blocks.
    // Instead, write blocks directly via the device, then verify superblock.
    drop(fs);

    let dev = FileDevice::open(image).map_err(|e| format!("open: {e}"))?;
    for (blk, data) in &blocks {
        dev.write_block(*blk, data)
            .map_err(|e| format!("write block {blk}: {e}"))?;
    }
    dev.sync().map_err(|e| format!("sync: {e}"))?;

    // Verify the restored image opens and has the right UUID.
    let fs = Fs::open(image).map_err(|e| format!("verify open: {e:?}"))?;
    if fs.uuid() != uuid {
        return Err("restored UUID mismatch".into());
    }
    // Run a quick fsck.
    fs.check().map_err(|e| format!("check: {e:?}"))?;

    println!("Restore complete: {nblocks} blocks to {}", image.display());
    Ok(())
}

/// B5: Create an incremental backup — only blocks changed since the named
/// snapshot. Format v2: same as v1 plus base snapshot name.
fn do_create_inc(image: &Path, snap_name: &str, backup: &Path) -> Result<(), String> {
    let mut fs = Fs::open(image).map_err(|e| format!("open image: {e:?}"))?;

    // Find the snapshot by name.
    let snaps = fs
        .snapshot_list()
        .map_err(|e| format!("list snaps: {e:?}"))?;
    let snap_id = snaps
        .iter()
        .find(|(_, name)| name == snap_name.as_bytes())
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("snapshot '{snap_name}' not found"))?;

    // Diff snapshot roots against current.
    let old_roots = fs
        .snapshot_roots(snap_id)
        .map_err(|e| format!("get snap roots: {e:?}"))?;
    let mut changed = fs
        .diff_roots(&old_roots)
        .map_err(|e| format!("diff: {e:?}"))?;
    // diff_roots skips the snap tree (snapshot_roots reports it as the
    // current root), but the target needs the new snap-tree nodes if
    // snapshots were added since the base.
    for blk in fs
        .snap_tree_blocks()
        .map_err(|e| format!("snap tree: {e:?}"))?
    {
        changed.push(blk);
    }
    changed.sort_unstable();
    changed.dedup();

    // Deleted blocks: reachable at snapshot time, not reachable now.
    let mut deleted = fs
        .deleted_blocks(&old_roots)
        .map_err(|e| format!("deleted: {e:?}"))?;
    deleted.sort_unstable();
    deleted.dedup();

    // Current roots and superblock metadata for restore.
    let new_roots = fs.roots();
    let new_gen = fs.generation();
    let uuid = fs.uuid();
    drop(fs);

    // Read superblock for tree lengths and allocator state.
    let dev = FileDevice::open(image).map_err(|e| format!("open device: {e}"))?;
    let (sb, _) =
        cownfs_core::superblock::open(&dev).map_err(|e| format!("read superblock: {e}"))?;

    let mut out = File::create(backup).map_err(|e| format!("create backup: {e}"))?;

    // Header (v3).
    write_u64(&mut out, MAGIC).map_err(|e| e.to_string())?;
    write_u32(&mut out, 3).map_err(|e| e.to_string())?; // VERSION 3
    out.write_all(&uuid).map_err(|e| e.to_string())?;
    write_u64(&mut out, new_gen).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_u64(&mut out, now).map_err(|e| e.to_string())?;
    // Base snapshot name (null-padded to 64 bytes).
    let mut snap_buf = [0u8; 64];
    let name_bytes = snap_name.as_bytes();
    let n = name_bytes.len().min(64);
    snap_buf[..n].copy_from_slice(&name_bytes[..n]);
    out.write_all(&snap_buf).map_err(|e| e.to_string())?;

    // Base roots (for exact-base verification): 4x (idx u64, gen u32).
    for r in [
        old_roots.inode,
        old_roots.dir,
        old_roots.extent,
        old_roots.snap,
    ] {
        write_u64(&mut out, r.idx).map_err(|e| e.to_string())?;
        write_u32(&mut out, r.gen).map_err(|e| e.to_string())?;
    }
    // New roots: 4x (idx u64, gen u32).
    for r in [
        new_roots.inode,
        new_roots.dir,
        new_roots.extent,
        new_roots.snap,
    ] {
        write_u64(&mut out, r.idx).map_err(|e| e.to_string())?;
        write_u32(&mut out, r.gen).map_err(|e| e.to_string())?;
    }
    // Tree lengths: 4x u64.
    write_u64(&mut out, sb.inode_len).map_err(|e| e.to_string())?;
    write_u64(&mut out, sb.dir_len).map_err(|e| e.to_string())?;
    write_u64(&mut out, sb.extent_len).map_err(|e| e.to_string())?;
    write_u64(&mut out, sb.snap_len).map_err(|e| e.to_string())?;
    // Allocator state.
    write_u64(&mut out, sb.next_inode).map_err(|e| e.to_string())?;
    write_u64(&mut out, sb.next_snap).map_err(|e| e.to_string())?;

    // Changed blocks.
    write_u64(&mut out, changed.len() as u64).map_err(|e| e.to_string())?;
    let mut buf = [0u8; BLOCK_SIZE];
    for &blk in &changed {
        dev.read_block(blk, &mut buf)
            .map_err(|e| format!("read block {blk}: {e}"))?;
        let cksum = checksum32(&buf);
        write_u64(&mut out, blk).map_err(|e| e.to_string())?;
        write_u32(&mut out, cksum).map_err(|e| e.to_string())?;
        out.write_all(&buf).map_err(|e| e.to_string())?;
    }

    // Deleted blocks.
    write_u64(&mut out, deleted.len() as u64).map_err(|e| e.to_string())?;
    for &blk in &deleted {
        write_u64(&mut out, blk).map_err(|e| e.to_string())?;
    }

    println!(
        "Incremental backup complete: {} changed, {} deleted blocks (since '{}', gen {new_gen}) to {}",
        changed.len(),
        deleted.len(),
        snap_name,
        backup.display()
    );
    Ok(())
}

/// B5: Restore an incremental backup (v3) onto the exact base image.
///
/// Verifies (exactly): backup UUID == target UUID, target contains a
/// snapshot with the exact recorded name whose data-tree roots exactly
/// match the recorded base roots, and target generation < backup
/// generation. Then applies changed blocks, frees deleted blocks,
/// rebuilds the bitmap (with CRCs), and advances the superblock to the
/// recorded new generation/roots.
fn do_restore_inc(image: &Path, backup: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open backup: {e}"))?;

    let magic = read_u64(&mut f).map_err(|e| format!("read magic: {e}"))?;
    if magic != MAGIC {
        return Err("not a cownfs backup file".into());
    }
    let ver = read_u32(&mut f).map_err(|e| format!("read version: {e}"))?;
    if ver != 3 {
        return Err(format!(
            "restore-inc requires a v3 incremental backup (got v{ver})"
        ));
    }
    let mut uuid = [0u8; 16];
    f.read_exact(&mut uuid)
        .map_err(|e| format!("read uuid: {e}"))?;
    let new_gen = read_u64(&mut f).map_err(|e| format!("read new_gen: {e}"))?;
    let _ts = read_u64(&mut f).map_err(|e| format!("read ts: {e}"))?;
    let mut snap_name = [0u8; 64];
    f.read_exact(&mut snap_name)
        .map_err(|e| format!("read snap name: {e}"))?;

    let mut base_roots = Vec::new();
    for _ in 0..4 {
        let idx = read_u64(&mut f).map_err(|e| format!("read base root: {e}"))?;
        let gen = read_u32(&mut f).map_err(|e| format!("read base root gen: {e}"))?;
        base_roots.push((idx, gen));
    }
    let mut new_roots = Vec::new();
    for _ in 0..4 {
        let idx = read_u64(&mut f).map_err(|e| format!("read new root: {e}"))?;
        let gen = read_u32(&mut f).map_err(|e| format!("read new root gen: {e}"))?;
        new_roots.push((idx, gen));
    }
    let mut new_lens = Vec::new();
    for _ in 0..4 {
        new_lens.push(read_u64(&mut f).map_err(|e| format!("read len: {e}"))?);
    }
    let next_inode = read_u64(&mut f).map_err(|e| format!("read next_inode: {e}"))?;
    let next_snap = read_u64(&mut f).map_err(|e| format!("read next_snap: {e}"))?;

    let n_changed = read_u64(&mut f).map_err(|e| format!("read n_changed: {e}"))?;
    if n_changed > 10_000_000 {
        return Err("absurd changed-block count".into());
    }
    let mut changed: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::with_capacity(n_changed as usize);
    let mut buf = [0u8; BLOCK_SIZE];
    for i in 0..n_changed {
        let blk = read_u64(&mut f).map_err(|e| format!("read blk {i}: {e}"))?;
        let cksum = read_u32(&mut f).map_err(|e| format!("read cksum {i}: {e}"))?;
        f.read_exact(&mut buf)
            .map_err(|e| format!("read data {i}: {e}"))?;
        if checksum32(&buf) != cksum {
            return Err(format!("backup corrupt at block {blk} (checksum mismatch)"));
        }
        let mut data = [0u8; BLOCK_SIZE];
        data.copy_from_slice(&buf);
        changed.push((blk, data));
    }

    let n_deleted = read_u64(&mut f).map_err(|e| format!("read n_deleted: {e}"))?;
    if n_deleted > 10_000_000 {
        return Err("absurd deleted-block count".into());
    }
    let mut deleted = Vec::with_capacity(n_deleted as usize);
    for i in 0..n_deleted {
        deleted.push(read_u64(&mut f).map_err(|e| format!("read deleted {i}: {e}"))?);
    }

    // --- Verify target is the exact base. ---
    let fs = Fs::open(image).map_err(|e| format!("open image: {e:?}"))?;
    if fs.uuid() != uuid {
        return Err("UUID mismatch: backup is for a different image".into());
    }
    let target_gen = fs.generation();
    if target_gen >= new_gen {
        return Err(format!(
            "target generation {target_gen} is not older than backup generation {new_gen}"
        ));
    }
    let snap_name_bytes: Vec<u8> = snap_name.iter().take_while(|&&b| b != 0).copied().collect();
    let snaps = fs
        .snapshot_list()
        .map_err(|e| format!("list snaps: {e:?}"))?;
    let snap_id = snaps
        .iter()
        .find(|(_, name)| *name == snap_name_bytes)
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            format!(
                "snapshot '{}' not found in target image",
                String::from_utf8_lossy(&snap_name_bytes)
            )
        })?;
    let target_roots = fs
        .snapshot_roots(snap_id)
        .map_err(|e| format!("get snap roots: {e:?}"))?;
    // Compare data-tree roots only (index 3 is the snap tree, which
    // legitimately changes as new snapshots are created).
    let target_tuples = [
        (target_roots.inode.idx, target_roots.inode.gen),
        (target_roots.dir.idx, target_roots.dir.gen),
        (target_roots.extent.idx, target_roots.extent.gen),
    ];
    for (i, ((ti, tg), (bi, bg))) in target_tuples.iter().zip(base_roots.iter()).enumerate() {
        if ti != bi || tg != bg {
            return Err(format!(
                "snapshot root {i} mismatch: target is not the exact base state"
            ));
        }
    }
    // The target's live state must also be exactly at the base; applying
    // the diff to a diverged live state would corrupt the image.
    // (Compare data-tree roots only; the snap tree legitimately changes
    // as new snapshots are created.)
    let live = fs.roots();
    let live_tuples = [
        (live.inode.idx, live.inode.gen),
        (live.dir.idx, live.dir.gen),
        (live.extent.idx, live.extent.gen),
    ];
    for (i, ((li, lg), (bi, bg))) in live_tuples.iter().zip(base_roots.iter()).enumerate() {
        if li != bi || lg != bg {
            return Err(format!(
                "target live root {i} diverged from base snapshot (exact-base-only)"
            ));
        }
    }

    // Build FsRoots from the backup header.
    let mk_roots = |v: &[(u64, u32)]| cownfs_core::engine::FsRoots {
        inode: cownfs_core::btree::NodeId {
            idx: v[0].0,
            gen: v[0].1,
        },
        dir: cownfs_core::btree::NodeId {
            idx: v[1].0,
            gen: v[1].1,
        },
        extent: cownfs_core::btree::NodeId {
            idx: v[2].0,
            gen: v[2].1,
        },
        snap: cownfs_core::btree::NodeId {
            idx: v[3].0,
            gen: v[3].1,
        },
    };
    let base = mk_roots(&base_roots);
    let new = mk_roots(&new_roots);
    let lens = [new_lens[0], new_lens[1], new_lens[2], new_lens[3]];

    // Apply: writes changed blocks, rebuilds the bitmap from the new
    // reachable set, persists it with CRCs, and advances the superblock.
    // The deleted-block list in the backup is informational; the bitmap
    // is rebuilt from scratch so it cannot go stale.
    let mut fs = fs;
    fs.apply_incremental(&base, &new, lens, next_inode, next_snap, &changed)
        .map_err(|e| format!("apply: {e:?}"))?;
    drop(fs);

    // --- Verify. ---
    let fs = Fs::open(image).map_err(|e| format!("verify open: {e:?}"))?;
    if fs.uuid() != uuid {
        return Err("UUID changed after restore".into());
    }
    if fs.generation() != target_gen + 1 {
        return Err(format!(
            "generation {} != expected {} after restore",
            fs.generation(),
            target_gen + 1
        ));
    }
    fs.check().map_err(|e| format!("check: {e:?}"))?;

    println!(
        "Incremental restore complete: {} changed blocks (gen {target_gen} -> {})",
        changed.len(),
        fs.generation()
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "create" if args.len() == 4 => do_create(Path::new(&args[2]), Path::new(&args[3])),
        "create-inc" if args.len() == 5 => {
            do_create_inc(Path::new(&args[2]), &args[3], Path::new(&args[4]))
        }
        "restore" if args.len() == 4 => do_restore(Path::new(&args[2]), Path::new(&args[3])),
        "restore-inc" if args.len() == 4 => {
            do_restore_inc(Path::new(&args[2]), Path::new(&args[3]))
        }
        "verify" if args.len() == 3 => do_verify(Path::new(&args[2])),
        "list" if args.len() == 3 => do_list(Path::new(&args[2])),
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
