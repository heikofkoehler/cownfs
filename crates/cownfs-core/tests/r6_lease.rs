//! R6: robust leader lease.
//!
//! Exit criteria:
//! 1. Two-process race: only one winner.
//! 2. kill -9 the primary → standby takes over within 2× TTL.
//! 3. A stale primary's commit is rejected (fencing).
//!
//! The lease lives on a dedicated block (not the superblock slots), lease
//! I/O uses O_DIRECT where supported, and every durable commit checks the
//! fencing epoch.
//!
//! Process-based tests re-exec this test binary with `COWNFS_R6_HELPER`
//! set; the `r6_helper_entry` test (selected via a filter arg) runs the
//! helper and exits.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use cownfs_core::engine::{Fs, FsError, ROOT_INO};

fn tmp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "cownfs-r6-{tag}-{}-{}.img",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn this_test_exe() -> PathBuf {
    std::env::current_exe().expect("test exe")
}

/// Run the helper named by `COWNFS_R6_HELPER`. Never returns.
fn run_helper() -> ! {
    let mode = std::env::var("COWNFS_R6_HELPER").expect("helper mode");
    let img = std::env::var("COWNFS_R6_IMG").expect("helper img");
    let node = std::env::var("COWNFS_R6_NODE").expect("helper node");
    let ttl: u64 = std::env::var("COWNFS_R6_TTL")
        .expect("helper ttl")
        .parse()
        .unwrap();
    let img = Path::new(&img);
    match mode.as_str() {
        // Acquire once; write "true"/"false" to COWNFS_R6_RESULT.
        "acquire" => {
            let result = std::env::var("COWNFS_R6_RESULT").expect("helper result");
            let mut fs = Fs::open(img).expect("helper open");
            let won = fs.lease_acquire(&node, ttl).expect("helper acquire");
            std::fs::write(result, if won { "true" } else { "false" }).unwrap();
        }
        // Acquire, then sleep forever (the test SIGKILLs us).
        "hold" => {
            let mut fs = Fs::open(img).expect("helper open");
            assert!(fs.lease_acquire(&node, ttl).expect("helper acquire"));
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        // Poll acquire until success or COWNFS_R6_DEADLINE secs; write
        // elapsed secs ("timeout" on failure).
        "acquire-loop" => {
            let result = std::env::var("COWNFS_R6_RESULT").expect("helper result");
            let deadline: u64 = std::env::var("COWNFS_R6_DEADLINE")
                .expect("helper deadline")
                .parse()
                .unwrap();
            let start = Instant::now();
            loop {
                let mut fs = Fs::open(img).expect("helper open");
                if fs.lease_acquire(&node, ttl).expect("helper acquire") {
                    std::fs::write(result, format!("{:.1}", start.elapsed().as_secs_f64()))
                        .unwrap();
                    break;
                }
                if start.elapsed() > Duration::from_secs(deadline) {
                    std::fs::write(result, "timeout").unwrap();
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        _ => panic!("unknown helper mode {mode}"),
    }
    std::process::exit(0);
}

/// This test is the helper entry point: when the binary is re-executed
/// with COWNFS_R6_HELPER set (and this test selected via filter), run the
/// helper instead of the test suite.
#[test]
fn r6_helper_entry() {
    if std::env::var("COWNFS_R6_HELPER").is_ok() {
        run_helper();
    }
    // Normal test run: no-op.
}

fn spawn_helper(mode: &str, img: &str, node: &str, ttl: &str, extra: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(this_test_exe());
    cmd.arg("r6_helper_entry")
        .env("COWNFS_R6_HELPER", mode)
        .env("COWNFS_R6_IMG", img)
        .env("COWNFS_R6_NODE", node)
        .env("COWNFS_R6_TTL", ttl);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn helper")
}

/// R6.1: two processes race for the lease; exactly one wins.
#[test]
fn r6_race_only_one_wins() {
    let img = tmp_path("race");
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 2048).expect("format");
    let img_s = img.to_str().unwrap().to_owned();

    let r1 = tmp_path("race-r1").to_str().unwrap().to_owned();
    let r2 = tmp_path("race-r2").to_str().unwrap().to_owned();
    let _ = std::fs::remove_file(&r1);
    let _ = std::fs::remove_file(&r2);

    // Start both racers as close together as possible.
    let mut c1 = spawn_helper(
        "acquire",
        &img_s,
        "node-A",
        "60",
        &[("COWNFS_R6_RESULT", &r1)],
    );
    let mut c2 = spawn_helper(
        "acquire",
        &img_s,
        "node-B",
        "60",
        &[("COWNFS_R6_RESULT", &r2)],
    );
    assert!(c1.wait().expect("wait c1").success());
    assert!(c2.wait().expect("wait c2").success());

    let w1 = std::fs::read_to_string(&r1).unwrap();
    let w2 = std::fs::read_to_string(&r2).unwrap();
    // Exactly one winner.
    assert!(
        (w1 == "true") != (w2 == "true"),
        "race: expected exactly one winner, got {w1:?} vs {w2:?}"
    );

    // The winner really holds it.
    let fs = Fs::open(&img).expect("open");
    let winner = if w1 == "true" { "node-A" } else { "node-B" };
    assert!(fs.lease_check(winner).expect("check"));

    for p in [&img, Path::new(&r1), Path::new(&r2)] {
        let _ = std::fs::remove_file(p);
    }
}

/// R6.2: kill -9 the primary; the standby takes over within 2× TTL.
#[test]
fn r6_failover_within_two_ttl() {
    let img = tmp_path("failover");
    let _ = std::fs::remove_file(&img);
    Fs::format(&img, 2048).expect("format");
    let img_s = img.to_str().unwrap().to_owned();
    let result = tmp_path("failover-r").to_str().unwrap().to_owned();
    let _ = std::fs::remove_file(&result);

    // Primary acquires with TTL=3 and holds until killed.
    let mut primary = spawn_helper("hold", &img_s, "primary", "3", &[]);
    // Wait until the primary actually holds the lease.
    let start = Instant::now();
    loop {
        let fs = Fs::open(&img).expect("open");
        if fs.lease_check("primary").unwrap_or(false) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "primary never acquired"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // SIGKILL the primary (no cleanup, no release).
    kill9(primary.id());
    // Reap (ignore the signal status).
    let _ = primary.wait();

    // Standby polls; it can only win after the 3s TTL expires.
    // Must succeed within 2× TTL = 6s of the kill.
    let kill_at = Instant::now();
    let mut standby = spawn_helper(
        "acquire-loop",
        &img_s,
        "standby",
        "3",
        &[
            ("COWNFS_R6_RESULT", result.as_str()),
            ("COWNFS_R6_DEADLINE", "10"),
        ],
    );
    assert!(standby.wait().expect("wait standby").success());
    let out = std::fs::read_to_string(&result).unwrap();
    let won_after: f64 = out
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("standby should win, got {out:?}"));
    let total = kill_at.elapsed().as_secs_f64();
    println!("R6: standby took over {won_after:.1}s after poll start ({total:.1}s after kill)");
    assert!(
        total <= 6.5,
        "failover took {total:.1}s, exceeding 2x TTL (6s)"
    );
    assert!(
        won_after >= 2.0,
        "standby won too early ({won_after:.1}s < TTL 3s): lease not respected"
    );

    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&result);
}

fn kill9(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: kill(2) with SIGKILL.
    unsafe {
        assert_eq!(kill(pid as i32, 9), 0);
    }
}

/// R6.3: a stale primary's commit is rejected after losing the lease.
#[test]
fn r6_stale_primary_commit_rejected() {
    let img = tmp_path("stale");
    let _ = std::fs::remove_file(&img);

    // A acquires with a short TTL and stages a write (no commit yet).
    let mut fs_a = Fs::format(&img, 2048).expect("format");
    assert!(fs_a.lease_acquire("node-A", 2).expect("A acquire"));
    let ino = fs_a.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs_a.write(ino, 0, b"stale data").expect("write");

    // Let A's lease expire; B takes over (epoch bumps).
    std::thread::sleep(Duration::from_secs(3));
    let mut fs_b = Fs::open(&img).expect("open B");
    assert!(fs_b.lease_acquire("node-B", 60).expect("B acquire"));
    assert!(fs_b.lease_check("node-B").expect("B check"));

    // A's commit must be rejected: its epoch is stale.
    match fs_a.commit() {
        Err(FsError::Fenced) => {}
        other => panic!("expected Fenced, got {other:?}"),
    }

    // B (current holder) can still commit.
    let ino_b = fs_b.create(ROOT_INO, b"g", 0o644, 0, 0).expect("create B");
    fs_b.write(ino_b, 0, b"fresh").expect("write B");
    fs_b.commit().expect("B commit works");

    // B releases; A re-acquiring gets a fresh epoch and can commit again.
    fs_b.lease_release("node-B").expect("B release");
    assert!(fs_a.lease_acquire("node-A", 60).expect("A re-acquire"));
    fs_a.commit().expect("A commit after re-acquire works");

    let _ = std::fs::remove_file(&img);
}

/// R6.4: renew extends the lease without bumping the epoch; release frees it.
#[test]
fn r6_renew_keeps_epoch() {
    let img = tmp_path("renew");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).expect("format");

    assert!(fs.lease_acquire("n1", 2).expect("acquire"));
    // Renew before expiry: epoch unchanged, so our commits keep working.
    std::thread::sleep(Duration::from_secs(1));
    assert!(fs.lease_renew("n1", 60).expect("renew"));
    let ino = fs.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs.write(ino, 0, b"x").expect("write");
    fs.commit().expect("commit after renew works");

    // Release; another node can take it immediately.
    fs.lease_release("n1").expect("release");
    let mut fs2 = Fs::open(&img).expect("open");
    assert!(fs2.lease_acquire("n2", 60).expect("n2 acquire"));
    assert!(!fs.lease_check("n1").expect("n1 lost"));

    let _ = std::fs::remove_file(&img);
}

/// R6 cross-host race: two "hosts" (separate Fs instances, no shared flock)
/// racing to acquire an empty lease. Only one may win.
/// Simulates SAN hosts by having both read-empty then write concurrently.
#[test]
fn r6_cross_host_race_one_winner() {
    let img = tmp_path("xhost");
    let _ = std::fs::remove_file(&img);
    let _fs0 = Fs::format(&img, 2048).expect("format");
    drop(_fs0);

    // Two separate Fs handles = two "hosts". They share the image file but
    // (simulating separate SAN hosts) we bypass the local flock by racing
    // the acquire calls from threads; the unique-epoch + re-read protocol
    // must ensure only one reports success.
    let img1 = img.clone();
    let img2 = img.clone();
    let h1 = std::thread::spawn(move || {
        let mut fs = Fs::open(&img1).expect("open1");
        fs.lease_acquire("hostA", 60).expect("acquire A")
    });
    let h2 = std::thread::spawn(move || {
        let mut fs = Fs::open(&img2).expect("open2");
        fs.lease_acquire("hostB", 60).expect("acquire B")
    });
    let a_won = h1.join().expect("t1");
    let b_won = h2.join().expect("t2");

    // Exactly one winner.
    assert!(
        a_won ^ b_won,
        "R6 cross-host race: exactly one must win (A={a_won}, B={b_won})"
    );

    let _ = std::fs::remove_file(&img);
}

/// R6: unique epochs — two acquisitions generate different fencing epochs.
#[test]
fn r6_epoch_unique() {
    let img = tmp_path("epochuniq");
    let _ = std::fs::remove_file(&img);
    let mut fs = Fs::format(&img, 2048).expect("format");

    assert!(fs.lease_acquire("n1", 60).expect("acquire1"));
    let e1 = fs.lease_epoch_for_test();
    fs.lease_release("n1").expect("release");

    let mut fs2 = Fs::open(&img).expect("open2");
    assert!(fs2.lease_acquire("n1", 60).expect("acquire2"));
    let e2 = fs2.lease_epoch_for_test();

    assert_ne!(e1, e2, "R6: fencing epochs must be unique per acquisition");

    let _ = std::fs::remove_file(&img);
}

/// N4: re-acquiring with the SAME node_id must fence the previous holder.
/// Two processes sharing a node_id (e.g., old/new pod with the same
/// StatefulSet name) cannot both pass fencing.
#[test]
fn r6_same_node_id_reacquire_fences() {
    let img = tmp_path("n4");
    let _ = std::fs::remove_file(&img);
    let mut fs1 = Fs::format(&img, 2048).expect("format");

    // First holder acquires.
    assert!(fs1.lease_acquire("server", 60).expect("acquire1"));
    let e1 = fs1.lease_epoch_for_test().expect("epoch1");

    // Second holder with SAME node_id acquires (simulates new pod).
    let mut fs2 = Fs::open(&img).expect("open2");
    assert!(fs2.lease_acquire("server", 60).expect("acquire2"));
    let e2 = fs2.lease_epoch_for_test().expect("epoch2");

    // Epochs must differ (N4: always bump on acquire).
    assert_ne!(e1, e2, "N4: re-acquire must generate a new epoch");

    // First holder's commit must now be rejected (fenced).
    let ino = fs1.create(ROOT_INO, b"f", 0o644, 0, 0).expect("create");
    fs1.write(ino, 0, b"x").expect("write");
    match fs1.commit() {
        Err(FsError::Fenced) => {} // expected
        Ok(()) => panic!("N4: first holder's commit should be fenced after re-acquire"),
        Err(e) => panic!("N4: unexpected error {e:?}"),
    }

    // Second holder can still commit.
    let ino2 = fs2.create(ROOT_INO, b"g", 0o644, 0, 0).expect("create2");
    fs2.write(ino2, 0, b"y").expect("write2");
    fs2.commit().expect("second holder commits");

    let _ = std::fs::remove_file(&img);
}
