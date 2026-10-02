//! cownfs-replicate: ship snapshots from a primary to a read replica.
//!
//! Usage:
//!   cownfs-replicate send <primary-image> <replica-addr> [--state <file>]
//!   cownfs-replicate receive <replica-image> [listen-addr]
//!
//! The sender diffs the primary's current roots against the last-replicated
//! roots (stored in the state file), reads the changed 4KiB blocks, and
//! streams them to the receiver. The receiver writes blocks to its image,
//! verifies node-block checksums, and fsyncs on COMMIT. The superblock
//! slots and active bitmap area are always sent, so the replica's roots,
//! generation, and allocator state advance atomically with the data.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::engine::{Fs, FsRoots};
use cownfs_core::superblock::{self, SLOT_BLOCKS};
use cownfs_core::{Block, BLOCK_SIZE};

const MAGIC: u32 = 0x434F_5752; // "COWR"
const VERSION: u32 = 1;

const MSG_SNAPSHOT: u8 = 1;
const MSG_BLOCK: u8 = 2;
const MSG_COMMIT: u8 = 3;
const MSG_ACK: u8 = 4;
const MSG_ERROR: u8 = 5;

/// Node block magic (from store.rs): used to detect node blocks for
/// checksum verification on receipt.
const NODE_MAGIC: u16 = 0xB72E;

