//! C2: paged readdir.

use cownfs_core::engine::{Fs, ROOT_INO};
use std::sync::atomic::{AtomicU64, Ordering};

static CTR: AtomicU64 = AtomicU64::new(0);

fn test_image() -> std::path::PathBuf {
    let n = CTR.fetch_add(1, Ordering::SeqCst);
    let img = std::env::temp_dir().join(format!("cownfs-paged-{n}.img"));
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 8192).unwrap();
    fs.commit().unwrap();
    drop(fs);
    img
}

#[test]
fn readdir_paged_iterates_all() {
    let img = test_image();
    let mut fs = Fs::open(&img).unwrap();
    // Create 100 files.
    for i in 0..100 {
        fs.create(ROOT_INO, format!("f{i:03}").as_bytes(), 0o644, 0, 0)
            .unwrap();
    }
    fs.commit().unwrap();

    // Page through with limit 10.
    let mut all = Vec::new();
    let mut start_after: Option<Vec<u8>> = None;
    loop {
        let (entries, has_more) = fs
            .readdir_paged(ROOT_INO, start_after.as_deref(), 10)
            .unwrap();
        if entries.is_empty() {
            break;
        }
        start_after = Some(entries.last().unwrap().0.clone());
        all.extend(entries);
        if !has_more {
            break;
        }
        // Safety: prevent infinite loop.
        if all.len() > 1000 {
            panic!("too many entries");
        }
    }
    assert_eq!(all.len(), 100, "should get all 100 files via paging");
    // Verify names.
    for (i, (name, _, _)) in all.iter().enumerate() {
        assert_eq!(name, &format!("f{i:03}").as_bytes().to_vec());
    }
    let _ = std::fs::remove_file(&img);
}
