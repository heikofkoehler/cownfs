//! cownfs-fsck: offline filesystem consistency checker.
//!
//!   cownfs-fsck <image>   - check image consistency, exit 0 if healthy

use std::env;
use std::path::Path;

use cownfs_core::engine::Fs;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: cownfs-fsck <image>");
        std::process::exit(2);
    }
    let image = Path::new(&args[1]);
    let fs = match Fs::open(image) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("fsck: open failed: {e:?}");
            std::process::exit(1);
        }
    };
    match fs.check() {
        Ok(report) => {
            println!(
                "fsck: OK (gen {}, {} meta blocks, {} data blocks, {} allocated)",
                fs.generation(),
                report.meta_blocks,
                report.data_blocks,
                report.allocated_blocks
            );
        }
        Err(e) => {
            eprintln!("fsck: CHECK FAILED: {e:?}");
            std::process::exit(1);
        }
    }
}
