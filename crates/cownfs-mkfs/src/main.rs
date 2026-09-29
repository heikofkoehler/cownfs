//! cownfs-mkfs: format a cownfs image (root directory only).

use std::path::PathBuf;

use cownfs_core::engine::Fs;

fn parse_size(s: &str) -> Result<u64, String> {
    let (num, mult) = match s.chars().last() {
        Some('K') | Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('M') | Some('m') => (&s[..s.len() - 1], 1024u64.pow(2)),
        Some('G') | Some('g') => (&s[..s.len() - 1], 1024u64.pow(3)),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("bad size: {s}"))
}

fn hex_uuid(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut size = 1024u64.pow(3); // default 1 GiB
    let mut image: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--size" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--size needs a value");
                    std::process::exit(2);
                }
                size = parse_size(&args[i]).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(2);
                });
            }
            a if a.starts_with("--size=") => {
                size = parse_size(&a["--size=".len()..]).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(2);
                });
            }
            other => image = Some(other.to_string()),
        }
        i += 1;
    }
    let image = image.unwrap_or_else(|| {
        eprintln!("usage: cownfs-mkfs [--size 1G] IMAGE");
        std::process::exit(2);
    });

    if size % 4096 != 0 || size < 64 * 1024 {
        eprintln!("size must be a multiple of 4K and at least 64K");
        std::process::exit(2);
    }
    if PathBuf::from(&image).exists() {
        eprintln!("{image}: already exists");
        std::process::exit(1);
    }
    let blocks = size / 4096;

    match Fs::format(&PathBuf::from(&image), blocks) {
        Ok(fs) => println!(
            "formatted {image}: {} blocks ({} MiB), uuid={}, generation={}",
            fs.block_count(),
            fs.block_count() * 4096 / 1024 / 1024,
            hex_uuid(&fs.uuid()),
            fs.generation(),
        ),
        Err(e) => {
            eprintln!("format {image}: {e}");
            std::process::exit(1);
        }
    }
}
