//! Stress test: large randomized create/write/truncate/rename/link/unlink/
//! snapshot workload against a byte-exact in-memory shadow model.
//!
//! Iteration count is controlled by `COWNFS_STRESS_ITERS` (default 20000).
//! Commits happen at random intervals; every 5000 ops the image is
//! reopened and `check()` must pass. At the end every surviving file is
//! verified byte-for-byte against the shadow model and `check()` must be
//! clean. Snapshot isolation is verified: data read through a snapshot
//! must equal the pre-snapshot shadow even after the live file changes.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::collections::HashMap;

fn iters() -> usize {
    std::env::var("COWNFS_STRESS_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

struct FileState {
    data: Vec<u8>,
    refs: usize,
}

struct Name {
    parent: u64,
    name: Vec<u8>,
    ino: u64,
}

#[test]
fn stress_shadow_model() {
    let n = iters();
    println!("stress: {n} ops");
    let dir = std::env::temp_dir();
    let img = dir.join(format!("cownfs-stress-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&img);

    let mut rng = Rng(0xfeedfacecafebeef);
    let mut fs = Fs::format(&img, 32768).unwrap(); // 128 MiB
    let mut files: HashMap<u64, FileState> = HashMap::new();
    let mut names: Vec<Name> = Vec::new();
    let mut dirs: Vec<u64> = vec![ROOT_INO];
    let mut snaps: Vec<(u64, Vec<u8>)> = Vec::new();

    let live_name_count = |names: &Vec<Name>| names.len();

    for i in 0..n {
        let op = rng.below(12);
        match op {
            // 0,1,11: random write (possibly extending, possibly overlapping)
            0 | 1 | 11 => {
                if names.is_empty() {
                    continue;
                }
                let ni = rng.below(names.len());
                let ino = names[ni].ino;
                let st = files.get_mut(&ino).unwrap();
                let off = rng.below(st.data.len() + 4096) as u64;
                let len = rng.below(8192) + 1;
                let mut data = vec![0u8; len];
                for (j, b) in data.iter_mut().enumerate() {
                    *b = ((i + j) % 251) as u8;
                }
                fs.write(ino, off, &data).unwrap();
                let end = off as usize + len;
                if end > st.data.len() {
                    st.data.resize(end, 0);
                }
                st.data[off as usize..end].copy_from_slice(&data);
            }
            // 2: create
            2 => {
                if live_name_count(&names) >= 400 {
                    continue;
                }
                let parent = dirs[rng.below(dirs.len())];
                let name = format!("s{i:08}").into_bytes();
                if fs.lookup(parent, &name).unwrap().is_some() {
                    continue;
                }
                let ino = fs.create(parent, &name, 0o644, 0, 0).unwrap();
                files.insert(
                    ino,
                    FileState {
                        data: Vec::new(),
                        refs: 1,
                    },
                );
                names.push(Name { parent, name, ino });
            }
            // 3: truncate
            3 => {
                if names.is_empty() {
                    continue;
                }
                let ni = rng.below(names.len());
                let ino = names[ni].ino;
                let new_len = rng.below(65537) as u64;
                fs.truncate(ino, new_len).unwrap();
                let st = files.get_mut(&ino).unwrap();
                st.data.resize(new_len as usize, 0);
            }
            // 4: rename
            4 => {
                if names.is_empty() {
                    continue;
                }
                let ni = rng.below(names.len());
                let (sp, sn, ino) = {
                    let e = &names[ni];
                    (e.parent, e.name.clone(), e.ino)
                };
                let dp = dirs[rng.below(dirs.len())];
                let dn = format!("t{i:08}").into_bytes();
                if fs.lookup(dp, &dn).unwrap().is_some() {
                    continue;
                }
                fs.rename(sp, &sn, dp, &dn).unwrap();
                names[ni].parent = dp;
                names[ni].name = dn;
                let _ = ino;
            }
            // 5: unlink
            5 => {
                if names.is_empty() {
                    continue;
                }
                let ni = rng.below(names.len());
                let e = names.remove(ni);
                fs.unlink(e.parent, &e.name).unwrap();
                let st = files.get_mut(&e.ino).unwrap();
                st.refs -= 1;
                if st.refs == 0 {
                    files.remove(&e.ino);
                }
            }
            // 6: hard link
            6 => {
                if names.is_empty() {
                    continue;
                }
                let ni = rng.below(names.len());
                let ino = names[ni].ino;
                let parent = dirs[rng.below(dirs.len())];
                let name = format!("l{i:08}").into_bytes();
                if fs.lookup(parent, &name).unwrap().is_some() {
                    continue;
                }
                fs.link(ino, parent, &name).unwrap();
                files.get_mut(&ino).unwrap().refs += 1;
                names.push(Name { parent, name, ino });
            }
            // 7: mkdir (bounded)
            7 => {
                if dirs.len() >= 17 {
                    continue;
                }
                let name = format!("d{i:06}").into_bytes();
                if fs.lookup(ROOT_INO, &name).unwrap().is_some() {
                    continue;
                }
                let ino = fs.mkdir(ROOT_INO, &name, 0o755, 0, 0).unwrap();
                dirs.push(ino);
            }
            // 8: snapshot create (bounded)
            8 => {
                if snaps.len() >= 6 {
                    continue;
                }
                let name = format!("st{i:06}").into_bytes();
                if let Ok(id) = fs.snapshot_create(&name) {
                    snaps.push((id, name));
                }
            }
            // 9: snapshot delete
            9 => {
                if snaps.is_empty() {
                    continue;
                }
                let si = rng.below(snaps.len());
                let (id, _) = snaps.remove(si);
                fs.snapshot_delete(id).unwrap();
            }
            // 10: snapshot isolation probe
            _ => {
                if names.is_empty() || snaps.len() >= 6 {
                    continue;
                }
                let ni = rng.below(names.len());
                let ino = names[ni].ino;
                let before = files.get(&ino).unwrap().data.clone();
                let sname = format!("iso{i:06}").into_bytes();
                let id = fs.snapshot_create(&sname).unwrap();
                // Overwrite the live file completely.
                let new_data = vec![(i % 251) as u8; before.len().max(1024)];
                fs.write(ino, 0, &new_data).unwrap();
                files.get_mut(&ino).unwrap().data = new_data;
                // Snapshot must still show the pre-write bytes.
                let via_snap = fs.snapshot_read(id, ino, 0, before.len()).unwrap();
                assert_eq!(via_snap, before, "snapshot isolation violated at op {i}");
                // And the name must still resolve in the snapshot.
                let entry = &names[ni];
                let found = fs.snapshot_lookup(id, entry.parent, &entry.name).unwrap();
                assert_eq!(found.map(|(f, _)| f), Some(ino));
                fs.snapshot_delete(id).unwrap();
            }
        }

        // Commit at random intervals to vary the dirty/clean mix.
        if i % (1 + rng.below(20)) == 0 {
            fs.commit().unwrap();
        }

        // Periodically reopen and fsck.
        if i > 0 && i % 5000 == 0 {
            fs.commit().unwrap();
            drop(fs);
            fs = Fs::open(&img).unwrap();
            fs.check().expect("check failed mid-stress");
            println!("stress: {i}/{n} ops, check clean");
        }
    }
    fs.commit().unwrap();

    // Byte-exact verification of every surviving name.
    for e in &names {
        let found = fs.lookup(e.parent, &e.name).unwrap();
        assert_eq!(
            found.map(|(ino, _)| ino),
            Some(e.ino),
            "lookup mismatch for {:?}",
            String::from_utf8_lossy(&e.name)
        );
        let st = files.get(&e.ino).expect("shadow missing ino");
        let attr = fs.getattr(e.ino).unwrap();
        assert_eq!(
            attr.size,
            st.data.len() as u64,
            "size mismatch ino {}",
            e.ino
        );
        let data = fs.read(e.ino, 0, st.data.len()).unwrap();
        assert_eq!(data, st.data, "data mismatch ino {}", e.ino);
    }

    fs.check().expect("final check failed");
    println!(
        "stress: {n} ops, {} live names, {} live inodes, {} snapshots, all verified",
        names.len(),
        files.len(),
        snaps.len()
    );

    drop(fs);
    std::fs::remove_file(&img).unwrap();
}
