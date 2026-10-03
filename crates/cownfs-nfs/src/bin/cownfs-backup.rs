//! cownfs-backup: snapshot backup and restore to/from portable files.
//!
//!   cownfs-backup create <image> <backup-file>   - export full image to backup
//!   cownfs-backup restore <backup-file> <image>  - import backup to new image
//!   cownfs-backup verify <backup-file>           - check backup integrity
//!   cownfs-backup list <backup-file>             - show backup metadata
//!
//! Format: file-based, portable, checksummed. Not incremental (v1).

use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::checksum::checksum32;
use cownfs_core::engine::Fs;

const MAGIC: u64 = 0x434f574e42554b50; // "COWNBUKP"
const VERSION: u32 = 1;
const BLOCK_SIZE: usize = 4096;

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  cownfs-backup create <image> <backup-file>");
    eprintln!("  cownfs-backup restore <backup-file> <image>");
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
    let magic = read_u64(f).map_err(|e| format!("read magic: {e}"))?;
    if magic != MAGIC {
        return Err("not a cownfs backup file".into());
    }
    let ver = read_u32(f).map_err(|e| format!("read version: {e}"))?;
    if ver != VERSION {
        return Err(format!("unsupported backup version {ver}"));
    }
    let mut uuid = [0u8; 16];
    f.read_exact(&mut uuid)
        .map_err(|e| format!("read uuid: {e}"))?;
    let gen = read_u64(f).map_err(|e| format!("read gen: {e}"))?;
    let ts = read_u64(f).map_err(|e| format!("read ts: {e}"))?;
    Ok((uuid, gen, ts))
}

fn do_verify(backup: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open: {e}"))?;
    let (uuid, gen, ts) = read_header(&mut f)?;
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
    let (_uuid, gen, ts) = read_header(&mut f)?;
    let nblocks = read_u64(&mut f).map_err(|e| format!("read count: {e}"))?;
    println!("generation: {gen}");
    println!("created: {ts}");
    println!("blocks: {nblocks}");
    println!("size: ~{} MiB", nblocks * 4096 / 1024 / 1024);
    Ok(())
}

fn do_restore(backup: &Path, image: &Path) -> Result<(), String> {
    let mut f = File::open(backup).map_err(|e| format!("open backup: {e}"))?;
    let (uuid, gen, _) = read_header(&mut f)?;
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
    let mut fs = Fs::format(image, nblocks_img).map_err(|e| format!("format: {e:?}"))?;
    // The format created a fresh FS; we need to overwrite with backup blocks.
    // Instead, write blocks directly via the device, then verify superblock.
    drop(fs);

    let dev = FileDevice::open(image).map_err(|e| format!("open: {e}"))?;
    let mut dev = dev;
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

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "create" if args.len() == 4 => do_create(Path::new(&args[2]), Path::new(&args[3])),
        "restore" if args.len() == 4 => do_restore(Path::new(&args[2]), Path::new(&args[3])),
        "verify" if args.len() == 3 => do_verify(Path::new(&args[2])),
        "list" if args.len() == 3 => do_list(Path::new(&args[2])),
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
