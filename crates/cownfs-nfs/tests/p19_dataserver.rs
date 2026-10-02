//! Phase 3: cownfs-ds data server TCP integration test.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cownfs-ds"))
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
fn ds_write_read_checksum_over_tcp() {
    let dir = std::env::temp_dir().join(format!("cownfs-ds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("store.bin");
    let port = free_port();

    let mut ds = Command::new(bin())
        .args([store.to_str().unwrap(), &format!("127.0.0.1:{port}")])
        .spawn()
        .expect("spawn cownfs-ds");
    std::thread::sleep(Duration::from_millis(300));

    let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    // HELLO
    s.write_all(&0x4453_3031u32.to_be_bytes()).unwrap();
    s.write_all(&1u32.to_be_bytes()).unwrap();

    // WRITE block 7
    let mut data = [0u8; 4096];
    data[..11].copy_from_slice(b"ds-payload!");
    s.write_all(&[2u8]).unwrap();
    s.write_all(&7u64.to_be_bytes()).unwrap();
    s.write_all(&data).unwrap();
    assert_eq!(read_u32(&mut s), 0, "write status ok");
    let wsum = read_u64(&mut s);

    // READ block 7
    s.write_all(&[1u8]).unwrap();
    s.write_all(&7u64.to_be_bytes()).unwrap();
    assert_eq!(read_u32(&mut s), 0, "read status ok");
    let mut back = [0u8; 4096];
    s.read_exact(&mut back).unwrap();
    let rsum = read_u64(&mut s);
    assert_eq!(&back[..11], b"ds-payload!");
    assert_eq!(wsum, rsum, "write and read checksums match");

    // STATUS
    s.write_all(&[3u8]).unwrap();
    assert_eq!(read_u32(&mut s), 0);
    assert_eq!(read_u64(&mut s), 1, "one block stored");

    ds.kill().ok();
    ds.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
}
