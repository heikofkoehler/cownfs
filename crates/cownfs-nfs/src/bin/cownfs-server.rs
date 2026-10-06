//! cownfs NFSv4.0 server: serves an image file over TCP (default 127.0.0.1:2049).
use std::env;

use cownfs_core::engine::Fs;
use cownfs_nfs::server;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cownfs-server [--read-only] [--ds-addr <addr>] [--referrals <file>] [--node-id <id>] [--lease-ttl <secs>] [--snapshot-policy <spec>] [--txg-interval-ms <ms>] [--quota <uid>:<blocks>]... <image> [addr]");
        std::process::exit(1);
    }
    let read_only = args.iter().any(|a| a == "--read-only");
    let ds_addr = args
        .iter()
        .position(|a| a == "--ds-addr")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let referrals_file = args
        .iter()
        .position(|a| a == "--referrals")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let node_id = args
        .iter()
        .position(|a| a == "--node-id")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let log_level = args
        .iter()
        .position(|a| a == "--log-level")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
        .unwrap_or("info");
    cownfs_nfs::log::set_level(log_level);
    let lease_ttl: u64 = args
        .iter()
        .position(|a| a == "--lease-ttl")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let snapshot_policy = args
        .iter()
        .position(|a| a == "--snapshot-policy")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let txg_interval_ms: u64 = args
        .iter()
        .position(|a| a == "--txg-interval-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let txg_interval_arg = args
        .iter()
        .position(|a| a == "--txg-interval-ms")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --quota uid:blocks (repeatable).
    let quota_args: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--quota")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();
    let quota_specs: Vec<(u32, u64)> = quota_args
        .iter()
        .filter_map(|s| {
            let (u, b) = s.split_once(':')?;
            Some((u.parse().ok()?, b.parse().ok()?))
        })
        .collect();
    let positional: Vec<&String> = args[1..]
        .iter()
        .filter(|a| {
            *a != "--read-only"
                && *a != "--ds-addr"
                && *a != "--referrals"
                && *a != "--node-id"
                && *a != "--lease-ttl"
                && *a != "--log-level"
                && *a != "--snapshot-policy"
                && *a != "--txg-interval-ms"
                && *a != "--quota"
        })
        .collect();
    // Remove the option values from positionals.
    let positional: Vec<&String> = positional
        .into_iter()
        .filter(|a| {
            Some(*a) != ds_addr.as_ref()
                && Some(*a) != referrals_file.as_ref()
                && Some(*a) != node_id.as_ref()
                && Some(*a)
                    != args
                        .iter()
                        .position(|a| a == "--lease-ttl")
                        .and_then(|i| args.get(i + 1))
                && Some(*a) != snapshot_policy.as_ref()
                && Some(*a) != txg_interval_arg.as_ref()
                && !quota_args.iter().any(|q| *a == q)
        })
        .collect();
    let addr = positional
        .get(1)
        .cloned()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:2049".into());
    let mut fs = Fs::open(std::path::Path::new(positional[0])).expect("open image");

    // Per-UID block quotas.
    for (uid, blocks) in &quota_specs {
        fs.set_quota(*uid, *blocks).unwrap();
        eprintln!("quota: uid {uid} limited to {blocks} blocks");
    }

    // Leader lease: acquire on startup if --node-id is given (and not read-only).
    // The lease prevents split-brain: two primaries cannot both hold it.
    if let Some(ref nid) = node_id {
        if read_only {
            eprintln!("warning: --node-id ignored in read-only mode");
        } else {
            match fs.lease_acquire(nid, lease_ttl) {
                Ok(true) => eprintln!("acquired write lease as '{nid}' (ttl {lease_ttl}s)"),
                Ok(false) => {
                    eprintln!("error: write lease held by another node; refusing to start");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error acquiring lease: {e:?}");
                    std::process::exit(1);
                }
            }
            // Background renewal. If we lose the lease, fence ourselves.
            let image_path = positional[0].clone();
            let nid_clone = nid.clone();
            std::thread::spawn(move || {
                let renew_interval = std::time::Duration::from_secs((lease_ttl / 3).max(1));
                loop {
                    std::thread::sleep(renew_interval);
                    let mut fs = match Fs::open(std::path::Path::new(&image_path)) {
                        Ok(fs) => fs,
                        Err(e) => {
                            eprintln!("lease renew: failed to open image: {e:?}; fencing");
                            std::process::exit(1);
                        }
                    };
                    match fs.lease_renew(&nid_clone, lease_ttl) {
                        Ok(true) => {}
                        Ok(false) => {
                            eprintln!("lease lost to another node; fencing (exiting)");
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("lease renew failed: {e:?}; fencing");
                            std::process::exit(1);
                        }
                    }
                }
            });
        }
    }

    eprintln!(
        "serving {} on {addr}{}{}",
        positional[0],
        if read_only { " (read-only)" } else { "" },
        ds_addr
            .as_ref()
            .map(|a| format!(" (ds: {a})"))
            .unwrap_or_default(),
    );
    let shared = if read_only {
        server::Shared::new_read_only(fs, Some(std::path::PathBuf::from(&positional[0])))
    } else {
        server::Shared::new(fs)
    };
    let shared = match ds_addr {
        Some(a) => shared.with_ds_addr(a),
        None => shared,
    };
    let shared = match referrals_file {
        Some(f) => {
            let table = cownfs_nfs::referrals::ReferralTable::load(std::path::Path::new(&f))
                .expect("load referrals");
            eprintln!("loaded referrals from {f}");
            shared.with_referrals(table)
        }
        None => shared,
    };

    // Metrics/health HTTP endpoint (on addr port + 1000).
    let metrics_shared = shared.clone();
    let metrics_addr = {
        let mut parts = addr.rsplitn(2, ':');
        let port: u16 = parts.next().unwrap().parse().unwrap_or(2049);
        let host = parts.next().unwrap_or("127.0.0.1");
        format!("{host}:{}", port + 1000)
    };
    std::thread::spawn(move || {
        let _ = server::serve_metrics(&metrics_addr, &metrics_shared);
    });

    // Transaction group sync interval. Shared::new already started the
    // background sync thread; just tune its interval.
    if !read_only {
        shared.set_txg_interval_ms(txg_interval_ms);
        eprintln!("txg sync every {txg_interval_ms}ms");
    }

    // Telescoping snapshot scheduler (NetApp-style). Ticks once a minute;
    // creates timestamped snapshots per tier and prunes beyond keep counts.
    if let Some(spec) = snapshot_policy {
        if read_only {
            eprintln!("warning: --snapshot-policy ignored in read-only mode");
        } else {
            match cownfs_nfs::snapshot_sched::SnapshotPolicy::parse(&spec) {
                Ok(policy) => {
                    eprintln!("snapshot scheduler: {spec}");
                    let sched_shared = shared.clone();
                    std::thread::spawn(move || {
                        let tick = std::time::Duration::from_secs(60);
                        loop {
                            std::thread::sleep(tick);
                            let now = std::time::SystemTime::now();
                            // Hold the fs lock across schedule + commit so no
                            // NFS op interleaves between them.
                            let mut fs = sched_shared.fs.write().unwrap();
                            let events = match policy.run_once(&mut fs, now) {
                                Ok(ev) => ev,
                                Err(e) => {
                                    eprintln!("snapshot scheduler: error: {e:?}");
                                    continue;
                                }
                            };
                            if events.is_empty() {
                                continue;
                            }
                            if let Err(e) = fs.commit() {
                                eprintln!("snapshot scheduler: commit failed: {e:?}");
                                continue;
                            }
                            drop(fs);
                            for ev in &events {
                                match ev {
                                    cownfs_nfs::snapshot_sched::SchedEvent::Created {
                                        name,
                                        id,
                                        ..
                                    } => {
                                        eprintln!("snapshot scheduler: created {name} (id {id})")
                                    }
                                    cownfs_nfs::snapshot_sched::SchedEvent::Pruned {
                                        name,
                                        id,
                                        ..
                                    } => {
                                        eprintln!("snapshot scheduler: pruned {name} (id {id})")
                                    }
                                    cownfs_nfs::snapshot_sched::SchedEvent::SkippedCollision {
                                        name,
                                        ..
                                    } => {
                                        eprintln!(
                                            "snapshot scheduler: name collision, skipped {name}"
                                        )
                                    }
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    eprintln!("error: bad --snapshot-policy: {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    if let Err(e) = server::serve(&addr, shared) {
        eprintln!("server error: {e:?}");
        std::process::exit(1);
    }
}
