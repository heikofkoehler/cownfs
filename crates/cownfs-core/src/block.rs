//! Raw block access: the bottom of the storage stack.
//!
//! Everything above this layer speaks in 4 KiB blocks numbered from 0.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::{Block, BLOCK_SIZE};

/// Abstraction over the raw storage holding fixed-size blocks.
pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()>;
    /// P4: takes `&self` — positional `write_all_at` needs no file offset,
    /// so concurrent writers need no mutex. Fault-injector state lives
    /// behind its own lock.
    fn write_block(&self, n: u64, buf: &Block) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

/// P4: `Arc<D>` forwards to `D`, so shared devices can be passed to
/// `&impl BlockDevice` helpers without unwrapping.
impl<D: BlockDevice> BlockDevice for std::sync::Arc<D> {
    fn block_count(&self) -> u64 {
        (**self).block_count()
    }
    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()> {
        (**self).read_block(n, buf)
    }
    fn write_block(&self, n: u64, buf: &Block) -> io::Result<()> {
        (**self).write_block(n, buf)
    }
    fn sync(&self) -> io::Result<()> {
        (**self).sync()
    }
}

/// A [`BlockDevice`] backed by a regular file (or a block device node).
pub struct FileDevice {
    file: File,
    blocks: u64,
    /// Fault injection for B3/D3 testing. None = disabled.
    /// P4: behind a Mutex so all device I/O takes `&self` (positional
    /// pread/pwrite need no file-level lock; only the injector's own
    /// small mutable state is guarded).
    faults: std::sync::Mutex<Option<FaultInjector>>,
    /// T1: operation recorder for crash-state enumeration. When armed,
    /// every `write_block` and `sync` is appended to the shared log.
    /// `Arc<Mutex<..>>` so the test can read it mid-workload (to tag
    /// durability-ledger entries with the current sync index).
    recorder: std::sync::Mutex<Option<std::sync::Arc<std::sync::Mutex<Vec<RecordedOp>>>>>,
}

/// Fault injection modes for testing crash consistency.
#[derive(Debug, Clone, Default)]
pub struct FaultInjector {
    /// If Some(n), writes only the first n bytes of each block (torn write).
    pub torn_write_bytes: Option<usize>,
    /// Probability (0.0-1.0) of flipping a random bit in each written block.
    pub bit_flip_prob: f64,
    /// If true, buffer writes and flush in reverse order on sync (reordering).
    pub reorder_writes: bool,
    /// Buffered writes when reorder_writes is true.
    buffered: Vec<(u64, Block)>,
    /// T9: If true, read_block returns EIO.
    pub fail_reads: bool,
    /// T9: If true, sync returns EIO.
    pub fail_sync: bool,
    /// T9: If Some(ms), sync sleeps for ms milliseconds (latency injection).
    pub sync_latency_ms: Option<u64>,
}

impl FaultInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_torn_writes(mut self, bytes: usize) -> Self {
        self.torn_write_bytes = Some(bytes);
        self
    }

    pub fn with_bit_flips(mut self, prob: f64) -> Self {
        self.bit_flip_prob = prob;
        self
    }

    pub fn with_reordering(mut self) -> Self {
        self.reorder_writes = true;
        self
    }
}

impl FileDevice {
    /// Set the fault injector (B3/D3 testing).
    pub fn set_faults(&self, faults: FaultInjector) {
        *self.faults.lock().unwrap() = Some(faults);
    }

    /// Clear the fault injector.
    pub fn clear_faults(&self) {
        *self.faults.lock().unwrap() = None;
    }

