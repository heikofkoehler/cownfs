//! Free-space bitmap: one bit per block, persisted as ordinary blocks.
//!
//! The bitmap itself is CoW-updated per transaction (see the architecture
//! plan); this type is the in-memory representation plus serialization.
//!
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

/// Words per bitmap page (4KiB block / 8 bytes per word).
pub const WORDS_PER_PAGE: usize = 512;
/// Bits per bitmap page.
pub const BITS_PER_PAGE: u64 = WORDS_PER_PAGE as u64 * 64;
/// Maximum pages held in memory (64 * 4KiB = 256KiB).
pub const MAX_CACHED_PAGES: usize = 64;

/// A memory-bounded bitmap that pages 4KiB blocks from disk on demand.
///
/// Only `MAX_CACHED_PAGES` pages are held in memory (LRU eviction).
/// Per-page free counts are always resident (8KB per 1M pages) for
/// efficient allocation scanning without loading every page.
#[derive(Debug)]
pub struct PagedBitmap {
    nbits: u64,
    bitmap_blocks: u64,
    /// page_idx -> 512 words (4KiB). Only cached pages are here.
    cache: std::collections::HashMap<u64, [u64; WORDS_PER_PAGE]>,
    /// LRU order: front = oldest.
    lru: std::collections::VecDeque<u64>,
    /// Pages with unflushed modifications.
    dirty_pages: std::collections::HashSet<u64>,
    /// Free bit count per page. Always resident. Initialized at load.
    free_counts: Vec<u32>,
    /// Global word indices modified (for delta persists).
    dirty_words: std::collections::HashSet<u64>,
    /// Allocation cursor: page index to start next alloc scan.
    cursor: u64,
}

