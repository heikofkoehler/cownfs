//! cownfs-fsck: validate superblock slots, walk all four trees
//! (checksums, key order, link resolvability), and reconcile every
//! reachable block against the active bitmap area (exit 1 on any error).

use std::path::PathBuf;

use cownfs_core::block::FileDevice;
use cownfs_core::superblock;

fn hex_uuid(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut reclaim = false;
    let mut image = None;
    for a in &args[1..] {
        if a == "--reclaim" {
            reclaim = true;
        } else if image.is_none() {
            image = Some(a.clone());
        } else {
            eprintln!("unexpected argument: {a}");
            std::process::exit(2);
        }
    }
    let image = image.unwrap_or_else(|| {
        eprintln!("usage: cownfs-fsck [--reclaim] IMAGE");
        std::process::exit(2);
    });
    let path = PathBuf::from(&image);
    let dev = FileDevice::open(&path).unwrap_or_else(|e| {
        eprintln!("open {image}: {e}");
        std::process::exit(1);
    });

    let mut healthy = true;
    for slot in 0..2 {
        match superblock::read_slot(&dev, slot) {
            Some(sb) => println!(
                "slot {slot}: OK generation={} bitmap_area={} blocks={} uuid={}",
                sb.generation,
                sb.bitmap_area,
                sb.block_count,
                hex_uuid(&sb.uuid),
            ),
            None => {
                println!("slot {slot}: CORRUPT or invalid");
                healthy = false;
            }
        }
    }

    let (sb, slot) = match superblock::open(&dev) {
        Ok(v) => v,
        Err(e) => {
            println!("NO VALID SUPERBLOCK: {e}");
            std::process::exit(1);
        }
    };
    println!("active slot: {slot}, generation {}", sb.generation);

    drop(dev);
    match cownfs_core::engine::Fs::open(&path) {
        Ok(mut fs) => {
            if reclaim {
                match fs.reclaim_unreachable() {
                    Ok(n) => {
                        println!("reclaimed {n} unreachable blocks");
                        if n > 0 {
                            if let Err(e) = fs.commit() {
                                println!("COMMIT FAILED: {e}");
                                healthy = false;
                            } else {
                                println!("reclaim committed");
                            }
                        }
                    }
                    Err(e) => {
                        println!("RECLAIM FAILED: {e}");
                        healthy = false;
                    }
                }
            }
            match fs.check() {
                Ok(rep) => println!(
                    "trees OK: meta_blocks={} data_blocks={} allocated_blocks={}",
                    rep.meta_blocks, rep.data_blocks, rep.allocated_blocks,
                ),
                Err(e) => {
                    println!("CHECK FAILED: {e}");
                    healthy = false;
                }
            }
        }
        Err(e) => {
            println!("OPEN FAILED: {e}");
            healthy = false;
        }
    }

    std::process::exit(if healthy { 0 } else { 1 });
}