    /// T1: arm the operation recorder. Every subsequent `write_block`
    /// and `sync` is appended to the shared log (in order).
    pub fn arm_recorder(&self) {
        *self.recorder.lock().unwrap() =
            Some(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    }

    /// T1: get a handle to the live recording (for mid-workload sync
    /// counting). Returns None if the recorder is not armed.
    pub fn recorder_handle(&self) -> Option<std::sync::Arc<std::sync::Mutex<Vec<RecordedOp>>>> {
        self.recorder.lock().unwrap().clone()
    }

    /// T1: take the recorded operations, disarming the recorder.
    pub fn take_recording(&self) -> Vec<RecordedOp> {
        match self.recorder.lock().unwrap().take() {
            Some(arc) => std::sync::Arc::try_unwrap(arc)
                .map(|m| m.into_inner().unwrap())
                .unwrap_or_else(|arc| arc.lock().unwrap().clone()),
            None => Vec::new(),
        }
    }

    /// Take an exclusive advisory lock on the image file (flock).
    /// Serializes lease acquire/renew/release across processes.
    pub fn lock_exclusive(&self) -> io::Result<()> {
        use fs2::FileExt;
        self.file.lock_exclusive()
    }

    /// Release the advisory lock.
    pub fn unlock(&self) -> io::Result<()> {
        fs2::FileExt::unlock(&self.file)
    }

    /// Reads `buf.len() / BLOCK_SIZE` contiguous blocks starting at `start`
    /// in a single syscall. `buf.len()` must be a multiple of BLOCK_SIZE.
    pub fn read_blocks(&self, start: u64, buf: &mut [u8]) -> io::Result<()> {
        assert!(buf.len() % BLOCK_SIZE == 0);
        let count = buf.len() / BLOCK_SIZE;
        if start + count as u64 > self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block range out of range",
            ));
        }
        self.file.read_exact_at(buf, start * BLOCK_SIZE as u64)
    }
    /// Creates a new image file with `blocks` zeroed blocks.
    pub fn create(path: &Path, blocks: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(blocks * BLOCK_SIZE as u64)?;
        Ok(Self {
            file,
            blocks,
            faults: std::sync::Mutex::new(None),
            recorder: std::sync::Mutex::new(None),
        })
    }

    /// Opens an existing image; its size must be a multiple of the block size.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        if len % BLOCK_SIZE as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image size is not a multiple of the block size",
            ));
        }
        Ok(Self {
            blocks: len / BLOCK_SIZE as u64,
            file,
            faults: std::sync::Mutex::new(None),
            recorder: std::sync::Mutex::new(None),
        })
    }
}

