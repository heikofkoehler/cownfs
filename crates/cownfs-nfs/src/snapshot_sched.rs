//! Telescoping snapshot scheduler, NetApp-style.
//!
//! A [`SnapshotPolicy`] is an ordered list of retention tiers (hourly,
//! daily, weekly, ...). A background thread in `cownfs-server` ticks once
//! a minute and calls [`SnapshotPolicy::run_once`]: for each tier it
//! creates a timestamped snapshot when the tier is due, then prunes the
//! tier's oldest snapshots beyond its keep count.
//!
//! Snapshot names are `{tier}-YYYYMMDD-HHMMSS` (UTC), e.g.
//! `hourly-20261003-230000`. Only snapshots carrying a tier prefix are
//! ever created or deleted by the scheduler; user snapshots (created via
//! `cownfs-snapshot` or a future NFS op) are left alone.

use cownfs_core::engine::{Fs, FsError};
use std::time::{SystemTime, UNIX_EPOCH};

/// One retention tier: snapshots every `interval_secs`, keeping the newest `keep`.
#[derive(Debug, Clone)]
pub struct Tier {
    /// Tier name, also the snapshot name prefix minus the trailing '-'.
    pub name: String,
    pub interval_secs: u64,
    pub keep: usize,
}

impl Tier {
    fn prefix(&self) -> String {
        format!("{}-", self.name)
    }
}

/// What the scheduler did during one [`SnapshotPolicy::run_once`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedEvent {
    Created {
        tier: String,
        name: String,
        id: u64,
    },
    Pruned {
        tier: String,
        name: String,
        id: u64,
    },
    /// Tried to create but a snapshot with the generated name already existed.
    SkippedCollision {
        tier: String,
        name: String,
    },
}

/// A telescoping snapshot retention policy.
#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    pub tiers: Vec<Tier>,
}

impl SnapshotPolicy {
    /// NetApp-style default: 24 hourly, 7 daily, 4 weekly.
    pub fn netapp_default() -> Self {
        SnapshotPolicy {
            tiers: vec![
                Tier {
                    name: "hourly".into(),
                    interval_secs: 3600,
                    keep: 24,
                },
                Tier {
                    name: "daily".into(),
                    interval_secs: 86400,
                    keep: 7,
                },
                Tier {
                    name: "weekly".into(),
                    interval_secs: 7 * 86400,
                    keep: 4,
                },
            ],
        }
    }

