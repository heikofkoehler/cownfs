//! cownfs-cluster: one-command scale-out topology setup.
//!
//! Sets up a sharded + replicated cownfs cluster:
//!   cownfs-cluster init --shards 3 --replicas 2 --base-dir /data
//!
//! Creates:
//!   /data/shard0.img, /data/shard1.img, ... (shard primaries)
//!   /data/referral.img (namespace skeleton with referral dirs)
//!   /data/referrals.conf (inode -> shard mapping)
//!
//! Then start servers manually (or via the printed commands).

use std::env;
use std::path::Path;

use cownfs_core::engine::{Fs, ROOT_INO};

fn usage() -> ! {
    eprintln!("usage: cownfs-cluster init --shards N --replicas M --base-dir DIR [--ports BASE]");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args[1] != "init" {
        usage();
    }

    let mut shards = 0usize;
    let mut replicas = 0usize;
    let mut base_dir = String::new();
    let mut port_base = 2049u16;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--shards" => {
                shards = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
                i += 2;
            }
            "--replicas" => {
                replicas = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0);
                i += 2;
            }
            "--base-dir" => {
                base_dir = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--ports" => {
                port_base = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(2049);
                i += 2;
            }
            _ => usage(),
        }
    }

    if shards == 0 || base_dir.is_empty() {
        usage();
    }

    let base = Path::new(&base_dir);
    std::fs::create_dir_all(base).expect("create base dir");

    println!("Setting up {shards} shards, {replicas} replicas each, in {base_dir}");

    // 1. Create shard images.
    for s in 0..shards {
        let img = base.join(format!("shard{s}.img"));
        if !img.exists() {
            println!("  formatting {}", img.display());
            Fs::format(&img, 16384).expect("format shard");
        } else {
            println!("  exists {}", img.display());
        }
        // Replica images (empty, will be filled by cownfs-replicate).
        for r in 0..replicas {
            let rimg = base.join(format!("shard{s}-replica{r}.img"));
            if !rimg.exists() {
                println!("  formatting {}", rimg.display());
                Fs::format(&rimg, 16384).expect("format replica");
            }
        }
    }

    // 2. Create referral image with shard mountpoint dirs.
    let ref_img = base.join("referral.img");
    let mut ref_fs = if ref_img.exists() {
        Fs::open(&ref_img).expect("open referral")
    } else {
        println!("  formatting {}", ref_img.display());
        Fs::format(&ref_img, 4096).expect("format referral")
    };

    let mut conf_lines = Vec::new();
    for s in 0..shards {
        let name = format!("shard{s}");
        // Create dir if it doesn't exist.
        let ino = match ref_fs.lookup(ROOT_INO, name.as_bytes()).expect("lookup") {
            Some((ino, _)) => ino,
            None => {
                let ino = ref_fs
                    .mkdir(ROOT_INO, name.as_bytes(), 0o755, 0, 0)
                    .expect("mkdir shard dir");
                println!("  created /{name} (ino {ino})");
                ino
            }
        };
        // Shard server will run on port_base + 1 + s.
        let port = port_base + 1 + s as u16;
        conf_lines.push(format!("{ino} 127.0.0.1:{port} /"));
    }
    ref_fs.commit().expect("commit referral");
    drop(ref_fs);

    // 3. Write referrals.conf.
    let conf_path = base.join("referrals.conf");
    std::fs::write(&conf_path, conf_lines.join("\n") + "\n").expect("write conf");
    println!("  wrote {}", conf_path.display());

    // 4. Print startup commands.
    println!("\nStart servers:");
    println!("  # Referral server (port {port_base}):");
    println!(
        "  cownfs-server --referrals {} {} 127.0.0.1:{port_base}",
        conf_path.display(),
        base.join("referral.img").display()
    );
    for s in 0..shards {
        let port = port_base + 1 + s as u16;
        println!("  # Shard {s} (port {port}):");
        println!(
            "  cownfs-server {} 127.0.0.1:{port}",
            base.join(format!("shard{s}.img")).display()
        );
        for r in 0..replicas {
            let rport = port_base + 100 + (s * 10 + r) as u16;
            println!("  # Shard {s} replica {r} (port {rport}):");
            println!(
                "  cownfs-replicate receive {} 127.0.0.1:{rport} &",
                base.join(format!("shard{s}-replica{r}.img")).display()
            );
            println!(
                "  cownfs-server --read-only {} 127.0.0.1:{}",
                base.join(format!("shard{s}-replica{r}.img")).display(),
                rport + 1000
            );
        }
    }
    println!("\nClient mounts only the referral server:");
    println!("  mount -t nfs -o vers=4.0,port={port_base} 127.0.0.1:/ /mnt/cow");
}
