//! cownfs NFSv4.0 server: serves an image file over TCP (default 127.0.0.1:2049).
use std::env;

use cownfs_core::engine::Fs;
use cownfs_nfs::server;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cownfs-server <image> [addr]");
        std::process::exit(1);
    }
    let addr = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:2049".into());
    let fs = Fs::open(std::path::Path::new(&args[1])).expect("open image");
    eprintln!("serving {} on {addr}", args[1]);
    if let Err(e) = server::serve(&addr, server::Shared::new(fs)) {
        eprintln!("server error: {e:?}");
        std::process::exit(1);
    }
}