    /// Parse a policy spec like `"hourly:24,daily:7,weekly:4"`.
    ///
    /// Each entry is `<tier>[:keep]`. Known tiers and their intervals:
    /// `hourly` (3600s), `daily` (86400s), `weekly` (604800s),
    /// `monthly` (2592000s = 30d). Omitting `:keep` selects a default
    /// (hourly 24, daily 7, weekly 4, monthly 12).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut tiers = Vec::new();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (name, keep) = match entry.split_once(':') {
                Some((n, k)) => {
                    let k: usize = k
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad keep count in {entry:?}"))?;
                    if k == 0 {
                        return Err(format!("keep count must be > 0 in {entry:?}"));
                    }
                    (n.trim(), k)
                }
                None => (entry, default_keep(entry)?),
            };
            let interval_secs = tier_interval(name)?;
            tiers.push(Tier {
                name: name.to_string(),
                interval_secs,
                keep,
            });
        }
        if tiers.is_empty() {
            return Err("empty snapshot policy".to_string());
        }
        Ok(SnapshotPolicy { tiers })
    }

    /// Run one scheduling pass at time `now`.
    ///
    /// For each tier: create a snapshot if the tier is due (no snapshot
    /// yet, or the newest is at least `interval_secs` old), then delete
    /// the tier's oldest snapshots beyond `keep`. Does not commit; the
    /// caller persists if the returned event list is non-empty.
    pub fn run_once(&self, fs: &mut Fs, now: SystemTime) -> Result<Vec<SchedEvent>, FsError> {
        let mut events = Vec::new();
        let all = fs.snapshot_list()?;
        for tier in &self.tiers {
            let prefix = tier.prefix();
            // (id, name, parsed timestamp or None)
            let mut snaps: Vec<(u64, String, Option<u64>)> = all
                .iter()
                .filter(|(_, n)| n.starts_with(prefix.as_bytes()))
                .map(|(id, n)| {
                    let name = String::from_utf8_lossy(n).into_owned();
                    let ts = name.strip_prefix(&prefix).and_then(parse_stamp);
                    (*id, name, ts)
                })
                .collect();

            // Due check: newest parseable snapshot per tier.
            let newest_ts = snaps.iter().filter_map(|(_, _, ts)| *ts).max();
            let now_secs = system_secs(now);
            let due = match (newest_ts, now_secs) {
                (_, None) => false,      // clock broken; do nothing
                (None, Some(_)) => true, // first snapshot for this tier
                (Some(ts), Some(now)) => now.saturating_sub(ts) >= tier.interval_secs,
            };
            if due {
                if let Some(now_s) = now_secs {
                    let name = format!("{prefix}{}", format_stamp(now_s));
                    if snaps.iter().any(|(_, n, _)| *n == name) {
                        events.push(SchedEvent::SkippedCollision {
                            tier: tier.name.clone(),
                            name,
                        });
                    } else {
                        let id = fs.snapshot_create(name.as_bytes())?;
                        events.push(SchedEvent::Created {
                            tier: tier.name.clone(),
                            name: name.clone(),
                            id,
                        });
                        snaps.push((id, name, Some(now_s)));
                    }
                }
            }

            // Prune: keep the newest `keep` by (timestamp, id); unparseable
            // names sort as oldest so they rotate out first.
            snaps.sort_by_key(|s| (s.2.unwrap_or(0), s.0));
            while snaps.len() > tier.keep {
                let (id, name, _) = snaps.remove(0);
                fs.snapshot_delete(id)?;
                events.push(SchedEvent::Pruned {
                    tier: tier.name.clone(),
                    name,
                    id,
                });
            }
        }
        Ok(events)
    }
}

fn tier_interval(name: &str) -> Result<u64, String> {
    match name {
        "hourly" => Ok(3600),
        "daily" => Ok(86400),
        "weekly" => Ok(604800),
        "monthly" => Ok(30 * 86400),
        _ => Err(format!(
            "unknown snapshot tier {name:?} (want hourly|daily|weekly|monthly)"
        )),
    }
}

fn default_keep(name: &str) -> Result<usize, String> {
    match name {
        "hourly" => Ok(24),
        "daily" => Ok(7),
        "weekly" => Ok(4),
        "monthly" => Ok(12),
        _ => Err(format!(
            "unknown snapshot tier {name:?} (want hourly|daily|weekly|monthly)"
        )),
    }
}

