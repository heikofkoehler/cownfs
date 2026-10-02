//! cownfs NFSv4.0 server: serves an image file over TCP (default 127.0.0.1:2049).
use std::env;

use cownfs_core::engine::Fs;
use cownfs_nfs::server;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cownfs-server [--read-only] [--ds-addr <addr>] <image> [addr]");
        std::process::exit(1);
    }
    let read_only = args.iter().any(|a| a == "--read-only");
    let ds_addr = args
        .iter()
        .position(|a| a == "--ds-addr")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let positional: Vec<&String> = args[1..]
        .iter()
        .filter(|a| *a != "--read-only" && *a != "--ds-addr")
        .collect();
    // Remove the ds-addr value from positionals.
    let positional: Vec<&String> = positional
        .into_iter()
        .filter(|a| Some(*a) != ds_addr.as_ref())
        .collect();
    let addr = positional
        .get(1)
        .cloned()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:2049".into());
    let fs = Fs::open(std::path::Path::new(positional[0])).expect("open image");
    eprintln!(
        "serving {} on {addr}{}{}",
        positional[0],
        if read_only { " (read-only)" } else { "" },
        ds_addr
            .as_ref()
            .map(|a| format!(" (ds: {a})"))
            .unwrap_or_default(),
    );
    let shared = if read_only {
        server::Shared::new_read_only(fs)
    } else {
        server::Shared::new(fs)
    };
    let shared = match ds_addr {
        Some(a) => shared.with_ds_addr(a),
        None => shared,
    };
    if let Err(e) = server::serve(&addr, shared) {
        eprintln!("server error: {e:?}");
        std::process::exit(1);
    }
}
