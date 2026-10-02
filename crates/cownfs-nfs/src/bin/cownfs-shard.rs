//! cownfs-shard: static subtree sharding tooling (Phase 2).
//!
//! Each shard is an independent cownfs image served by its own
//! `cownfs-server`. This tool manages the shard map — a static config
//! mapping path prefixes to shards — and provisions shard images.
//!
//! Usage:
//!   cownfs-shard route <map-file> <path>     # which shard serves <path>
//!   cownfs-shard init <map-file>              # create missing shard images
//!   cownfs-shard list <map-file>              # show the shard map
//!   cownfs-shard check <map-file>             # verify shards are reachable
//!
//! Map file format (simple `key = value` per shard, blank line separated):
//!
//!   [home]
//!   prefix = /home
//!   addr = 127.0.0.1:2049
//!   image = /data/shard-home.img
//!
//!   [data]
//!   prefix = /data
//!   addr = 127.0.0.1:2050
//!   image = /data/shard-data.img
//!
//! Routing uses longest-prefix match. Cross-shard RENAME is impossible:
//! shards are separate mounts, so clients get EXDEV/XDEV naturally.

use std::collections::HashMap;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
struct Shard {
    name: String,
    prefix: String,
    addr: String,
    image: String,
}

fn parse_map(path: &Path) -> Result<Vec<Shard>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open map: {e}"))?;
    let mut text = String::new();
    f.read_to_string(&mut text)
        .map_err(|e| format!("read map: {e}"))?;

    let mut shards = Vec::new();
    let mut cur: Option<(String, HashMap<String, String>)> = None;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if let Some((n, kv)) = cur.take() {
                shards.push(make_shard(&n, kv).map_err(|e| format!("line {}: {e}", lineno))?);
            }
            cur = Some((name.trim().to_string(), HashMap::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            match &mut cur {
                Some((_, kv)) => {
                    kv.insert(k.trim().to_string(), v.trim().to_string());
                }
                None => return Err(format!("line {}: key=value outside a section", lineno + 1)),
            }
        } else {
            return Err(format!("line {}: unparseable: {line}", lineno + 1));
        }
    }
    if let Some((n, kv)) = cur.take() {
        shards.push(make_shard(&n, kv).map_err(|e| format!("eof: {e}"))?);
    }
    if shards.is_empty() {
        return Err("no shards defined".into());
    }
    // Validate: prefixes must be absolute, non-empty, and unique.
    let mut seen = std::collections::HashSet::new();
    for s in &shards {
        if !s.prefix.starts_with('/') || s.prefix.len() < 2 {
            return Err(format!(
                "shard '{}': prefix must be absolute like /home",
                s.name
            ));
        }
        if !seen.insert(s.prefix.clone()) {
            return Err(format!("duplicate prefix {}", s.prefix));
        }
    }
    Ok(shards)
}

fn make_shard(name: &str, kv: HashMap<String, String>) -> Result<Shard, String> {
    let get = |k: &str| {
        kv.get(k)
            .cloned()
            .ok_or_else(|| format!("shard '{name}': missing {k}"))
    };
    Ok(Shard {
        name: name.to_string(),
        prefix: get("prefix")?,
        addr: get("addr")?,
        image: get("image")?,
    })
}

/// Longest-prefix match: which shard serves `path`?
fn route<'a>(shards: &'a [Shard], path: &str) -> Option<&'a Shard> {
    let mut best: Option<&Shard> = None;
    for s in shards {
        // Prefix matches if path == prefix or path starts with prefix + '/'.
        let hit = path == s.prefix
            || (path.starts_with(s.prefix.as_str()) && path[s.prefix.len()..].starts_with('/'));
        if hit && best.map_or(true, |b: &Shard| s.prefix.len() > b.prefix.len()) {
            best = Some(s);
        }
    }
    best
}

fn do_route(map: &Path, path: &str) -> Result<(), String> {
    let shards = parse_map(map)?;
    match route(&shards, path) {
        Some(s) => {
            println!("{} {} {}", s.name, s.addr, s.image);
            Ok(())
        }
        None => Err(format!("no shard serves {path}")),
    }
}

fn do_list(map: &Path) -> Result<(), String> {
    let shards = parse_map(map)?;
    for s in &shards {
        println!("{}: {} -> {} ({})", s.name, s.prefix, s.addr, s.image);
    }
    Ok(())
}

