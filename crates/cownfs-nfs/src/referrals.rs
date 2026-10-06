//! NFSv4.0 referrals (RFC 7530 §6.4).
//!
//! A referral maps a directory to a list of (server, path) locations.
//! When LOOKUP hits a referral, the server returns NFS4ERR_MOVED and the
//! client transparently follows `fs_locations` to the real server.

use std::collections::HashMap;
use std::path::Path;

/// One target for a referral: a server and the path on that server.
#[derive(Debug, Clone)]
pub struct ReferralTarget {
    pub server: String, // hostname or IP
    pub port: u16,      // 0 = default (2049)
    pub path: String,   // absolute path on the target, e.g. "/data"
}

/// Table of referrals: directory inode -> list of targets.
/// Loaded from a simple config file: each line is
/// `<ino> <server>[:port] <path>`
#[derive(Debug, Clone, Default)]
pub struct ReferralTable {
    map: HashMap<u64, Vec<ReferralTarget>>,
}

impl ReferralTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from a config file. Lines: `<ino> <server>[:port] <path>`.
    /// Blank lines and `#` comments are ignored.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    /// Parse referral config from text. Exposed for fuzzing (T3).
    pub fn parse(text: &str) -> std::io::Result<Self> {
        let mut t = Self::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "line {}: expected '<ino> <server>[:port] <path>'",
                        lineno + 1
                    ),
                ));
            }
            let ino: u64 = parts[0].parse().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("line {}: bad inode", lineno + 1),
                )
            })?;
            let (server, port) = match parts[1].rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                    (h.to_string(), p.parse().unwrap_or(2049))
                }
                _ => (parts[1].to_string(), 2049),
            };
            t.map.entry(ino).or_default().push(ReferralTarget {
                server,
                port,
                path: parts[2].to_string(),
            });
        }
        Ok(t)
    }

    pub fn lookup(&self, ino: u64) -> Option<&[ReferralTarget]> {
        self.map.get(&ino).map(|v| v.as_slice())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_referral_config() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("cownfs-referral-{}.conf", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "# referral config").unwrap();
        writeln!(f, "100 shard0.example.com /data").unwrap();
        writeln!(f, "101 10.0.0.5:12049 /shard1").unwrap();
        drop(f);

        let t = ReferralTable::load(&p).unwrap();
        let r0 = t.lookup(100).unwrap();
        assert_eq!(r0.len(), 1);
        assert_eq!(r0[0].server, "shard0.example.com");
        assert_eq!(r0[0].port, 2049);
        assert_eq!(r0[0].path, "/data");

        let r1 = t.lookup(101).unwrap();
        assert_eq!(r1[0].server, "10.0.0.5");
        assert_eq!(r1[0].port, 12049);

        assert!(t.lookup(999).is_none());
        std::fs::remove_file(&p).ok();
    }
}