fn read_u32(r: &mut TcpStream) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(r: &mut TcpStream) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn write_u32(w: &mut TcpStream, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn write_u64(w: &mut TcpStream, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

fn read_roots(r: &mut TcpStream) -> std::io::Result<(FsRoots, u64, [u8; 16])> {
    let mut get = || -> std::io::Result<(u64, u32)> {
        let blk = read_u64(r)?;
        let gen = read_u32(r)?;
        Ok((blk, gen))
    };
    let (ib, ig) = get()?;
    let (db, dg) = get()?;
    let (eb, eg) = get()?;
    let (sb, sg) = get()?;
    let generation = read_u64(r)?;
    let mut uuid = [0u8; 16];
    r.read_exact(&mut uuid)?;
    Ok((
        FsRoots {
            inode: cownfs_core::btree::NodeId { idx: ib, gen: ig },
            dir: cownfs_core::btree::NodeId { idx: db, gen: dg },
            extent: cownfs_core::btree::NodeId { idx: eb, gen: eg },
            snap: cownfs_core::btree::NodeId { idx: sb, gen: sg },
        },
        generation,
        uuid,
    ))
}

fn write_roots(
    w: &mut TcpStream,
    roots: &FsRoots,
    generation: u64,
    uuid: &[u8; 16],
) -> std::io::Result<()> {
    for (blk, gen) in [
        (roots.inode.idx, roots.inode.gen),
        (roots.dir.idx, roots.dir.gen),
        (roots.extent.idx, roots.extent.gen),
        (roots.snap.idx, roots.snap.gen),
    ] {
        write_u64(w, blk)?;
        write_u32(w, gen)?;
    }
    write_u64(w, generation)?;
    w.write_all(uuid)
}

fn state_path(image: &Path, override_: Option<&str>) -> PathBuf {
    match override_ {
        Some(p) => PathBuf::from(p),
        None => {
            let mut p = image.as_os_str().to_owned();
            p.push(".repl");
            PathBuf::from(p)
        }
    }
}

fn load_state(path: &Path) -> Option<(FsRoots, u64, [u8; 16])> {
    let data = std::fs::read(path).ok()?;
    if data.len() != 4 * 12 + 8 + 16 {
        return None;
    }
    let mut off = 0;
    let mut get = || {
        let blk = u64::from_be_bytes(data[off..off + 8].try_into().unwrap());
        let gen = u32::from_be_bytes(data[off + 8..off + 12].try_into().unwrap());
        off += 12;
        (blk, gen)
    };
    let (ib, ig) = get();
    let (db, dg) = get();
    let (eb, eg) = get();
    let (sb, sg) = get();
    let generation = u64::from_be_bytes(data[off..off + 8].try_into().unwrap());
    off += 8;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&data[off..off + 16]);
    Some((
        FsRoots {
            inode: cownfs_core::btree::NodeId { idx: ib, gen: ig },
            dir: cownfs_core::btree::NodeId { idx: db, gen: dg },
            extent: cownfs_core::btree::NodeId { idx: eb, gen: eg },
            snap: cownfs_core::btree::NodeId { idx: sb, gen: sg },
        },
        generation,
        uuid,
    ))
}

fn save_state(
    path: &Path,
    roots: &FsRoots,
    generation: u64,
    uuid: &[u8; 16],
) -> std::io::Result<()> {
    let mut data = Vec::with_capacity(4 * 12 + 8 + 16);
    for (blk, gen) in [
        (roots.inode.idx, roots.inode.gen),
        (roots.dir.idx, roots.dir.gen),
        (roots.extent.idx, roots.extent.gen),
        (roots.snap.idx, roots.snap.gen),
    ] {
        data.extend_from_slice(&blk.to_be_bytes());
        data.extend_from_slice(&gen.to_be_bytes());
    }
    data.extend_from_slice(&generation.to_be_bytes());
    data.extend_from_slice(uuid);
    std::fs::write(path, data)
}

fn send_error(s: &mut TcpStream, msg: &str) {
    let _ = write_u32(s, MAGIC);
    let _ = write_u32(s, VERSION);
    let _ = s.write_all(&[MSG_ERROR]);
    let b = msg.as_bytes();
    let _ = write_u32(s, b.len() as u32);
    let _ = s.write_all(b);
}

fn do_send(
    primary: &Path,
    replica_addr: &str,
    state_file: Option<&str>,
) -> Result<(usize, u64), String> {
    let spath = state_path(primary, state_file);
    let mut fs = Fs::open(primary).map_err(|e| format!("open primary: {e:?}"))?;
    let dev = FileDevice::open(primary).map_err(|e| format!("open device: {e}"))?;

    let cur_roots = fs.roots();
    let cur_gen = fs.generation();

    // Determine the block set: incremental diff or full send.
    // Validate the diff base: UUID must match (same primary) and the old
    // generation must not be newer than current (sanity). If validation
    // fails, fall back to a full send rather than a corrupt diff.
    let cur_uuid = fs.uuid();
    let mut blocks: HashSet<u64> = HashSet::new();
    let is_incremental = match load_state(&spath) {
        Some((old_roots, old_gen, old_uuid)) if old_uuid == cur_uuid && old_gen <= cur_gen => {
            let diff = fs
                .diff_roots(&old_roots)
                .map_err(|e| format!("diff: {e:?}"))?;
            blocks.extend(diff);
            true
        }
        Some(_) => {
            eprintln!("stale or divergent state: falling back to full send");
            blocks.extend(fs.allocated_blocks());
            false
        }
        None => {
            blocks.extend(fs.allocated_blocks());
            false
        }
    };

    // Always send superblock slots and the active bitmap area: the
    // replica's roots, generation, and allocator advance with the data.
    let (sb, _) = superblock::open(&dev).map_err(|e| format!("read sb: {e:?}"))?;
    let bitmap_start = sb.bitmap_start;
    let bitmap_blocks = sb.bitmap_blocks;
    let bitmap_area = sb.bitmap_area;
    for slot in SLOT_BLOCKS {
        blocks.insert(slot);
    }
    let area_start = bitmap_start + bitmap_area * bitmap_blocks;
    for b in area_start..area_start + bitmap_blocks {
        blocks.insert(b);
    }

    eprintln!(
        "{} send: {} blocks (gen {cur_gen})",
        if is_incremental {
            "incremental"
        } else {
            "full"
        },
        blocks.len()
    );

    let mut stream = TcpStream::connect(replica_addr).map_err(|e| format!("connect: {e}"))?;

    // HELLO
    write_u32(&mut stream, MAGIC).map_err(|e| e.to_string())?;
    write_u32(&mut stream, VERSION).map_err(|e| e.to_string())?;

    // SNAPSHOT
    stream
        .write_all(&[MSG_SNAPSHOT])
        .map_err(|e| e.to_string())?;
    write_roots(&mut stream, &cur_roots, cur_gen, &fs.uuid()).map_err(|e| e.to_string())?;
    write_u64(&mut stream, fs.block_count()).map_err(|e| e.to_string())?;

    // BLOCKs: data and bitmap blocks first, superblock slots (0, 1) LAST.
    // Crash safety: the receiver fsyncs data before publishing the new
    // superblock, so a crash never exposes a generation whose blocks
    // aren't durable.
    let mut blk = [0u8; BLOCK_SIZE];
    let mut sorted: Vec<u64> = blocks.into_iter().collect();
    sorted.sort_unstable();
    let (sb_slots, data_blocks): (Vec<u64>, Vec<u64>) = sorted.into_iter().partition(|id| *id < 2);
    for id in data_blocks.iter().chain(sb_slots.iter()) {
        dev.read_block(*id, &mut blk)
            .map_err(|e| format!("read block {id}: {e}"))?;
        stream.write_all(&[MSG_BLOCK]).map_err(|e| e.to_string())?;
        write_u64(&mut stream, *id).map_err(|e| e.to_string())?;
        stream.write_all(&blk).map_err(|e| e.to_string())?;
    }

    // COMMIT
    stream.write_all(&[MSG_COMMIT]).map_err(|e| e.to_string())?;
    write_u64(&mut stream, cur_gen).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    // ACK
    let magic = read_u32(&mut stream).map_err(|e| e.to_string())?;
    let _ver = read_u32(&mut stream).map_err(|e| e.to_string())?;
    if magic != MAGIC {
        return Err("bad magic in reply".into());
    }
    let mut tag = [0u8; 1];
    stream.read_exact(&mut tag).map_err(|e| e.to_string())?;
    match tag[0] {
        MSG_ACK => {
            let status = read_u32(&mut stream).map_err(|e| e.to_string())?;
            if status != 0 {
                return Err(format!("replica reported error status {status}"));
            }
        }
        MSG_ERROR => {
            let len = read_u32(&mut stream).map_err(|e| e.to_string())? as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).map_err(|e| e.to_string())?;
            return Err(format!("replica error: {}", String::from_utf8_lossy(&buf)));
        }
        t => return Err(format!("unexpected reply tag {t}")),
    }

    save_state(&spath, &cur_roots, cur_gen, &cur_uuid).map_err(|e| e.to_string())?;
    eprintln!(
        "replicated {} blocks, gen {cur_gen}",
        data_blocks.len() + sb_slots.len()
    );
    Ok((data_blocks.len() + sb_slots.len(), cur_gen))
}

