//! P7 gate: soak test — long-running create/write/rename/delete/snapshot
//! workload leaves a clean image.
//!
//! Iteration count is controlled by `COWNFS_SOAK_ITERS` (default 500).
//! The multi-hour gate is satisfied by running with a high count;
//! CI uses the default for a bounded smoke.

use cownfs_core::engine::{Fs, ROOT_INO};

fn iters() -> usize {
    std::env::var("COWNFS_SOAK_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}

#[test]
fn p7_soak() {
    let n = iters();
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-p7-soak-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 16384).unwrap();
    let root = ROOT_INO;

    // Deterministic PRNG (xorshift64).
    let mut state: u64 = 0x123456789abcdef;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut files: Vec<(Vec<u8>, u64)> = Vec::new(); // (name, ino)
    let mut snap_count = 0u64;

    for i in 0..n {
        let op = rand() % 6;
        match op {
            // 0: create + write
            0 => {
                let name = format!("f{i:06}").into_bytes();
                if fs.lookup(root, &name).unwrap().is_none() {
                    let ino = fs.create(root, &name, 0o644, 0, 0).unwrap();
                    let size = (rand() % 8192) as usize + 1;
                    let mut data = vec![0u8; size];
                    for (j, b) in data.iter_mut().enumerate() {
                        *b = ((i + j) % 256) as u8;
                    }
                    fs.write(ino, 0, &data).unwrap();
                    files.push((name, ino));
                }
            }
            // 1: rename
            1 => {
                if !files.is_empty() {
                    let idx = (rand() as usize) % files.len();
                    let (old_name, _) = &files[idx];
                    let new_name = format!("r{i:06}").into_bytes();
                    if fs.lookup(root, &new_name).unwrap().is_none() {
                        fs.rename(root, old_name, root, &new_name).unwrap();
                        files[idx].0 = new_name;
                    }
                }
            }
            // 2: delete
            2 => {
                if !files.is_empty() {
                    let idx = (rand() as usize) % files.len();
                    let (name, _) = files.remove(idx);
                    let _ = fs.unlink(root, &name);
                }
            }
            // 3: snapshot create
            3 => {
                let snap_name = format!("snap{snap_count}");
                if fs.snapshot_create(snap_name.as_bytes()).is_ok() {
                    snap_count += 1;
                }
            }
            // 4: snapshot delete (oldest)
            4 => {
                let snaps = fs.snapshot_list().unwrap();
                if !snaps.is_empty() {
                    let (id, _) = snaps[0];
                    let _ = fs.snapshot_delete(id);
                }
            }
            // 5: overwrite existing file
            _ => {
                if !files.is_empty() {
                    let idx = (rand() as usize) % files.len();
                    let (_, ino) = files[idx];
                    let data = vec![(i % 256) as u8; 1024];
                    let _ = fs.write(ino, 0, &data);
                }
            }
        }
        // Commit every 10 ops to exercise the commit path.
        if i % 10 == 9 {
            fs.commit().unwrap();
        }
        // Heartbeat for long (multi-hour) runs.
        if i % 100_000 == 99_999 {
            let live_snaps = fs.snapshot_list().unwrap().len();
            eprintln!(
                "soak progress: {}/{n} ops, {} files, {live_snaps} live snapshots",
                i + 1,
                files.len()
            );
        }
    }
    fs.commit().unwrap();

    // Verify all surviving files are readable.
    for (name, ino) in &files {
        if let Some((found_ino, _)) = fs.lookup(root, name).unwrap() {
            assert_eq!(found_ino, *ino);
            let attr = fs.getattr(*ino).unwrap();
            let _ = fs.read(*ino, 0, attr.size.min(1024) as usize).unwrap();
        }
    }

    // Final check must pass.
    fs.check().expect("soak left image inconsistent");
    let live_snaps = fs.snapshot_list().unwrap().len();
    println!(
        "Soak: {n} ops, {} files, {snap_count} snapshots created ({live_snaps} live), check clean",
        files.len()
    );

    drop(fs);
    std::fs::remove_file(&img).unwrap();
}
