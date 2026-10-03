//! C3: connection-scale measurement — many concurrent clients.

#[path = "common/mod.rs"]
mod common;

use common::{establish_client, spawn_server, NfsClient, Ops};
use cownfs_nfs::nfs4::*;
use std::time::Instant;

#[test]
fn connection_scale_100_clients() {
    let srv = spawn_server(4096);
    // Create a file via the server's fs? We need a file to read.
    // For simplicity, just test connection establishment and getattr.

    let start = Instant::now();
    let mut handles = Vec::new();
    // 100 concurrent clients, each doing 10 getattrs.
    for i in 0..100 {
        let addr = srv.addr.clone();
        handles.push(std::thread::spawn(move || {
            let mut c = NfsClient::connect(&addr);
            let _id = establish_client(&mut c, format!("client{i}").as_bytes());
            for _ in 0..10 {
                let mut ops = Ops::new();
                ops.putrootfh();
                ops.getattr(&[FATTR4_SIZE]);
                let (status, _) = c.call(b"getattr", ops);
                assert_eq!(status, NFS4_OK);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = start.elapsed();
    println!("100 clients x 10 getattrs: {elapsed:?}");
    // Should complete in reasonable time (< 60s). If thread-per-connection
    // collapses, this will time out or be very slow.
    assert!(
        elapsed.as_secs() < 60,
        "too slow: {elapsed:?}, thread-per-connection may be collapsing"
    );
}
