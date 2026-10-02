//! Replication crash-safety: superblock slots must be transmitted last,
//! so a crash before COMMIT never exposes a torn generation.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use cownfs_core::engine::{Fs, ROOT_INO};

fn test_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cownfs-repl-order-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cownfs-replicate"))
}

fn read_u32(s: &mut TcpStream) -> u32 {
    let mut b = [0u8; 4];
    s.read_exact(&mut b).unwrap();
    u32::from_be_bytes(b)
}

fn read_u64(s: &mut TcpStream) -> u64 {
    let mut b = [0u8; 8];
    s.read_exact(&mut b).unwrap();
    u64::from_be_bytes(b)
}

#[test]
fn superblock_transmitted_last() {
    let dir = test_dir("order");
    let primary = dir.join("primary.img");
    let state = dir.join("repl.state");

    let mut fs = Fs::format(&primary, 256).unwrap();
    let ino = fs.create(ROOT_INO, b"f", 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, b"data").unwrap();
    fs.commit().unwrap();
    drop(fs);

    // Mock receiver: record BLOCK IDs in arrival order, then hang up
    // before COMMIT (simulating a crash).
    let port = free_port();
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    let handle = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // HELLO
        assert_eq!(read_u32(&mut s), 0x434F_5752);
        assert_eq!(read_u32(&mut s), 1);
        // SNAPSHOT
        let mut tag = [0u8; 1];
        s.read_exact(&mut tag).unwrap();
        assert_eq!(tag[0], 1);
        // roots (4x u64+u32) + generation (u64) + uuid (16)
        let mut skip = [0u8; 4 * 12 + 8 + 16];
        s.read_exact(&mut skip).unwrap();
        let _block_count = read_u64(&mut s);
        // BLOCKs: record IDs until EOF.
        let mut ids = Vec::new();
        loop {
            let mut t = [0u8; 1];
            if s.read_exact(&mut t).is_err() {
                break;
            }
            if t[0] == 2 {
                ids.push(read_u64(&mut s));
                let mut blk = [0u8; 4096];
                s.read_exact(&mut blk).unwrap();
            } else {
                break; // COMMIT or unknown: stop recording.
            }
        }
        ids
    });
    std::thread::sleep(Duration::from_millis(200));

    // Run sender (full send, no prior state).
    let out = Command::new(bin())
        .args([
            "send",
            primary.to_str().unwrap(),
            &format!("127.0.0.1:{port}"),
            "--state",
            state.to_str().unwrap(),
        ])
        .output()
        .expect("run send");
    // Sender will fail (mock hung up), but should have sent blocks.
    let ids = handle.join().unwrap();
    assert!(!ids.is_empty(), "should have transmitted blocks");

    // Superblock slots 0 and 1 must be the LAST two blocks.
    assert!(ids.len() >= 2, "need at least the superblock slots");
    let last_two = &ids[ids.len() - 2..];
    assert!(
        last_two.contains(&0) && last_two.contains(&1),
        "superblock slots 0 and 1 must be last, got {ids:?}"
    );
    // And they must not appear earlier.
    assert!(
        !ids[..ids.len() - 2].contains(&0) && !ids[..ids.len() - 2].contains(&1),
        "superblock slots must not appear before the end"
    );

    let _ = out;
    std::fs::remove_dir_all(&dir).ok();
}
