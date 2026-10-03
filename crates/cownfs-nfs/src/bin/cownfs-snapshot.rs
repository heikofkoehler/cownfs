//! cownfs-snapshot: manage filesystem snapshots.
//!
//!   cownfs-snapshot create <image> <name>   - create a snapshot
//!   cownfs-snapshot list <image>            - list snapshots
//!   cownfs-snapshot delete <image> <id>     - delete a snapshot
//!   cownfs-snapshot info <image> <id>       - show snapshot details
//!
//! Snapshots are cheap CoW root copies. They pin data blocks until deleted.

use std::env;
use std::path::Path;

use cownfs_core::engine::Fs;

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  cownfs-snapshot create <image> <name>");
    eprintln!("  cownfs-snapshot list <image>");
    eprintln!("  cownfs-snapshot delete <image> <id>");
    eprintln!("  cownfs-snapshot info <image> <id>");
    std::process::exit(1);
}

fn do_create(image: &Path, name: &str) -> Result<(), String> {
    let mut fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let id = fs
        .snapshot_create(name.as_bytes())
        .map_err(|e| format!("create: {e:?}"))?;
    fs.commit().map_err(|e| format!("commit: {e:?}"))?;
    println!("Snapshot '{name}' created with id {id}");
    Ok(())
}

fn do_list(image: &Path) -> Result<(), String> {
    let fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let snaps = fs.snapshot_list().map_err(|e| format!("list: {e:?}"))?;
    if snaps.is_empty() {
        println!("No snapshots.");
    } else {
        println!("ID\tName");
        for (id, name) in snaps {
            let name_str = String::from_utf8_lossy(&name);
            println!("{id}\t{name_str}");
        }
    }
    Ok(())
}

fn do_delete(image: &Path, id: u64) -> Result<(), String> {
    let mut fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    fs.snapshot_delete(id)
        .map_err(|e| format!("delete: {e:?}"))?;
    fs.commit().map_err(|e| format!("commit: {e:?}"))?;
    println!("Snapshot {id} deleted.");
    Ok(())
}

fn do_info(image: &Path, id: u64) -> Result<(), String> {
    let fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let snaps = fs.snapshot_list().map_err(|e| format!("list: {e:?}"))?;
    for (sid, name) in snaps {
        if sid == id {
            let name_str = String::from_utf8_lossy(&name);
            println!("ID: {sid}");
            println!("Name: {name_str}");
            // TODO: show creation time, size, etc. when available.
            return Ok(());
        }
    }
    Err(format!("snapshot {id} not found"))
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        usage();
    }
    let result = match args[1].as_str() {
        "create" if args.len() == 4 => do_create(Path::new(&args[2]), &args[3]),
        "list" if args.len() == 3 => do_list(Path::new(&args[2])),
        "delete" if args.len() == 4 => {
            let id: u64 = args[3].parse().unwrap_or_else(|_| usage());
            do_delete(Path::new(&args[2]), id)
        }
        "info" if args.len() == 4 => {
            let id: u64 = args[3].parse().unwrap_or_else(|_| usage());
            do_info(Path::new(&args[2]), id)
        }
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
