//! P9: dirty-data backpressure — WRITEs get NFS4ERR_DELAY when uncommitted
//! dirty bytes exceed the threshold, instead of growing memory without bound.

#[path = "common/mod.rs"]
mod common;

use common::{NfsClient, Ops};
use cownfs_core::engine::ROOT_INO;
use cownfs_nfs::nfs4::{self, FILE_SYNC4, UNSTABLE4};

#[test]
fn dirty_backpressure_returns_delay_then_recovers() {
    let srv = common::spawn_server(4096);
    // Disable the background sync so dirty bytes don't get reset mid-test.
    // u64::MAX makes op_commit/op_write fall back to direct sync_txg()
    // instead of waiting on the (disabled) background thread.
    srv.shared.set_txg_interval_ms(u64::MAX);
    // Low threshold for the test.
    srv.shared
        .fs
        .read()
        .unwrap()
        .set_dirty_backpressure_threshold(100);

    let mut c = NfsClient::connect(&srv.addr);
    let clientid = common::establish_client_verifier(&mut c, &[7, 7, 7, 7, 7, 7, 7, 7], b"p9");
    let ino = common::create_file(&mut c, &srv.uuid, clientid, ROOT_INO, b"f", 0o644);

    // UNSTABLE write of 200 bytes: dirty_bytes (200) > threshold (100).
    // UNSTABLE does not commit, so dirty stays.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(0, UNSTABLE4, &vec![0xabu8; 200]);
    let (overall, _) = c.call(b"w1", ops);
    assert_eq!(overall, nfs4::NFS4_OK);

    // Next WRITE trips backpressure → NFS4ERR_DELAY.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(200, UNSTABLE4, b"x");
    let (overall, _) = c.call(b"w2", ops);
    assert_eq!(
        overall,
        nfs4::NFS4ERR_DELAY,
        "over-threshold WRITE should get DELAY"
    );

    // COMMIT makes the txg durable and resets dirty_bytes.
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.commit();
    c.check_ok(b"commit", ops);

    // WRITE succeeds again (UNSTABLE to avoid waiting on disabled bg sync).
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.write(200, UNSTABLE4, b"y");
    let (overall, _) = c.call(b"w3", ops);
    assert_eq!(overall, nfs4::NFS4_OK, "WRITE after COMMIT should succeed");

    // Data intact: 200 x 0xab, then "y".
    let mut ops = Ops::new();
    ops.putfh(&srv.uuid, ino);
    ops.read(0, 201);
    let replies = c.check_ok(b"read-back", ops);
    match &replies[1] {
        common::Reply::Read { data, .. } => {
            assert_eq!(&data[..200], &vec![0xabu8; 200][..]);
            assert_eq!(data[200], b'y');
        }
        r => panic!("read: unexpected reply {r:?}"),
    }
}
