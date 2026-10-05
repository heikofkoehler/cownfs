use cownfs_core::block::{BlockDevice, FileDevice};
use cownfs_core::engine::{Fs, ROOT_INO};
use cownfs_core::{superblock, BLOCK_SIZE};

fn main() {
    let img = std::env::temp_dir().join("t0-r3-debug.img");
    let _ = std::fs::remove_file(&img);

    let mut fs = Fs::format(&img, 4096).unwrap();
    fs.commit().unwrap();

    let ino1 = fs.create(ROOT_INO, b"f1", 0o644, 0, 0).unwrap();
    fs.write(ino1, 0, &vec![0x11u8; 8192]).unwrap();
    fs.commit().unwrap();
    println!("gen2 committed, gen={}", fs.generation());

    fs.unlink(ROOT_INO, b"f1").unwrap();
    fs.commit_async().unwrap();
    println!("gen3 staged");

    let ino2 = fs.create(ROOT_INO, b"f2", 0o644, 0, 0).unwrap();
    fs.write(ino2, 0, &vec![0x22u8; 8192]).unwrap();
    println!("f2 created");
    drop(fs);

    // Inspect.
    let dev = FileDevice::open(&img).unwrap();
    let (sb, slot) = superblock::open(&dev).unwrap();
    println!("reopened: gen={}, slot={}", sb.generation, slot);
    
    // Read both bitmap areas.
    for s in 0..2 {
        let start = sb.bitmap_start + s as u64 * sb.bitmap_blocks;
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(start, &mut blk).unwrap();
        // Print first 16 bytes as bits.
        print!("area {s}: ");
        for i in 0..16 {
            print!("{:08b}", blk[i]);
        }
        println!();
    }
    
    let _ = std::fs::remove_file(&img);
}
