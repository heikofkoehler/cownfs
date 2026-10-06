//! Backup stream parser (T3 fuzz target).
//!
//! Parses the cownfs backup stream format without performing I/O.
//! Exposed for fuzzing to ensure malformed inputs don't cause panics.

const MAGIC: u64 = 0x434f574e42554b50; // "COWNBUKP"
const VERSION: u32 = 1;
const INC_VERSION: u32 = 3;
const BLOCK_SIZE: usize = 4096;

/// Parse a backup stream from bytes. Returns Ok(()) if the stream is
/// structurally valid, Err with a message otherwise. Must never panic.
pub fn parse_backup_stream(data: &[u8]) -> Result<(), String> {
    let mut pos = 0usize;

    macro_rules! read_u64 {
        () => {{
            if pos + 8 > data.len() {
                return Err("truncated u64".into());
            }
            let v = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;
            v
        }};
    }

    macro_rules! read_u32 {
        () => {{
            if pos + 4 > data.len() {
                return Err("truncated u32".into());
            }
            let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            pos += 4;
            v
        }};
    }

    macro_rules! skip {
        ($n:expr) => {{
            let n = $n;
            if pos + n > data.len() {
                return Err("truncated skip".into());
            }
            pos += n;
        }};
    }

    // Header: MAGIC, VERSION, UUID, GEN, TS.
    let magic = read_u64!();
    if magic != MAGIC {
        return Err("bad magic".into());
    }
    let ver = read_u32!();
    if ver != VERSION && ver != 2 && ver != INC_VERSION {
        return Err(format!("unsupported version {ver}"));
    }
    skip!(16); // UUID
    let _gen = read_u64!();
    let _ts = read_u64!();
    if ver != VERSION {
        skip!(64); // SNAP_NAME
    }
    if ver == INC_VERSION {
        skip!(48 + 48 + 32 + 8 + 8); // v3 incremental header
    }

    // Block count.
    let nblocks = read_u64!();
    // Sanity bound: prevent huge allocation/time on fuzz input.
    if nblocks > 1_000_000 {
        return Err("nblocks too large".into());
    }

    // Each block: BLK (u64), CKSUM (u32), DATA (4096).
    for i in 0..nblocks {
        let _blk = read_u64!();
        let _cksum = read_u32!();
        skip!(BLOCK_SIZE);
        let _ = i;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fails_gracefully() {
        assert!(parse_backup_stream(&[]).is_err());
    }

    #[test]
    fn bad_magic_fails() {
        let mut data = vec![0u8; 100];
        assert!(parse_backup_stream(&data).is_err());
        // Set correct magic but truncated.
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        assert!(parse_backup_stream(&data).is_err());
    }
}