fn do_init(map: &Path) -> Result<(), String> {
    let shards = parse_map(map)?;
    for s in &shards {
        let img = Path::new(&s.image);
        if img.exists() {
            println!("{}: {} exists, skipping", s.name, s.image);
            continue;
        }
        if let Some(parent) = img.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        // 1 GiB default per shard; caller can resize the image later.
        cownfs_core::engine::Fs::format(img, 262144)
            .map_err(|e| format!("format {}: {e:?}", s.image))?;
        println!("{}: formatted {}", s.name, s.image);
    }
    Ok(())
}

fn do_check(map: &Path) -> Result<(), String> {
    let shards = parse_map(map)?;
    let mut all_ok = true;
    for s in &shards {
        match TcpStream::connect_timeout(
            &s.addr
                .parse()
                .map_err(|e| format!("bad addr {}: {e}", s.addr))?,
            Duration::from_secs(2),
        ) {
            Ok(_) => println!("{}: {} reachable", s.name, s.addr),
            Err(e) => {
                println!("{}: {} UNREACHABLE ({e})", s.name, s.addr);
                all_ok = false;
            }
        }
    }
    if all_ok {
        Ok(())
    } else {
        Err("some shards unreachable".into())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(|s| s.as_str()) {
        Some("route") => {
            if args.len() < 4 {
                eprintln!("usage: cownfs-shard route <map-file> <path>");
                std::process::exit(1);
            }
            do_route(Path::new(&args[2]), &args[3])
        }
        Some("list") => {
            if args.len() < 3 {
                eprintln!("usage: cownfs-shard list <map-file>");
                std::process::exit(1);
            }
            do_list(Path::new(&args[2]))
        }
        Some("init") => {
            if args.len() < 3 {
                eprintln!("usage: cownfs-shard init <map-file>");
                std::process::exit(1);
            }
            do_init(Path::new(&args[2]))
        }
        Some("check") => {
            if args.len() < 3 {
                eprintln!("usage: cownfs-shard check <map-file>");
                std::process::exit(1);
            }
            do_check(Path::new(&args[2]))
        }
        _ => {
            eprintln!("usage:");
            eprintln!("  cownfs-shard route <map-file> <path>");
            eprintln!("  cownfs-shard list <map-file>");
            eprintln!("  cownfs-shard init <map-file>");
            eprintln!("  cownfs-shard check <map-file>");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("shard error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards() -> Vec<Shard> {
        vec![
            Shard {
                name: "home".into(),
                prefix: "/home".into(),
                addr: "a".into(),
                image: "i".into(),
            },
            Shard {
                name: "data".into(),
                prefix: "/data".into(),
                addr: "b".into(),
                image: "j".into(),
            },
            Shard {
                name: "archive".into(),
                prefix: "/data/archive".into(),
                addr: "c".into(),
                image: "k".into(),
            },
        ]
    }

    #[test]
    fn longest_prefix_wins() {
        let s = shards();
        assert_eq!(route(&s, "/data/archive/old").unwrap().name, "archive");
        assert_eq!(route(&s, "/data/new").unwrap().name, "data");
        assert_eq!(route(&s, "/home/u").unwrap().name, "home");
    }

    #[test]
    fn exact_prefix_matches() {
        let s = shards();
        assert_eq!(route(&s, "/home").unwrap().name, "home");
        assert_eq!(route(&s, "/data/archive").unwrap().name, "archive");
    }

    #[test]
    fn no_false_prefix() {
        let s = shards();
        // /database is not under /data.
        assert!(route(&s, "/database").is_none());
        assert!(route(&s, "/home2").is_none());
        assert!(route(&s, "/").is_none());
    }

    #[test]
    fn parse_and_validate() {
        let dir = std::env::temp_dir();
        let p = dir.join("test-shard-map.txt");
        std::fs::write(
            &p,
            "[home]\nprefix = /home\naddr = 127.0.0.1:2049\nimage = /tmp/h.img\n\n[data]\nprefix = /data\naddr = 127.0.0.1:2050\nimage = /tmp/d.img\n",
        )
        .unwrap();
        let s = parse_map(&p).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(route(&s, "/home/x").unwrap().addr, "127.0.0.1:2049");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_bad_prefix() {
        let dir = std::env::temp_dir();
        let p = dir.join("test-shard-map-bad.txt");
        std::fs::write(&p, "[x]\nprefix = home\naddr = a\nimage = i\n").unwrap();
        assert!(parse_map(&p).is_err());
        std::fs::remove_file(&p).ok();
    }
}