fn verify_node_checksum(blk: &Block) -> bool {
    // Node blocks have magic u16 at [0..2] and CRC32C u64 at [16..24].
    // Only verify if the magic matches; data blocks are skipped.
    let magic = u16::from_le_bytes(blk[0..2].try_into().unwrap_or([0; 2]));
    if magic != NODE_MAGIC {
        return true; // Not a node block — nothing to verify.
    }
    let stored = u64::from_le_bytes(blk[16..24].try_into().unwrap_or([0; 8]));
    let mut tmp = *blk;
    tmp[16..24].copy_from_slice(&[0u8; 8]);
    cownfs_core::checksum::checksum(&tmp) == stored
}

fn do_receive(image: &Path, listen_addr: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen_addr).map_err(|e| format!("bind: {e}"))?;
    eprintln!("replica listening on {listen_addr} for {image:?}");
    let (mut stream, peer) = listener.accept().map_err(|e| format!("accept: {e}"))?;
    eprintln!("connection from {peer}");

    let magic = read_u32(&mut stream).map_err(|e| e.to_string())?;
    let version = read_u32(&mut stream).map_err(|e| e.to_string())?;
    if magic != MAGIC || version != VERSION {
        send_error(&mut stream, "bad hello");
        return Err("bad hello".into());
    }

    let mut dev = FileDevice::open(image).map_err(|e| format!("open image: {e}"))?;
    let mut blocks_received = 0u64;
    // Superblock slots are buffered, not written immediately. On COMMIT
    // we fsync data blocks first, then publish the superblock, then fsync
    // again. A crash before the second fsync leaves the old superblock
    // intact (ping-pong slots + CRC detect torn writes).
    let mut pending_sb: Vec<(u64, [u8; BLOCK_SIZE])> = Vec::new();

    loop {
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_err() {
            send_error(&mut stream, "unexpected EOF");
            return Err("unexpected EOF".into());
        }
        match tag[0] {
            MSG_SNAPSHOT => {
                let (_roots, _gen, _uuid) = read_roots(&mut stream).map_err(|e| e.to_string())?;
                let block_count = read_u64(&mut stream).map_err(|e| e.to_string())?;
                if block_count != dev.block_count() {
                    send_error(&mut stream, "block count mismatch");
                    return Err(format!(
                        "block count mismatch: replica {}, primary {block_count}",
                        dev.block_count()
                    ));
                }
                // TODO: validate _uuid against the replica's superblock UUID
                // to reject divergent primaries. Needs a light superblock
                // UUID read (Fs::open is too heavy here).
            }
            MSG_BLOCK => {
                let id = read_u64(&mut stream).map_err(|e| e.to_string())?;
                let mut blk = [0u8; BLOCK_SIZE];
                stream.read_exact(&mut blk).map_err(|e| e.to_string())?;
                if !verify_node_checksum(&blk) {
                    send_error(&mut stream, "node checksum mismatch");
                    return Err(format!("node checksum mismatch on block {id}"));
                }
                if id < 2 {
                    // Superblock slot: defer until COMMIT.
                    pending_sb.push((id, blk));
                } else {
                    dev.write_block(id, &blk)
                        .map_err(|e| format!("write block {id}: {e}"))?;
                }
                blocks_received += 1;
            }
            MSG_COMMIT => {
                let gen = read_u64(&mut stream).map_err(|e| e.to_string())?;
                // 1. Data blocks durable.
                dev.sync().map_err(|e| format!("sync: {e}"))?;
                // 2. Publish the new superblock.
                for (id, blk) in &pending_sb {
                    dev.write_block(*id, blk)
                        .map_err(|e| format!("write superblock {id}: {e}"))?;
                }
                // 3. Superblock durable.
                dev.sync().map_err(|e| format!("sync superblock: {e}"))?;
                // ACK
                write_u32(&mut stream, MAGIC).map_err(|e| e.to_string())?;
                write_u32(&mut stream, VERSION).map_err(|e| e.to_string())?;
                stream.write_all(&[MSG_ACK]).map_err(|e| e.to_string())?;
                write_u32(&mut stream, 0).map_err(|e| e.to_string())?;
                stream.flush().map_err(|e| e.to_string())?;
                eprintln!("committed gen {gen}: {blocks_received} blocks");
                return Ok(());
            }
            t => {
                send_error(&mut stream, "unknown message");
                return Err(format!("unknown message tag {t}"));
            }
        }
    }
}

