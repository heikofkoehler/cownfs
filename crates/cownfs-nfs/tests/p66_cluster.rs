//! Hermetic multi-server NFS cluster tests.
//!
//! Uses `common::cluster::Cluster`: real `cownfs-server` processes forming
//! one namespace via referrals. The frontend refers /shard{i} to shard i;
//! a client traverses the cluster by following NFS4ERR_MOVED + fs_locations.

#[path = "common/mod.rs"]
mod common;

use common::cluster::{decode_fs_locations, Cluster};
use common::{establish_client, NfsClient, Ops, Reply};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4;

fn expect_moved(res: &[Reply], idx: usize, what: &str) {
    match &res[idx] {
        Reply::Err(s) => assert_eq!(*s, nfs4::NFS4ERR_MOVED, "{what}: wrong status"),
        r => panic!("{what}: expected MOVED, got {r:?}"),
    }
}

#[test]
fn sharded_cluster_traversal() {
    let cluster = Cluster::new_sharded(2);
    let frontend = cluster.frontend();
    let shards = cluster.shards();
    assert_eq!(shards.len(), 2);

    // Write distinct data to each shard directly (bypassing the frontend).
    for (i, shard) in shards.iter().enumerate() {
        let mut c = NfsClient::connect(&shard.addr);
        let clientid = establish_client(&mut c, b"writer");
        let name = format!("data{i}.txt");
        let ino = common::create_file(
            &mut c,
            &shard.uuid,
            clientid,
            ROOT_INO,
            name.as_bytes(),
            0o644,
        );
        let content = format!("shard-{i}-data").into_bytes();
        let mut ops = Ops::new();
        ops.putfh(&shard.uuid, ino);
        ops.write(0, 2, &content);
        c.check_ok(b"write", ops);
    }

    // Traverse via the frontend, following referrals.
    let mut c = NfsClient::connect(&frontend.addr);
    for (i, shard) in shards.iter().enumerate() {
        let dirname = format!("shard{i}");
        // LOOKUP shard dir -> MOVED (proves the referral fires).
        let mut ops = Ops::new();
        ops.putrootfh();
        ops.lookup(dirname.as_bytes());
        let res = c.call(b"lookup", ops).1;
        assert_eq!(res.len(), 2);
        expect_moved(&res, 1, "shard lookup");
        // GETATTR fs_locations via PUTFH on the shard dir inode.
        let mut ops = Ops::new();
        ops.putfh(&frontend.uuid, cluster.shard_dir_inos[i]);
        ops.getattr(&[nfs4::FATTR4_FS_LOCATIONS]);
        let res = c.call(b"getattr", ops).1;
        assert_eq!(res.len(), 2);
        let raw = match &res[1] {
            Reply::Attrs(a) => a
                .iter()
                .find(|(n, _)| *n == nfs4::FATTR4_FS_LOCATIONS)
                .expect("fs_locations attr")
                .1
                .clone(),
            r => panic!("getattr failed: {r:?}"),
        };
        let locs = decode_fs_locations(&raw);
        assert_eq!(locs.len(), 1, "one location");
        let (server, path) = &locs[0];
        assert_eq!(path, "/");
        let expected = format!("127.0.0.1:{}", shard.addr.port());
        assert_eq!(server, &expected, "referral points at shard {i}");

        // Read the file directly from the shard; verify it's the right data.
        let mut sc = NfsClient::connect(&shard.addr);
        let name = format!("data{i}.txt");
        let fh = sc
            .lookup_fh(&shard.uuid, ROOT_INO, name.as_bytes())
            .expect("lookup data file");
        let data = sc.read_all(&shard.uuid, fh.inode, 1024);
        let expected_data = format!("shard-{i}-data").into_bytes();
        assert_eq!(data, expected_data, "shard {i} data mismatch");
    }
}