impl BlockDevice for FileDevice {
    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()> {
        // T9: injected EIO on read.
        if self
            .faults
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|f| f.fail_reads)
        {
            return Err(io::Error::other("injected EIO on read"));
        }
        if n >= self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block number out of range",
            ));
        }
        self.file.read_exact_at(buf, n * BLOCK_SIZE as u64)
    }

    fn write_block(&self, n: u64, buf: &Block) -> io::Result<()> {
        if n >= self.blocks {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "block number out of range",
            ));
        }
        // T1: record the op (after the bounds check, before faults, so the
        // log reflects what the filesystem attempted at the device layer).
        if let Some(rec) = self.recorder.lock().unwrap().clone() {
            rec.lock().unwrap().push(RecordedOp::Write {
                block: n,
                data: *buf,
            });
        }
        // B3/D3: fault injection. Decide under the injector lock, then do
        // file I/O without holding it (P4: concurrent writers must not
        // serialize on fault state).
        enum FaultAction {
            Plain,
            Torn(usize),
            BitFlip(f64),
        }
        let action = {
            let mut faults = self.faults.lock().unwrap();
            match faults.as_mut() {
                None => FaultAction::Plain,
                Some(f) if f.reorder_writes => {
                    f.buffered.push((n, *buf));
                    return Ok(());
                }
                Some(f) => {
                    if let Some(torn_bytes) = f.torn_write_bytes {
                        FaultAction::Torn(torn_bytes)
                    } else if f.bit_flip_prob > 0.0 {
                        FaultAction::BitFlip(f.bit_flip_prob)
                    } else {
                        FaultAction::Plain
                    }
                }
            }
        };
        let at = n * BLOCK_SIZE as u64;
        match action {
            FaultAction::Plain => self.file.write_all_at(buf, at),
            FaultAction::Torn(torn_bytes) => {
                let mut torn = [0u8; BLOCK_SIZE];
                let nb = torn_bytes.min(BLOCK_SIZE);
                torn[..nb].copy_from_slice(&buf[..nb]);
                // The rest stays as it was (we don't know old content, so
                // just write the partial — the test will verify detection).
                self.file.write_all_at(&torn[..nb], at)
            }
            FaultAction::BitFlip(prob) => {
                let mut data = *buf;
                // Simple deterministic PRNG for reproducibility.
                let seed = n.wrapping_mul(0x9e3779b97f4a7c15);
                let r = ((seed >> 33) as f64) / (u64::MAX as f64);
                if r < prob {
                    let bit = (seed % (BLOCK_SIZE as u64 * 8)) as usize;
                    data[bit / 8] ^= 1 << (bit % 8);
                }
                self.file.write_all_at(&data, at)
            }
        }
    }

    fn sync(&self) -> io::Result<()> {
        // T9: injected EIO on sync / latency injection. Read the settings,
        // then act without holding the injector lock.
        let (fail_sync, latency_ms, reorder) = {
            let faults = self.faults.lock().unwrap();
            match faults.as_ref() {
                Some(f) => (f.fail_sync, f.sync_latency_ms, f.reorder_writes),
                None => (false, None, false),
            }
        };
        if fail_sync {
            return Err(io::Error::other("injected EIO on sync"));
        }
        if let Some(ms) = latency_ms {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        // B3/D3: flush buffered writes in reverse order (reordering test).
        if reorder {
            let buffered = {
                let mut faults = self.faults.lock().unwrap();
                faults
                    .as_mut()
                    .map(|f| std::mem::take(&mut f.buffered))
                    .unwrap_or_default()
            };
            // Reverse order: superblock (written last) hits disk first.
            for (n, buf) in buffered.into_iter().rev() {
                self.file.write_all_at(&buf, n * BLOCK_SIZE as u64)?;
            }
        }
        self.file.sync_all()?;
        // T1: record the sync barrier.
        if let Some(rec) = self.recorder.lock().unwrap().clone() {
            rec.lock().unwrap().push(RecordedOp::Sync);
        }
        Ok(())
    }
}

/// T1: Recording device for crash-state enumeration.
///
/// Logs every `write_block` and `sync` operation. A crash state is defined
/// as: the full prefix of operations up to some sync, plus any subset (in
/// any order) of the writes after that sync (bounded by `max_post_sync`).
///
/// To generate a crash state: call `crash_prefix(sync_idx)` to get the
/// operations up to the `sync_idx`-th sync, then apply a chosen subset of
/// subsequent writes to a fresh device.
#[derive(Debug, Clone)]
pub enum RecordedOp {
    Write { block: u64, data: Block },
    Sync,
}

pub struct RecordingDevice<D: BlockDevice> {
    inner: D,
    log: std::sync::Mutex<Vec<RecordedOp>>,
}

impl<D: BlockDevice> RecordingDevice<D> {
    pub fn new(inner: D) -> Self {
        RecordingDevice {
            inner,
            log: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The recorded operation log.
    /// P4: returns a snapshot copy; the log is behind a Mutex so
    /// `BlockDevice` can be implemented for `&self`.
    pub fn log(&self) -> Vec<RecordedOp> {
        self.log.lock().unwrap().clone()
    }

    /// Indices in the log where Sync operations occur.
    pub fn sync_indices(&self) -> Vec<usize> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, RecordedOp::Sync))
            .map(|(i, _)| i)
            .collect()
    }

    /// Get the prefix of operations up to (and including) the `sync_idx`-th sync.
    /// `sync_idx` is 0-based among syncs. Returns the ops to replay for a crash
    /// at that point.
    pub fn crash_prefix(&self, sync_idx: usize) -> Vec<RecordedOp> {
        let log = self.log.lock().unwrap();
        let syncs: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, RecordedOp::Sync))
            .map(|(i, _)| i)
            .collect();
        if sync_idx >= syncs.len() {
            return log.clone();
        }
        let end = syncs[sync_idx] + 1; // include the sync
        log[..end].to_vec()
    }

    /// Writes after the `sync_idx`-th sync (candidates for partial application).
    pub fn post_sync_writes(&self, sync_idx: usize) -> Vec<RecordedOp> {
        let log = self.log.lock().unwrap();
        let syncs: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, RecordedOp::Sync))
            .map(|(i, _)| i)
            .collect();
        let start = if sync_idx < syncs.len() {
            syncs[sync_idx] + 1
        } else {
            log.len()
        };
        log[start..]
            .iter()
            .filter(|op| matches!(op, RecordedOp::Write { .. }))
            .cloned()
            .collect()
    }
}