/// Replication lag metrics, persisted as JSON for monitoring.
#[derive(Debug, Clone)]
struct ReplStatus {
    last_attempt_unix: u64,
    last_success_unix: u64,
    last_gen_replicated: u64,
    primary_gen: u64,
    blocks_sent_last: u64,
    total_blocks_sent: u64,
    consecutive_failures: u32,
    last_error: String,
}

impl ReplStatus {
    fn new() -> Self {
        Self {
            last_attempt_unix: 0,
            last_success_unix: 0,
            last_gen_replicated: 0,
            primary_gen: 0,
            blocks_sent_last: 0,
            total_blocks_sent: 0,
            consecutive_failures: 0,
            last_error: String::new(),
        }
    }

    fn lag_seconds(&self, now: u64) -> u64 {
        if self.last_success_unix == 0 {
            return u64::MAX;
        }
        now.saturating_sub(self.last_success_unix)
    }

    fn generations_behind(&self) -> u64 {
        self.primary_gen.saturating_sub(self.last_gen_replicated)
    }

    fn to_json(&self) -> String {
        let esc = self.last_error.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            "{{\n  \"last_attempt_unix\": {},\n  \"last_success_unix\": {},\n  \"lag_seconds\": {},\n  \"primary_gen\": {},\n  \"last_gen_replicated\": {},\n  \"generations_behind\": {},\n  \"blocks_sent_last\": {},\n  \"total_blocks_sent\": {},\n  \"consecutive_failures\": {},\n  \"last_error\": \"{}\"\n}}\n",
            self.last_attempt_unix,
            self.last_success_unix,
            if self.last_success_unix == 0 {
                "null".to_string()
            } else {
                self.lag_seconds(now_unix()).to_string()
            },
            self.primary_gen,
            self.last_gen_replicated,
            self.generations_behind(),
            self.blocks_sent_last,
            self.total_blocks_sent,
            self.consecutive_failures,
            esc,
        )
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn status_path(state_file: Option<&str>, primary: &Path) -> PathBuf {
    match state_file {
        Some(s) => {
            let mut p = PathBuf::from(s);
            p.set_extension("status");
            p
        }
        None => {
            let mut p = primary.as_os_str().to_owned();
            p.push(".repl.status");
            PathBuf::from(p)
        }
    }
}

fn do_drive(
    primary: &Path,
    replica_addr: &str,
    state_file: Option<&str>,
    interval_secs: u64,
) -> Result<(), String> {
    let spath = status_path(state_file, primary);
    let mut status = ReplStatus::new();
    eprintln!("replication driver: {primary:?} -> {replica_addr} every {interval_secs}s");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        let now = now_unix();
        status.last_attempt_unix = now;
        // Peek at the primary generation for the behind-metric even on failure.
        if let Ok(fs) = Fs::open(primary) {
            status.primary_gen = fs.generation();
        }
        match do_send(primary, replica_addr, state_file) {
            Ok((blocks, gen)) => {
                status.last_success_unix = now;
                status.last_gen_replicated = gen;
                status.primary_gen = gen;
                status.blocks_sent_last = blocks as u64;
                status.total_blocks_sent += blocks as u64;
                status.consecutive_failures = 0;
                status.last_error.clear();
                eprintln!("replicated gen {gen} ({blocks} blocks), lag 0s");
            }
            Err(e) => {
                status.consecutive_failures += 1;
                status.last_error = e.clone();
                eprintln!(
                    "replication failed ({} in a row): {e}; {} generations behind",
                    status.consecutive_failures,
                    status.generations_behind(),
                );
            }
        }
        if let Err(e) = std::fs::write(&spath, status.to_json()) {
            eprintln!("warning: cannot write status file: {e}");
        }
    }
}

