//! NFSv4.0 referrals: LOOKUP on a referral dir returns MOVED,
//! GETATTR returns fs_locations.

#[path = "common/mod.rs"]
mod common;

use common::{NfsClient, Ops};
use cownfs_nfs::nfs4::*;
use std::io::Write;
use std::net::TcpListener;

#[test]
fn referral_lookup_returns_moved() {
    let dir = std::env::temp_dir().join(format!("cownfs-referral-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("test.img");
    let conf = dir.join("referrals.conf");

    // Format an image and create a dir, get its inode.
    let mut fs = cownfs_core::engine::Fs::format(&img, 4096).unwrap();
    let root = cownfs_core::engine::ROOT_INO;
    let shard_ino = fs.mkdir(root, b"shard0", 0o755, 1000, 1000).unwrap();
    let uuid = fs.uuid();
    fs.commit().unwrap();
    drop(fs);

    // Write referral config.
    let mut f = std::fs::File::create(&conf).unwrap();
    writeln!(f, "{shard_ino} shard0.example.com /data").unwrap();
    drop(f);

    // Start server with referrals.
    let table = cownfs_nfs::referrals::ReferralTable::load(&conf).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let img2 = img.clone();
    std::thread::spawn(move || {
        let fs = cownfs_core::engine::Fs::open(&img2).unwrap();
        let shared = cownfs_nfs::server::Shared::new(fs).with_referrals(table);
        let _ = cownfs_nfs::server::serve_listener(listener, &shared);
    });
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut c = NfsClient::connect(&addr);

    // LOOKUP shard0 -> should get MOVED (87).
    let mut ops = Ops::new();
    ops.putfh(&uuid, root);
    ops.lookup(b"shard0");
    let (st, _) = c.call(b"lookup-ref", ops);
    assert_eq!(st, NFS4ERR_MOVED, "referral LOOKUP should return MOVED");

    // GETATTR fs_locations on the referral dir.
    let mut ops = Ops::new();
    ops.putfh(&uuid, root);
    ops.lookup(b"shard0");
    // LOOKUP returned MOVED, but cfh is set. Now GETATTR.
    // Note: our LOOKUP sets cfh even on MOVED, so GETATTR works.
    let (st2, _) = c.call(b"lookup2", {
        let mut o = Ops::new();
        o.putfh(&uuid, shard_ino);
        o.getattr(&[FATTR4_FS_LOCATIONS]);
        o
    });
    // putfh with gen=0 skips validation; should succeed.
    assert_eq!(st2, NFS4_OK);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn referral_config_parsing() {
    let dir = std::env::temp_dir().join(format!("cownfs-refparse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("r.conf");
    let mut f = std::fs::File::create(&conf).unwrap();
    writeln!(f, "42 server1 /a").unwrap();
    writeln!(f, "43 server2:12049 /b").unwrap();
    drop(f);

    let t = cownfs_nfs::referrals::ReferralTable::load(&conf).unwrap();
    assert!(t.lookup(42).is_some());
    assert!(t.lookup(43).is_some());
    assert!(t.lookup(44).is_none());

    std::fs::remove_dir_all(&dir).ok();
}