/// Replay a set of recorded operations onto a device.
/// Free function (not associated with RecordingDevice) to avoid type inference issues.
pub fn replay_ops<D2: BlockDevice>(ops: &[RecordedOp], dev: &D2) -> io::Result<()> {
    for op in ops {
        match op {
            RecordedOp::Write { block, data } => {
                dev.write_block(*block, data)?;
            }
            RecordedOp::Sync => {
                dev.sync()?;
            }
        }
    }
    Ok(())
}

impl<D: BlockDevice> BlockDevice for RecordingDevice<D> {
    fn block_count(&self) -> u64 {
        self.inner.block_count()
    }

    fn read_block(&self, n: u64, buf: &mut Block) -> io::Result<()> {
        self.inner.read_block(n, buf)
    }

    fn write_block(&self, n: u64, buf: &Block) -> io::Result<()> {
        // Log before writing (so a crash during write is represented by
        // the op being in the log but potentially torn — the harness
        // can simulate torn writes by truncating the data).
        self.log.lock().unwrap().push(RecordedOp::Write {
            block: n,
            data: *buf,
        });
        self.inner.write_block(n, buf)
    }

    fn sync(&self) -> io::Result<()> {
        self.log.lock().unwrap().push(RecordedOp::Sync);
        self.inner.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cownfs-rec-{}.img", name))
    }

    #[test]
    fn t1_recording_device_logs_and_replays() {
        let path = temp_path("t1");
        let _ = std::fs::remove_file(&path);

        // Create a device and wrap it.
        let inner = FileDevice::create(&path, 16).unwrap();
        let rec = RecordingDevice::new(inner);

        // Do some writes and syncs.
        let mut blk1 = [0u8; BLOCK_SIZE];
        blk1[0] = 0xAA;
        rec.write_block(5, &blk1).unwrap();
        rec.sync().unwrap();

        let mut blk2 = [0u8; BLOCK_SIZE];
        blk2[0] = 0xBB;
        rec.write_block(6, &blk2).unwrap();
        // No sync after blk2 (simulates crash before sync).

        // Verify the log.
        let log = rec.log();
        assert_eq!(log.len(), 3); // write, sync, write
        assert!(matches!(log[0], RecordedOp::Write { block: 5, .. }));
        assert!(matches!(log[1], RecordedOp::Sync));
        assert!(matches!(log[2], RecordedOp::Write { block: 6, .. }));

        // Crash prefix up to sync 0 should include the sync.
        let prefix = rec.crash_prefix(0);
        assert_eq!(prefix.len(), 2);

        // Post-sync writes should be just blk2.
        let post = rec.post_sync_writes(0);
        assert_eq!(post.len(), 1);

        // Replay the prefix to a new device and verify.
        let path2 = temp_path("t1-replay");
        let _ = std::fs::remove_file(&path2);
        let mut dev2 = FileDevice::create(&path2, 16).unwrap();
        replay_ops(&prefix, &mut dev2).unwrap();

        let mut read_back = [0u8; BLOCK_SIZE];
        dev2.read_block(5, &mut read_back).unwrap();
        assert_eq!(read_back[0], 0xAA);
        // Block 6 was not in the prefix (crash before it was durable).
        dev2.read_block(6, &mut read_back).unwrap();
        assert_eq!(read_back[0], 0x00); // never written

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }
}