fn do_status(state_file: Option<&str>, primary: &Path) -> Result<(), String> {
    let spath = status_path(state_file, primary);
    let data = std::fs::read_to_string(&spath).map_err(|e| format!("read status: {e}"))?;
    println!("{data}");
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(|s| s.as_str()) {
        Some("send") => {
            let primary = args.get(2).cloned().unwrap_or_default();
            let addr = args.get(3).cloned().unwrap_or_default();
            let mut state: Option<String> = None;
            let mut i = 4;
            while i < args.len() {
                if args[i] == "--state" && i + 1 < args.len() {
                    state = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if primary.is_empty() || addr.is_empty() {
                eprintln!(
                    "usage: cownfs-replicate send <primary-image> <replica-addr> [--state <file>]"
                );
                std::process::exit(1);
            }
            do_send(Path::new(&primary), &addr, state.as_deref()).map(|_| ())
        }
        Some("receive") => {
            let image = args.get(2).cloned().unwrap_or_default();
            let addr = args
                .get(3)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:2050".into());
            if image.is_empty() {
                eprintln!("usage: cownfs-replicate receive <replica-image> [listen-addr]");
                std::process::exit(1);
            }
            do_receive(Path::new(&image), &addr)
        }
        Some("drive") => {
            let primary = args.get(2).cloned().unwrap_or_default();
            let addr = args.get(3).cloned().unwrap_or_default();
            let mut state: Option<String> = None;
            let mut interval: u64 = 60;
            let mut i = 4;
            while i < args.len() {
                match args[i].as_str() {
                    "--state" if i + 1 < args.len() => {
                        state = Some(args[i + 1].clone());
                        i += 2;
                    }
                    "--interval" if i + 1 < args.len() => {
                        interval = args[i + 1].parse().unwrap_or(60);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            if primary.is_empty() || addr.is_empty() {
                eprintln!("usage: cownfs-replicate drive <primary-image> <replica-addr> [--state <file>] [--interval <secs>]");
                std::process::exit(1);
            }
            do_drive(Path::new(&primary), &addr, state.as_deref(), interval)
        }
        Some("status") => {
            let primary = args.get(2).cloned().unwrap_or_default();
            let mut state: Option<String> = None;
            let mut i = 3;
            while i < args.len() {
                if args[i] == "--state" && i + 1 < args.len() {
                    state = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if primary.is_empty() {
                eprintln!("usage: cownfs-replicate status <primary-image> [--state <file>]");
                std::process::exit(1);
            }
            do_status(state.as_deref(), Path::new(&primary))
        }
        _ => {
            eprintln!("usage:");
            eprintln!("  cownfs-replicate send <primary-image> <replica-addr> [--state <file>]");
            eprintln!("  cownfs-replicate receive <replica-image> [listen-addr]");
            eprintln!("  cownfs-replicate drive <primary-image> <replica-addr> [--state <file>] [--interval <secs>]");
            eprintln!("  cownfs-replicate status <primary-image> [--state <file>]");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("replicate error: {e}");
        std::process::exit(1);
    }
}
