//! cownfs-core: on-disk primitives for the cownfs copy-on-write filesystem.
//!
//! P0 scope: block device abstraction, CRC32C checksums, the free-space
//! bitmap, and the ping-pong superblock (format / open / generation commit).

pub const BLOCK_SIZE: usize = 4096;

/// One filesystem block.
pub type Block = [u8; BLOCK_SIZE];

pub mod bitmap;
pub mod block;
pub mod btree;
pub mod checksum;
pub mod engine;
pub mod store;
pub mod superblock;
