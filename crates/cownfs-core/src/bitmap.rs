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
    /// Word indices modified since the last full bitmap write (or since
    /// load). Used for delta-bitmap persists: only dirty words are written
    /// per txg instead of the full bitmap.
    dirty: std::collections::HashSet<u64>,
    /// Allocation cursor: word index where the last alloc succeeded.
    /// Next alloc starts here (wraps around) instead of scanning from 0.
    cursor: u64,
}

impl Bitmap {
    pub fn new(nbits: u64) -> Self {
        Self {
            words: vec![0; nbits.div_ceil(64) as usize],
            nbits,
            dirty: std::collections::HashSet::new(),
            cursor: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.nbits
    }

    pub fn set(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let wi = i / 64;
        self.words[wi as usize] |= 1u64 << (i % 64);
        self.dirty.insert(wi);
    }

    pub fn clear(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let wi = i / 64;
        self.words[wi as usize] &= !(1u64 << (i % 64));
        self.dirty.insert(wi);
    }

    pub fn test(&self, i: u64) -> bool {
        debug_assert!(i < self.nbits);
        self.words[(i / 64) as usize] & (1u64 << (i % 64)) != 0
    }

    /// First-fit allocation starting from the cursor (wraps around).
    /// Returns a free block number and marks it used.
    pub fn alloc(&mut self) -> Option<u64> {
        let nwords = self.words.len() as u64;
        if nwords == 0 {
            return None;
        }
        let start = (self.cursor % nwords) as usize;
        for offset in 0..nwords {
            let wi = (start + offset as usize) % nwords as usize;
            let w = &mut self.words[wi];
            if *w != u64::MAX {
                let bit = w.trailing_ones();
                let idx = wi as u64 * 64 + bit as u64;
                if idx < self.nbits {
                    *w |= 1u64 << bit;
                    self.dirty.insert(wi as u64);
                    self.cursor = wi as u64;
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Try to allocate a specific block (for contiguous runs). Returns true
    /// if the block was free and is now allocated.
    pub fn alloc_at(&mut self, block: u64) -> bool {
        if block >= self.nbits {
            return false;
        }
        let wi = (block / 64) as usize;
        let bit = block % 64;
        if self.words[wi] & (1u64 << bit) == 0 {
            self.words[wi] |= 1u64 << bit;
            self.dirty.insert(wi as u64);
            self.cursor = wi as u64;
            true
        } else {
            false
        }
    }

    /// Word indices changed since the last [`Self::clear_dirty`].
    pub fn dirty_words(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.dirty.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// Clear the dirty set (after a full bitmap write or checkpoint).
    pub fn clear_dirty(&mut self) {
        self.dirty.clear();
    }

    /// Get a word value (for delta serialization).
    pub fn word(&self, wi: u64) -> u64 {
        self.words[wi as usize]
    }

    /// Set a word value directly (for delta application on open).
    /// Does not mark dirty.
    pub fn set_word(&mut self, wi: u64, val: u64) {
        self.words[wi as usize] = val;
    }

    /// Number of words.
    pub fn word_count(&self) -> u64 {
        self.words.len() as u64
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
        b.dirty.clear();
        b
    }
}
