//! P14: tarball-extraction workload over NFS.
//!
//! Reproduces the shape of `tar xzf` onto the mount: nested directories,
//! hundreds of files with mixed sizes, symlinks, then read-back verification,
//! renames and removes. A macOS kernel client crashed during a real tarball
//! extraction; this hammers the same op mix (CREATE/MKDIR/WRITE/READ/READDIR/
//! LOOKUP/SYMLINK/RENAME/REMOVE) through the userspace client to prove the
//! server side is solid.

#[path = "common/mod.rs"]
mod common;

use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::{FATTR4_SIZE, FATTR4_TYPE, FILE_SYNC4, NF4DIR};

/// Deterministic PRNG (xorshift64) so file contents are reproducible.
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
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            let n = chunk.len().min(8);
            chunk.copy_from_slice(&v[..n]);
        }
    }
}

fn write_all(c: &mut common::NfsClient, uuid: &[u8; 16], ino: u64, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + 65536).min(data.len());
        let mut ops = common::Ops::new();
        ops.putfh(uuid, ino);
        ops.write(off as u64, FILE_SYNC4, &data[off..end]);
        c.check_ok(b"tarball-write", ops);
        off = end;
    }
}

fn mkdir(c: &mut common::NfsClient, uuid: &[u8; 16], parent: u64, name: &[u8]) -> u64 {
    let mut ops = common::Ops::new();
    ops.putfh(uuid, parent);
    ops.create_dir(name, 0o755);
    ops.getfh();
    let res = c.check_ok(b"tarball-mkdir", ops);
    match &res[2] {
        common::Reply::Fh(fh) => fh.inode,
        r => panic!("mkdir: unexpected reply {r:?}"),
    }
}

