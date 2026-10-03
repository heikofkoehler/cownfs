//! cownfs-xattr: manage extended attributes.
//!
//!   cownfs-xattr set <image> <path> <name> <value>  - set an xattr
//!   cownfs-xattr get <image> <path> <name>           - get an xattr
//!   cownfs-xattr list <image> <path>                - list xattr names
//!   cownfs-xattr remove <image> <path> <name>        - remove an xattr
//!
//! Paths are absolute from the filesystem root (e.g. /dir/file).

use std::env;
use std::path::Path;

use cownfs_core::engine::{Fs, ROOT_INO};

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  cownfs-xattr set <image> <path> <name> <value>");
    eprintln!("  cownfs-xattr get <image> <path> <name>");
    eprintln!("  cownfs-xattr list <image> <path>");
    eprintln!("  cownfs-xattr remove <image> <path> <name>");
    std::process::exit(1);
}

fn resolve(fs: &Fs, path: &str) -> Result<u64, String> {
    if !path.starts_with('/') {
        return Err("path must be absolute".into());
    }
    let mut ino = ROOT_INO;
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        let (next, _) = fs
            .lookup(ino, comp.as_bytes())
            .map_err(|e| format!("lookup {comp}: {e:?}"))?
            .ok_or_else(|| format!("not found: {comp}"))?;
        ino = next;
    }
    Ok(ino)
}

fn do_set(image: &Path, path: &str, name: &str, value: &str) -> Result<(), String> {
    let mut fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let ino = resolve(&fs, path)?;
    fs.setxattr(ino, name.as_bytes(), value.as_bytes())
        .map_err(|e| format!("set: {e:?}"))?;
    fs.commit().map_err(|e| format!("commit: {e:?}"))?;
    println!("set {name} on {path}");
    Ok(())
}

fn do_get(image: &Path, path: &str, name: &str) -> Result<(), String> {
    let fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let ino = resolve(&fs, path)?;
    match fs
        .getxattr(ino, name.as_bytes())
        .map_err(|e| format!("get: {e:?}"))?
    {
        Some(v) => {
            println!("{}", String::from_utf8_lossy(&v));
            Ok(())
        }
        None => Err(format!("{name}: not set")),
    }
}

fn do_list(image: &Path, path: &str) -> Result<(), String> {
    let fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let ino = resolve(&fs, path)?;
    let names = fs
        .listxattrs(ino)
        .map_err(|e| format!("list: {e:?}"))?;
    for n in names {
        println!("{}", String::from_utf8_lossy(&n));
    }
    Ok(())
}

fn do_remove(image: &Path, path: &str, name: &str) -> Result<(), String> {
    let mut fs = Fs::open(image).map_err(|e| format!("open: {e:?}"))?;
    let ino = resolve(&fs, path)?;
    if fs
        .removexattr(ino, name.as_bytes())
        .map_err(|e| format!("remove: {e:?}"))?
    {
        fs.commit().map_err(|e| format!("commit: {e:?}"))?;
        println!("removed {name} from {path}");
        Ok(())
    } else {
        Err(format!("{name}: not set"))
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        usage();
    }
    let image = Path::new(&args[2]);
    let result = match args[1].as_str() {
        "set" if args.len() == 6 => do_set(image, &args[3], &args[4], &args[5]),
        "get" if args.len() == 5 => do_get(image, &args[3], &args[4]),
        "list" if args.len() == 4 => do_list(image, &args[3]),
        "remove" if args.len() == 5 => do_remove(image, &args[3], &args[4]),
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("cownfs-xattr: {e}");
        std::process::exit(1);
    }
}
