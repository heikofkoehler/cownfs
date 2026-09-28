//! Free-space bitmap: one bit per block, persisted as ordinary blocks.
//!
//! The bitmap itself is CoW-updated per transaction (see the architecture
//! plan); this type is the in-memory representation plus serialization.

use crate::BLOCK_SIZE;

/// Number of bitmap blocks needed to track `nbits` blocks.
pub fn blocks_needed(nbits: u64) -> u64 {
    nbits.div_ceil(BLOCK_SIZE as u64 * 8)
}

#[derive(Debug, Clone)]
pub struct Bitmap {
    words: Vec<u64>,
    nbits: u64,
}

impl Bitmap {
    pub fn new(nbits: u64) -> Self {
        Self {
            words: vec![0; nbits.div_ceil(64) as usize],
            nbits,
        }
    }

    pub fn len(&self) -> u64 {
        self.nbits
    }

    pub fn set(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        self.words[(i / 64) as usize] |= 1u64 << (i % 64);
    }

    pub fn clear(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        self.words[(i / 64) as usize] &= !(1u64 << (i % 64));
    }

    pub fn test(&self, i: u64) -> bool {
        debug_assert!(i < self.nbits);
        self.words[(i / 64) as usize] & (1u64 << (i % 64)) != 0
    }

    /// First-fit allocation: returns a free block number and marks it used.
    pub fn alloc(&mut self) -> Option<u64> {
        for (wi, w) in self.words.iter_mut().enumerate() {
            if *w != u64::MAX {
                let bit = w.trailing_ones();
                let idx = wi as u64 * 64 + bit as u64;
                if idx < self.nbits {
                    *w |= 1u64 << bit;
                    return Some(idx);
                }
            }
        }
        None
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.words.len() * 8);
        for w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(nbits: u64, bytes: &[u8]) -> Self {
        let mut b = Self::new(nbits);
        for (i, chunk) in bytes.chunks(8).enumerate() {
            if i >= b.words.len() {
                break;
            }
            let mut arr = [0u8; 8];
            arr[..chunk.len()].copy_from_slice(chunk);
            b.words[i] = u64::from_le_bytes(arr);
        }
        // Mask off padding bits past nbits in the final word.
        let excess = b.words.len() as u64 * 64 - nbits;
        if excess > 0 {
            if let Some(last) = b.words.last_mut() {
                *last &= u64::MAX >> excess;
            }
        }
        b
    }
}
