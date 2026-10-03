//! CRC32C (Castagnoli) checksums for bit-rot detection.

use crc::{Crc, CRC_32_ISCSI};

const CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// Checksum of `data`, widened to u64 for on-disk storage.
pub fn checksum(data: &[u8]) -> u64 {
    CRC32C.checksum(data) as u64
}

/// Checksum of `data` as u32 (for parent-stored data block checksums).
pub fn checksum32(data: &[u8]) -> u32 {
    CRC32C.checksum(data)
}