fn system_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// Format seconds-since-epoch as UTC `YYYYMMDD-HHMMSS`.
fn format_stamp(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let secs_of_day = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Parse UTC `YYYYMMDD-HHMMSS` back to seconds-since-epoch. Returns None
/// on any malformed input.
fn parse_stamp(s: &str) -> Option<u64> {
    if s.len() != 15 {
        return None;
    }
    let b = s.as_bytes();
    if b[8] != b'-' {
        return None;
    }
    let digits = |lo: usize, hi: usize| -> Option<u64> {
        let mut v: u64 = 0;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as u64;
        }
        Some(v)
    };
    let (y, mo, d) = (digits(0, 4)? as i64, digits(4, 6)?, digits(6, 8)?);
    let (hh, mm, ss) = (digits(9, 11)?, digits(11, 13)?, digits(13, 15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as i64, d as i64)?;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

// Howard Hinnant's civil-date algorithms.
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u64;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<u64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn stamp_roundtrip() {
        for secs in [0, 1, 86399, 86400, 1_700_000_000, 1_789_123_456] {
            let s = format_stamp(secs);
            assert_eq!(parse_stamp(&s), Some(secs), "stamp {s}");
        }
        // Known date: 2026-10-03 00:00:00 UTC = 1790985600.
        assert_eq!(format_stamp(1790985600), "20261003-000000");
    }

    #[test]
    fn stamp_rejects_garbage() {
        for bad in [
            "",
            "hourly-",
            "20261003_000000",
            "20261301-000000",
            "20261032-000000",
            "20261003-240000",
            "2026100-000000",
            "abcdefgh-ijklmn",
        ] {
            assert_eq!(parse_stamp(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_policy() {
        let p = SnapshotPolicy::parse("hourly:24,daily:7,weekly:4").unwrap();
        assert_eq!(p.tiers.len(), 3);
        assert_eq!(p.tiers[0].interval_secs, 3600);
        assert_eq!(p.tiers[0].keep, 24);
        assert_eq!(p.tiers[2].name, "weekly");

        let p = SnapshotPolicy::parse("daily").unwrap();
        assert_eq!(p.tiers[0].keep, 7);

        assert!(SnapshotPolicy::parse("").is_err());
        assert!(SnapshotPolicy::parse("minutely:5").is_err());
        assert!(SnapshotPolicy::parse("hourly:0").is_err());
        assert!(SnapshotPolicy::parse("hourly:abc").is_err());
    }

    #[test]
    fn creates_when_due_and_prunes() {
        let dir = std::env::temp_dir();
        let img = dir.join(format!("cownfs-sched-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&img);
        let mut fs = Fs::format(&img, 256).unwrap();

        let policy = SnapshotPolicy {
            tiers: vec![Tier {
                name: "hourly".into(),
                interval_secs: 3600,
                keep: 2,
            }],
        };
        // First run: nothing exists -> creates.
        let ev = policy.run_once(&mut fs, t(1_000_000)).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], SchedEvent::Created { tier, .. } if tier == "hourly"));

        // 10 minutes later: not due, no events.
        let ev = policy.run_once(&mut fs, t(1_000_600)).unwrap();
        assert!(ev.is_empty());

        // Just over an hour later: due again -> creates, still within keep.
        let ev = policy.run_once(&mut fs, t(1_003_601)).unwrap();
        assert_eq!(ev.len(), 1);

        // Another hour: creates a third, prunes the oldest (keep=2).
        let ev = policy.run_once(&mut fs, t(1_007_202)).unwrap();
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().any(|e| matches!(e, SchedEvent::Created { .. })));
        assert!(ev
            .iter()
            .any(|e| matches!(e, SchedEvent::Pruned { name, .. } if name.starts_with("hourly-"))));

        let names: Vec<String> = fs
            .snapshot_list()
            .unwrap()
            .into_iter()
            .map(|(_, n)| String::from_utf8_lossy(&n).into_owned())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|n| n.starts_with("hourly-")));

        // A user snapshot without a tier prefix is never touched.
        fs.snapshot_create(b"manual-backup").unwrap();
        let ev = policy.run_once(&mut fs, t(1_010_803)).unwrap();
        assert!(ev.iter().any(|e| matches!(e, SchedEvent::Created { .. })));
        let names: Vec<String> = fs
            .snapshot_list()
            .unwrap()
            .into_iter()
            .map(|(_, n)| String::from_utf8_lossy(&n).into_owned())
            .collect();
        assert!(names.contains(&"manual-backup".to_string()));
        assert_eq!(names.len(), 3); // 2 hourly + manual
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn unparseable_tier_names_prune_first() {
        let dir = std::env::temp_dir();
        let img = dir.join(format!("cownfs-sched2-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&img);
        let mut fs = Fs::format(&img, 256).unwrap();
        // Simulate a foreign snapshot under our prefix (e.g. hand-made).
        fs.snapshot_create(b"hourly-handmade").unwrap();

        let policy = SnapshotPolicy {
            tiers: vec![Tier {
                name: "hourly".into(),
                interval_secs: 3600,
                keep: 1,
            }],
        };
        // Due (no parseable snapshot): creates a proper one, prunes the
        // unparseable one since keep=1.
        let ev = policy.run_once(&mut fs, t(2_000_000)).unwrap();
        assert!(ev
            .iter()
            .any(|e| matches!(e, SchedEvent::Pruned { name, .. } if name == "hourly-handmade")));
        let _ = std::fs::remove_file(&img);
    }
}