#[test]
fn tarball_extraction_workload() {
    // 1 GiB image: plenty of room for the tree.
    let srv = common::spawn_server(262144);
    let mut c = common::NfsClient::connect(&srv.addr);
    let clientid = common::establish_client(&mut c, b"tarball-test");
    let mut rng = Rng(0x12345678);

    // ---- build a source-tarball-like tree ----
    // pkg/
    //   src/      (60 .c files, mixed sizes)
    //   include/  (30 .h files, small)
    //   docs/     (10 .md files)
    //   tests/    (20 files)
    //   lib/      (symlinks like libfoo.so -> libfoo.so.2)
    //   deep/a/b/c/d/e/ (deep nesting)
    //   empty files, 1 MiB blob
    let pkg = mkdir(&mut c, &srv.uuid, ROOT_INO, b"pkg");
    let src = mkdir(&mut c, &srv.uuid, pkg, b"src");
    let include = mkdir(&mut c, &srv.uuid, pkg, b"include");
    let docs = mkdir(&mut c, &srv.uuid, pkg, b"docs");
    let tests = mkdir(&mut c, &srv.uuid, pkg, b"tests");
    let lib = mkdir(&mut c, &srv.uuid, pkg, b"lib");

    // Deep nesting: pkg/deep/a/b/c/d/e
    let mut deep = mkdir(&mut c, &srv.uuid, pkg, b"deep");
    for name in [b"a".as_slice(), b"b", b"c", b"d", b"e"] {
        deep = mkdir(&mut c, &srv.uuid, deep, name);
    }

    // (path-in-tree, dir_ino, size) for every file; contents regenerated
    // deterministically for verification.
    let mut files: Vec<(String, u64, usize)> = Vec::new();

    let mut make_files = |c: &mut common::NfsClient,
                          rng: &mut Rng,
                          dir_ino: u64,
                          dir_label: &str,
                          prefix: &str,
                          ext: &str,
                          count: usize,
                          sizes: &[usize]| {
        for i in 0..count {
            let name = format!("{prefix}{i:03}{ext}");
            let size = sizes[i % sizes.len()];
            let ino = common::create_file(c, &srv.uuid, clientid, dir_ino, name.as_bytes(), 0o644);
            if size > 0 {
                let mut data = vec![0u8; size];
                rng.fill(&mut data);
                write_all(c, &srv.uuid, ino, &data);
            }
            files.push((format!("{dir_label}/{name}"), ino, size));
        }
    };

    // Mixed sizes: empty, tiny, 4 KiB, 64 KiB, 256 KiB
    let sizes = [0usize, 17, 4096, 65536, 262144];
    make_files(&mut c, &mut rng, src, "pkg/src", "mod", ".c", 60, &sizes);
    make_files(
        &mut c,
        &mut rng,
        include,
        "pkg/include",
        "hdr",
        ".h",
        30,
        &[0, 128, 1024],
    );
    make_files(
        &mut c,
        &mut rng,
        docs,
        "pkg/docs",
        "doc",
        ".md",
        10,
        &[512, 4096],
    );
    make_files(
        &mut c,
        &mut rng,
        tests,
        "pkg/tests",
        "t",
        ".c",
        20,
        &[256, 8192],
    );
    make_files(
        &mut c,
        &mut rng,
        deep,
        "pkg/deep/a/b/c/d/e",
        "deep",
        ".dat",
        5,
        &[100, 5000],
    );

    // One 1 MiB blob at the top level.
    let blob_ino = common::create_file(&mut c, &srv.uuid, clientid, pkg, b"blob.bin", 0o644);
    {
        let mut data = vec![0u8; 1024 * 1024];
        rng.fill(&mut data);
        write_all(&mut c, &srv.uuid, blob_ino, &data);
    }
    files.push(("pkg/blob.bin".to_string(), blob_ino, 1024 * 1024));

    // Symlinks: lib/libfoo.so -> libfoo.so.2, lib/libfoo.so.2 -> libfoo.so.2.1
    for (name, target) in [
        (b"libfoo.so.2.1".as_slice(), None),
        (b"libfoo.so.2".as_slice(), Some(b"libfoo.so.2.1".as_slice())),
        (b"libfoo.so".as_slice(), Some(b"libfoo.so.2".as_slice())),
    ] {
        if let Some(t) = target {
            let mut ops = common::Ops::new();
            ops.putfh(&srv.uuid, lib);
            ops.create_symlink(name, t);
            c.check_ok(b"tarball-symlink", ops);
        } else {
            let ino = common::create_file(&mut c, &srv.uuid, clientid, lib, name, 0o644);
            let mut data = vec![0u8; 32768];
            rng.fill(&mut data);
            write_all(&mut c, &srv.uuid, ino, &data);
            files.push((
                format!("pkg/lib/{}", String::from_utf8_lossy(name)),
                ino,
                32768,
            ));
        }
    }

    println!("created {} files + 2 symlinks across 8 dirs", files.len());

    // ---- verify: READDIR walk the whole tree ----
    fn walk(
        c: &mut common::NfsClient,
        uuid: &[u8; 16],
        ino: u64,
        path: &str,
        out: &mut Vec<String>,
    ) {
        for e in c.readdir_all(uuid, ino, 8192, &[FATTR4_TYPE]) {
            let name = String::from_utf8_lossy(&e.name).into_owned();
            let p = format!("{path}/{name}");
            out.push(p.clone());
            let t = common::attr_u32(&e.attrs, FATTR4_TYPE);
            if t == NF4DIR {
                let fh = c
                    .lookup_fh(uuid, ino, e.name.as_slice())
                    .expect("lookup dir");
                walk(c, uuid, fh.inode, &p, out);
            }
        }
    }
    let mut walked = Vec::new();
    walk(&mut c, &srv.uuid, pkg, "pkg", &mut walked);
    println!("readdir walk found {} entries under pkg/", walked.len());
    // 8 dirs (src include docs tests lib deep a b c d e = 11 actually) + files + symlinks
    assert!(
        walked.len() >= files.len() + 2,
        "walk missing entries: {} vs {} files",
        walked.len(),
        files.len()
    );

    // ---- verify: read back every file, compare sizes and contents ----
    // Contents were written with the same RNG sequence; regenerate in order.
    let mut rng2 = Rng(0x12345678);
    let mut verified = 0usize;
    let mut bytes_verified = 0u64;

    // Replay: files were created in `files` order, each consuming
    // ceil(size/8) RNG draws (fill() draws one u64 per 8 bytes).
    for (label, ino, size) in &files {
        // GETATTR size check
        let attrs = c
            .getattr(&srv.uuid, *ino, &[FATTR4_SIZE])
            .expect("getattr size");
        let got = common::attr_u64(&attrs, FATTR4_SIZE);
        assert_eq!(got as usize, *size, "size mismatch for {label}");

        // Content check
        let mut expected = vec![0u8; *size];
        rng2.fill(&mut expected);
        if *size > 0 {
            let got_data = c.read_all(&srv.uuid, *ino, *size as u64);
            assert_eq!(got_data.len(), *size, "short read for {label}");
            assert_eq!(got_data, expected, "content mismatch for {label}");
        }
        verified += 1;
        bytes_verified += *size as u64;
    }
    println!(
        "verified {verified} files, {} bytes read back OK",
        bytes_verified
    );

    // ---- mutate: rename 10 files, remove 10 files ----
    let mut ops = common::Ops::new();
    ops.putfh(&srv.uuid, src);
    for i in 0..10 {
        let old = format!("mod{i:03}.c");
        let new = format!("mod{i:03}.renamed.c");
        ops.rename(old.as_bytes(), new.as_bytes());
    }
    c.check_ok(b"tarball-rename-batch", ops);

    let mut ops = common::Ops::new();
    ops.putfh(&srv.uuid, tests);
    for i in 0..10 {
        let name = format!("t{i:03}.c");
        ops.remove(name.as_bytes());
    }
    c.check_ok(b"tarball-remove-batch", ops);

    // Confirm renames landed and removes are gone.
    for i in 0..10 {
        let new = format!("mod{i:03}.renamed.c");
        assert!(
            c.lookup_fh(&srv.uuid, src, new.as_bytes()).is_ok(),
            "renamed file missing: {new}"
        );
        let old = format!("t{i:03}.c");
        assert!(
            c.lookup_fh(&srv.uuid, tests, old.as_bytes()).is_err(),
            "removed file still present: {old}"
        );
    }

    println!("P14 tarball workload: OK");
}