impl PagedBitmap {
    pub fn new(nbits: u64, bitmap_blocks: u64) -> Self {
        let npages = bitmap_blocks as usize;
        Self {
            nbits,
            bitmap_blocks,
            cache: std::collections::HashMap::new(),
            lru: std::collections::VecDeque::new(),
            dirty_pages: std::collections::HashSet::new(),
            free_counts: vec![0; npages],
            dirty_words: std::collections::HashSet::new(),
            cursor: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.nbits
    }

    pub fn page_count(&self) -> u64 {
        self.bitmap_blocks
    }

    /// Page index for a bit.
    pub fn page_idx(bit: u64) -> u64 {
        bit / BITS_PER_PAGE
    }

    /// Is the page currently cached?
    pub fn is_cached(&self, page_idx: u64) -> bool {
        self.cache.contains_key(&page_idx)
    }

    /// Number of cached pages.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Test a bit. Page must be cached (use `is_cached` first).
    pub fn test_cached(&self, i: u64) -> bool {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        let words = self.cache.get(&page).expect("page not cached");
        let wi = ((i % BITS_PER_PAGE) / 64) as usize;
        words[wi] & (1u64 << (i % 64)) != 0
    }

    /// Set a bit. Page must be cached.
    pub fn set_cached(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        let wi = ((i % BITS_PER_PAGE) / 64) as usize;
        let bit = i % 64;
        let words = self.cache.get_mut(&page).expect("page not cached");
        if words[wi] & (1u64 << bit) == 0 {
            words[wi] |= 1u64 << bit;
            self.dirty_pages.insert(page);
            self.dirty_words.insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            if self.free_counts[page as usize] > 0 {
                self.free_counts[page as usize] -= 1;
            }
            self.touch(page);
        }
    }

    /// Clear a bit. Page must be cached.
    pub fn clear_cached(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        let wi = ((i % BITS_PER_PAGE) / 64) as usize;
        let bit = i % 64;
        let words = self.cache.get_mut(&page).expect("page not cached");
        if words[wi] & (1u64 << bit) != 0 {
            words[wi] &= !(1u64 << bit);
            self.dirty_pages.insert(page);
            self.dirty_words.insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            self.free_counts[page as usize] += 1;
            self.touch(page);
        }
    }

    /// Move page to MRU position.
    fn touch(&mut self, page_idx: u64) {
        if let Some(pos) = self.lru.iter().position(|&p| p == page_idx) {
            self.lru.remove(pos);
        }
        self.lru.push_back(page_idx);
    }

    /// Insert a page into the cache (from disk). Caller must ensure
    /// capacity (evict first if at MAX_CACHED_PAGES).
    pub fn insert_page(&mut self, page_idx: u64, words: [u64; WORDS_PER_PAGE]) {
        // Compute free count.
        let mut free = 0u32;
        let bits_in_page = BITS_PER_PAGE.min(self.nbits.saturating_sub(page_idx * BITS_PER_PAGE));
        for (wi, w) in words.iter().enumerate() {
            let word_bits = 64u64.min(bits_in_page.saturating_sub(wi as u64 * 64));
            if word_bits == 64 {
                free += w.count_zeros();
            } else if word_bits > 0 {
                let mask = (1u64 << word_bits) - 1;
                free += (!w & mask).count_ones();
            }
        }
        self.free_counts[page_idx as usize] = free;
        self.cache.insert(page_idx, words);
        self.touch(page_idx);
    }

    /// Evict the LRU page. Returns (page_idx, words, was_dirty).
    pub fn evict_lru(&mut self) -> Option<(u64, [u64; WORDS_PER_PAGE], bool)> {
        let page_idx = self.lru.pop_front()?;
        let words = self.cache.remove(&page_idx)?;
        let was_dirty = self.dirty_pages.remove(&page_idx);
        Some((page_idx, words, was_dirty))
    }

    /// Find a page with free space, starting from cursor (wraps).
    /// Returns None if all pages are full.
    pub fn find_free_page(&mut self) -> Option<u64> {
        let npages = self.bitmap_blocks;
        if npages == 0 {
            return None;
        }
        let start = self.cursor % npages;
        for offset in 0..npages {
            let pi = (start + offset) % npages;
            if self.free_counts[pi as usize] > 0 {
                self.cursor = pi;
                return Some(pi);
            }
        }
        None
    }

    /// Allocate a free bit in a cached page. Returns the bit index, or None
    /// if the page is full. Page must be cached.
    pub fn alloc_in_page(&mut self, page_idx: u64) -> Option<u64> {
        let words = self.cache.get_mut(&page_idx).expect("page not cached");
        let base = page_idx * BITS_PER_PAGE;
        for (wi, w) in words.iter_mut().enumerate() {
            if *w != u64::MAX {
                let bit = w.trailing_ones();
                let idx = base + wi as u64 * 64 + bit as u64;
                if idx < self.nbits {
                    *w |= 1u64 << bit;
                    self.dirty_pages.insert(page_idx);
                    self.dirty_words.insert(page_idx * WORDS_PER_PAGE as u64 + wi as u64);
                    if self.free_counts[page_idx as usize] > 0 {
                        self.free_counts[page_idx as usize] -= 1;
                    }
                    self.touch(page_idx);
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Try to allocate a specific bit. Page must be cached.
    /// Returns true if the bit was free and is now allocated.
    pub fn alloc_at_cached(&mut self, block: u64) -> bool {
        if block >= self.nbits {
            return false;
        }
        let page = Self::page_idx(block);
        let wi = ((block % BITS_PER_PAGE) / 64) as usize;
        let bit = block % 64;
        let words = self.cache.get_mut(&page).expect("page not cached");
        if words[wi] & (1u64 << bit) == 0 {
            words[wi] |= 1u64 << bit;
            self.dirty_pages.insert(page);
            self.dirty_words.insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            if self.free_counts[page as usize] > 0 {
                self.free_counts[page as usize] -= 1;
            }
            self.touch(page);
            true
        } else {
            false
        }
    }

    /// Word indices changed (for delta persists).
    pub fn dirty_words(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.dirty_words.iter().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_words.clear();
        self.dirty_pages.clear();
    }

    /// Get a word value (for delta serialization). Page must be cached.
    pub fn word_cached(&self, wi: u64) -> u64 {
        let page = wi / WORDS_PER_PAGE as u64;
        let w_in_page = (wi % WORDS_PER_PAGE as u64) as usize;
        self.cache.get(&page).expect("page not cached")[w_in_page]
    }

    /// Set a word directly (for delta application). Page must be cached.
    /// Does not mark dirty (used when loading).
    pub fn set_word_cached(&mut self, wi: u64, val: u64) {
        let page = wi / WORDS_PER_PAGE as u64;
        let w_in_page = (wi % WORDS_PER_PAGE as u64) as usize;
        let words = self.cache.get_mut(&page).expect("page not cached");
        let old = words[w_in_page];
        words[w_in_page] = val;
        // Update free count.
        let free_old = old.count_zeros();
        let free_new = val.count_zeros();
        let fc = &mut self.free_counts[page as usize];
        *fc = (*fc as i64 + free_new as i64 - free_old as i64).max(0) as u32;
    }

    pub fn word_count(&self) -> u64 {
        self.bitmap_blocks * WORDS_PER_PAGE as u64
    }

    /// Get a cached page's words (for flushing).
    pub fn get_page(&self, page_idx: u64) -> Option<&[u64; WORDS_PER_PAGE]> {
        self.cache.get(&page_idx)
    }

    /// Dirty page indices.
    pub fn dirty_page_indices(&self) -> Vec<u64> {
        self.dirty_pages.iter().copied().collect()
    }

    /// Mark a page clean after flushing.
    pub fn mark_clean(&mut self, page_idx: u64) {
        self.dirty_pages.remove(&page_idx);
    }

    /// Set free count directly (used at load time).
    pub fn set_free_count(&mut self, page_idx: u64, count: u32) {
        self.free_counts[page_idx as usize] = count;
    }
}
