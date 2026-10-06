//! cownfs NFSv4.0 server: serves an image file over TCP (default 127.0.0.1:2049).
use std::env;

use cownfs_core::engine::Fs;
use cownfs_nfs::server;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cownfs-server [--read-only] [--ds-addr <addr>] [--referrals <file>] [--node-id <id>] [--server-id <u32>] [--lease-ttl <secs>] [--snapshot-policy <spec>] [--txg-interval-ms <ms>] [--grace-period-secs <s>] [--state-log-addr <addr>] [--tail-state <addr>] [--promote-on-primary-loss] [--max-clients <n>] [--overflow-addr <addr>] [--state-wal <path>] [--quota <uid>:<blocks>]... <image> [addr]");
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
    // Server id qualifying issued clientids/stateids
    // (docs/v40-state-partitioning.md §4.2). Explicit --server-id wins;
    // otherwise derive one from --node-id (FNV-1a hash, folded to 32 bits);
    // otherwise 0 (single-server default). Primary/standby pairs in an HA
    // setup must use distinct ids — prefer explicit --server-id there.
    fn fnv1a_64(s: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }
    let server_id_arg = args
        .iter()
        .position(|a| a == "--server-id")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let server_id: u32 = server_id_arg
        .as_deref()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            node_id
                .as_ref()
                .map(|n| ((fnv1a_64(n) ^ (fnv1a_64(n) >> 32)) as u32).max(1))
        })
        .unwrap_or(0);
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
    // Grace period after (re)start (RFC 7530 §8.4): reclaim accepted, new
    // state establishment gets NFS4ERR_GRACE. Default 90s (the NFS lease
    // duration). 0 disables it (reclaim impossible; clients re-establish
    // fresh) — only for single-writer dev/test setups.
    let grace_period_secs: u64 = args
        .iter()
        .position(|a| a == "--grace-period-secs")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    let grace_period_arg = args
        .iter()
        .position(|a| a == "--grace-period-secs")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --state-log-addr <addr>: serve the state mutation log for standby
    // tailing (§4.3 step 4). Primary side.
    let state_log_addr = args
        .iter()
        .position(|a| a == "--state-log-addr")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --tail-state <addr>: tail the primary's state log (standby side).
    let tail_state = args
        .iter()
        .position(|a| a == "--tail-state")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --promote-on-primary-loss: standby auto-promotes when the primary
    // is lost (tail connection breaks repeatedly).
    let promote_on_loss = args.iter().any(|a| a == "--promote-on-primary-loss");
    // --max-clients <n>: admission control (§4.1 step 5). At the cap, new
    // SETCLIENTID gets NFS4ERR_DELAY. 0 (default) = unlimited.
    let max_clients_arg: Option<String> = args
        .iter()
        .position(|a| a == "--max-clients")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let max_clients: usize = max_clients_arg
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // --overflow-addr <addr>: when at the client cap, redirect fresh mounts
    // (LOOKUP on root) to this server via NFS4ERR_MOVED.
    let overflow_addr: Option<String> = args
        .iter()
        .position(|a| a == "--overflow-addr")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // --state-wal <path>: persistent state WAL (§4.4 step 6). Mutations are
    // fsynced to the WAL; on restart the WAL is replayed instead of
    // forcing clients through reclaim.
    let state_wal_path: Option<String> = args
        .iter()
        .position(|a| a == "--state-wal")
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
                && *a != "--server-id"
                && *a != "--lease-ttl"
                && *a != "--log-level"
                && *a != "--snapshot-policy"
                && *a != "--txg-interval-ms"
                && *a != "--grace-period-secs"
                && *a != "--state-log-addr"
                && *a != "--tail-state"
                && *a != "--promote-on-primary-loss"
                && *a != "--max-clients"
                && *a != "--overflow-addr"
                && *a != "--state-wal"
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
                && Some(*a) != server_id_arg.as_ref()
                && Some(*a) != grace_period_arg.as_ref()
                && Some(*a) != state_log_addr.as_ref()
                && Some(*a) != tail_state.as_ref()
                && Some(*a) != max_clients_arg.as_ref()
                && Some(*a) != overflow_addr.as_ref()
                && Some(*a) != state_wal_path.as_ref()
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
    shared.set_server_id(server_id);
    eprintln!("server-id {server_id} (qualifies clientids/stateids)");
    if max_clients > 0 {
        shared.state.set_max_clients(max_clients);
        eprintln!("admission control: max {max_clients} clients");
    }
    let mut shared = shared;
    if let Some(addr) = overflow_addr {
        shared.set_overflow_addr(addr.clone());
        eprintln!("admission control overflow -> {addr}");
    }
    // Persistent state WAL (§4.4 step 6). If the WAL exists, adopt its
    // identity and replay it; otherwise write a fresh header.
    if let Some(wal_path) = state_wal_path {
        let wal_path = std::path::PathBuf::from(wal_path);
        let (mut wal, existing) =
            cownfs_nfs::state_wal::StateWal::open(&wal_path).expect("open state WAL");
        match existing {
            Some((wal_server_id, wal_boot_gen)) => {
                // Adopt the WAL's identity so replayed stateids stay valid.
                shared.set_server_id(wal_server_id);
                shared.state.set_boot_gen(wal_boot_gen);
                let n = cownfs_nfs::state_wal::StateWal::replay(&wal_path, &shared.state)
                    .expect("replay state WAL");
                eprintln!(
                    "state WAL replayed: {n} records (server-id {wal_server_id}, boot_gen {wal_boot_gen})"
                );
            }
            None => {
                wal.write_header(shared.state.server_id(), shared.state.boot_gen())
                    .expect("write WAL header");
                eprintln!("state WAL initialized at {}", wal_path.display());
            }
        }
        shared.state.set_wal(wal);
    }
    // RFC 7530 §8.4: every (re)start enters the grace period so clients can
    // reclaim pre-restart state. 0 disables it (tests/development).
    if grace_period_secs == 0 {
        eprintln!("grace period disabled");
    } else {
        shared.enter_grace_period_for(std::time::Duration::from_secs(grace_period_secs));
        eprintln!("grace period {grace_period_secs}s");
    }
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

    // State mutation log tail server (primary side, §4.3 step 4).
    if let Some(log_addr) = state_log_addr {
        let log_state = shared.state.clone();
        eprintln!("state log tail server on {log_addr}");
        std::thread::spawn(move || {
            if let Err(e) = cownfs_nfs::state_log::serve_tail(&log_addr, log_state) {
                eprintln!("state log server error: {e}");
            }
        });
    }

    // State log tail client (standby side, §4.3 step 4).
    if let Some(primary) = tail_state {
        let tail_state = shared.state.clone();
        let promote_shared = shared.clone();
        eprintln!("tailing state log from {primary}");
        std::thread::spawn(move || {
            cownfs_nfs::state_log::tail_forever(&primary, tail_state, 1, move || {
                if promote_on_loss {
                    eprintln!("promoting to primary!");
                    if let Err(e) = promote_shared.promote() {
                        eprintln!("promotion failed: {e}");
                    } else {
                        eprintln!("promoted: now serving as primary");
                    }
                } else {
                    eprintln!("primary lost; not promoting (--promote-on-primary-loss not set)");
                }
            });
        });
    }

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
