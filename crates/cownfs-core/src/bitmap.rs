//! Free-space bitmap: one bit per block, persisted as ordinary blocks.
//!
//! The bitmap itself is CoW-updated per transaction (see the architecture
//! plan); this type is the in-memory representation plus serialization.
//!
use std::io;
use std::sync::Arc;

use crate::block::{BlockDevice, FileDevice};
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
    /// P3: O(1) free-space counter. Number of clear bits (free blocks).
    /// Maintained incrementally on set/clear/alloc; recomputed on load.
    free_count: u64,
}

impl Bitmap {
    /// S4: construct from raw words (used by PagedBitmap::to_bitmap).
    pub(crate) fn from_words(words: Vec<u64>, nbits: u64, free_count: u64) -> Self {
        let mut dirty = std::collections::HashSet::new();
        for i in 0..words.len() as u64 {
            dirty.insert(i);
        }
        Self {
            words,
            nbits,
            dirty,
            cursor: 0,
            free_count,
        }
    }
}

impl Bitmap {
    pub fn new(nbits: u64) -> Self {
        Self {
            words: vec![0; nbits.div_ceil(64) as usize],
            nbits,
            dirty: std::collections::HashSet::new(),
            cursor: 0,
            free_count: nbits, // all free initially
        }
    }

    pub fn len(&self) -> u64 {
        self.nbits
    }

    pub fn set(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let wi = i / 64;
        let bit = 1u64 << (i % 64);
        if self.words[wi as usize] & bit == 0 {
            // Was free, now allocated.
            self.free_count -= 1;
        }
        self.words[wi as usize] |= bit;
        self.dirty.insert(wi);
    }

    pub fn clear(&mut self, i: u64) {
        debug_assert!(i < self.nbits);
        let wi = i / 64;
        let bit = 1u64 << (i % 64);
        if self.words[wi as usize] & bit != 0 {
            // Was allocated, now free.
            self.free_count += 1;
        }
        self.words[wi as usize] &= !bit;
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
                    self.free_count -= 1; // P3: maintain counter
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
            self.free_count -= 1; // P3: maintain counter
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
        let old = self.words[wi as usize];
        self.words[wi as usize] = val;
        // P3: update free count for the changed word.
        // Only count bits within nbits (last word may have padding).
        let idx = wi as usize;
        let is_last = idx == self.words.len() - 1;
        let valid_bits = if is_last {
            let excess = self.words.len() as u64 * 64 - self.nbits;
            64 - excess
        } else {
            64
        };
        let mask = if valid_bits == 64 {
            u64::MAX
        } else {
            (1u64 << valid_bits) - 1
        };
        let old_free = (!old & mask).count_ones() as u64;
        let new_free = (!val & mask).count_ones() as u64;
        self.free_count = self.free_count + new_free - old_free;
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
        // P3: recompute free count from the loaded bits.
        let allocated: u64 = b.words.iter().map(|w| w.count_ones() as u64).sum();
        b.free_count = nbits.saturating_sub(allocated);
        b
    }

    /// P3: O(1) free block count.
    pub fn free_count(&self) -> u64 {
        self.free_count
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
/// Per-page free counts are lazily computed (`u32::MAX` = unknown).
pub struct PagedBitmap {
    nbits: u64,
    bitmap_blocks: u64,
    /// S4: device for paging I/O (None = standalone, no paging).
    dev: Option<Arc<FileDevice>>,
    /// S4: first block of the active bitmap area (for paging I/O).
    area_start: u64,
    /// page_idx -> 512 words (4KiB). Only cached pages are here.
    cache: std::collections::HashMap<u64, [u64; WORDS_PER_PAGE]>,
    /// LRU order: front = oldest.
    lru: std::collections::VecDeque<u64>,
    /// Pages with unflushed modifications.
    dirty_pages: std::collections::HashSet<u64>,
    /// Free bit count per page. `u32::MAX` = unknown (lazy).
    free_counts: Vec<u32>,
    /// Global word indices modified (for delta persists).
    dirty_words: std::collections::HashSet<u64>,
    /// Allocation cursor: page index to start next alloc scan.
    cursor: u64,
    /// S4: P3 free-block counter, maintained incrementally.
    total_free: u64,
}

impl PagedBitmap {
    pub fn new(nbits: u64, bitmap_blocks: u64) -> Self {
        let npages = bitmap_blocks as usize;
        Self {
            nbits,
            bitmap_blocks,
            dev: None,
            area_start: 0,
            cache: std::collections::HashMap::new(),
            lru: std::collections::VecDeque::new(),
            dirty_pages: std::collections::HashSet::new(),
            // S4: lazy (u32::MAX = unknown); computed on first page load.
            free_counts: vec![u32::MAX; npages],
            dirty_words: std::collections::HashSet::new(),
            cursor: 0,
            total_free: nbits, // all free initially; caller adjusts
        }
    }

    /// Attach a device for transparent paging. After this, `test`/`set`/
    /// `clear`/`alloc` page data in on demand.
    pub fn with_device(mut self, dev: Arc<FileDevice>, area_start: u64) -> Self {
        self.dev = Some(dev);
        self.area_start = area_start;
        self
    }

    /// Update the active area (on slot flip).
    /// The cache is cleared: cached pages belong to the old area and must
    /// not be written to the new area on eviction.
    /// Caller must have flushed dirty pages (commit does via write_full_to).
    pub fn set_area_start(&mut self, area_start: u64) {
        if self.area_start != area_start {
            self.area_start = area_start;
            // Dirty pages were just written to the new area by write_full_to.
            // Clear the cache to avoid stale reads/writes.
            self.cache.clear();
            self.lru.clear();
            self.dirty_pages.clear();
            self.dirty_words.clear();
        }
    }

    /// S4: P3 free-block counter.
    pub fn free_count(&self) -> u64 {
        self.total_free
    }

    /// Set the total free count (at load time, from the superblock).
    pub fn set_total_free(&mut self, n: u64) {
        self.total_free = n;
    }

    /// Recompute the total free count by reading all bitmap pages from
    /// disk and popcounting. Used on open for pre-S4 images whose
    /// superblock has no persisted free_blocks (N8). O(bitmap), but only
    /// runs once on upgrade.
    pub fn recompute_free_count(&mut self) -> io::Result<u64> {
        let (dev, area_start) = match (self.dev.clone(), self.area_start) {
            (Some(d), a) => (d, a),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "paged bitmap without device",
                ))
            }
        };
        let mut total_free = 0u64;
        // bitmap_blocks is the number of 4KiB blocks in the bitmap area.
        // Each block holds 32768 bits.
        for page_idx in 0..self.bitmap_blocks {
            let words = Self::read_page(&dev, area_start, page_idx)?;
            for w in words {
                // Count zero bits (free blocks). Each word has 64 bits.
                total_free += w.count_zeros() as u64;
            }
        }
        // The bitmap may have padding bits beyond nbits; subtract them.
        let total_bits = self.bitmap_blocks * 32768;
        if total_bits > self.nbits {
            total_free -= total_bits - self.nbits;
        }
        self.total_free = total_free;
        Ok(total_free)
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
            self.dirty_words
                .insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            if self.free_counts[page as usize] > 0 && self.free_counts[page as usize] != u32::MAX {
                self.free_counts[page as usize] -= 1;
            }
            if self.total_free > 0 {
                self.total_free -= 1;
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
            self.dirty_words
                .insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            if self.free_counts[page as usize] != u32::MAX {
                self.free_counts[page as usize] += 1;
            }
            self.total_free += 1;
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
                    self.dirty_words
                        .insert(page_idx * WORDS_PER_PAGE as u64 + wi as u64);
                    if self.free_counts[page_idx as usize] > 0
                        && self.free_counts[page_idx as usize] != u32::MAX
                    {
                        self.free_counts[page_idx as usize] -= 1;
                    }
                    if self.total_free > 0 {
                        self.total_free -= 1;
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
            self.dirty_words
                .insert(page * WORDS_PER_PAGE as u64 + wi as u64);
            if self.free_counts[page as usize] > 0 && self.free_counts[page as usize] != u32::MAX {
                self.free_counts[page as usize] -= 1;
            }
            if self.total_free > 0 {
                self.total_free -= 1;
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

    /// Ensure a page is cached (loading from disk if needed) and return
    /// its words. Used by the commit path to stream pages without
    /// materializing the full bitmap (N11).
    pub fn get_page_or_load(&mut self, page_idx: u64) -> io::Result<&[u64; WORDS_PER_PAGE]> {
        self.ensure_cached(page_idx)?;
        Ok(self.cache.get(&page_idx).expect("just cached"))
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

    // ---- S4: transparent paging (device-backed) ----

    /// Ensure `page_idx` is cached, loading from disk if necessary.
    /// Evicts the LRU *clean* page when at capacity. Dirty pages are pinned
    /// in memory until commit (N5: writing a dirty page to `area_start`
    /// would corrupt the live bitmap area, violating R1).
    fn ensure_cached(&mut self, page_idx: u64) -> io::Result<()> {
        if self.cache.contains_key(&page_idx) {
            return Ok(());
        }
        let (dev, area_start) = match (self.dev.clone(), self.area_start) {
            (Some(d), a) => (d, a),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "paged bitmap without device",
                ))
            }
        };
        // Evict clean pages if at capacity. Dirty pages stay pinned;
        // they are written to the new area during commit, never to the
        // live area (N5).
        while self.cache.len() >= MAX_CACHED_PAGES {
            if let Some((evict_idx, words)) = self.evict_lru_clean() {
                // Clean page: just drop it (no writeback needed).
                let _ = (evict_idx, words);
            } else {
                // All cached pages are dirty; allow the cache to grow.
                // They will be written to the new area at commit time.
                break;
            }
        }
        // Load the page.
        let words = Self::read_page(&dev, area_start, page_idx)?;
        self.insert_page(page_idx, words);
        Ok(())
    }

    /// Evict the LRU clean (non-dirty) page. Returns None if all cached
    /// pages are dirty.
    fn evict_lru_clean(&mut self) -> Option<(u64, [u64; WORDS_PER_PAGE])> {
        // Find the LRU clean page by scanning from the front.
        let mut clean_idx = None;
        for &page_idx in &self.lru {
            if !self.dirty_pages.contains(&page_idx) {
                clean_idx = Some(page_idx);
                break;
            }
        }
        let page_idx = clean_idx?;
        // Remove from LRU list.
        self.lru.retain(|&x| x != page_idx);
        let words = self.cache.remove(&page_idx)?;
        Some((page_idx, words))
    }

    fn read_page(
        dev: &FileDevice,
        area_start: u64,
        page_idx: u64,
    ) -> io::Result<[u64; WORDS_PER_PAGE]> {
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(area_start + page_idx, &mut blk)?;
        let mut words = [0u64; WORDS_PER_PAGE];
        for (i, w) in words.iter_mut().enumerate() {
            *w = u64::from_le_bytes(blk[i * 8..i * 8 + 8].try_into().unwrap());
        }
        Ok(words)
    }

    fn write_page(
        dev: &FileDevice,
        area_start: u64,
        page_idx: u64,
        words: &[u64; WORDS_PER_PAGE],
    ) -> io::Result<()> {
        let mut blk = [0u8; BLOCK_SIZE];
        for (i, w) in words.iter().enumerate() {
            blk[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        dev.write_block(area_start + page_idx, &blk)?;
        Ok(())
    }

    /// Test a bit, paging in on demand.
    pub fn test(&mut self, i: u64) -> io::Result<bool> {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        self.ensure_cached(page)?;
        Ok(self.test_cached(i))
    }

    /// Set a bit, paging in on demand.
    pub fn set(&mut self, i: u64) -> io::Result<()> {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        self.ensure_cached(page)?;
        self.set_cached(i);
        Ok(())
    }

    /// Clear a bit, paging in on demand.
    pub fn clear(&mut self, i: u64) -> io::Result<()> {
        debug_assert!(i < self.nbits);
        let page = Self::page_idx(i);
        self.ensure_cached(page)?;
        self.clear_cached(i);
        Ok(())
    }

    /// Allocate a free bit, paging in on demand.
    pub fn alloc(&mut self) -> io::Result<Option<u64>> {
        // Find a page with free space (loading unknown pages as needed).
        let npages = self.bitmap_blocks;
        if npages == 0 {
            return Ok(None);
        }
        let start = self.cursor % npages;
        for offset in 0..npages {
            let page = (start + offset) % npages;
            if self.free_counts[page as usize] == u32::MAX {
                // Unknown: load to learn the true count.
                self.ensure_cached(page)?;
            }
            if self.free_counts[page as usize] > 0 {
                // Ensure cached (may have been evicted).
                self.ensure_cached(page)?;
                self.cursor = page + 1;
                return Ok(self.alloc_in_page(page));
            }
        }
        Ok(None)
    }

    /// Allocate a specific bit if free, paging in on demand.
    pub fn alloc_at(&mut self, block: u64) -> io::Result<bool> {
        if block >= self.nbits {
            return Ok(false);
        }
        let page = Self::page_idx(block);
        self.ensure_cached(page)?;
        Ok(self.alloc_at_cached(block))
    }

    /// Write all dirty pages to the device area. Does not clear the dirty
    /// set (caller decides).
    pub fn flush_dirty(&mut self) -> io::Result<()> {
        let dev = self
            .dev
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "paged bitmap without device"))?
            .clone();
        let area_start = self.area_start;
        let dirty: Vec<u64> = self.dirty_pages.iter().copied().collect();
        for page in dirty {
            if let Some(words) = self.cache.get(&page) {
                Self::write_page(&dev, area_start, page, words)?;
            }
        }
        Ok(())
    }

    /// Mark all pages clean (after a successful flush).
    pub fn mark_all_clean(&mut self) {
        self.dirty_pages.clear();
        self.dirty_words.clear();
    }

    /// S4: materialize into a fully-resident `Bitmap` (for commit).
    /// Used on the commit path where correctness matters more than memory;
    /// the open path (RSS-sensitive) uses paging.
    pub fn to_bitmap(&mut self) -> io::Result<Bitmap> {
        let dev = self
            .dev
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "paged bitmap without device"))?
            .clone();
        let source_start = self.area_start;
        let mut words = vec![0u64; ((self.nbits + 63) / 64) as usize];
        for page in 0..self.bitmap_blocks {
            if let Some(cached) = self.cache.get(&page) {
                let base = page as usize * WORDS_PER_PAGE;
                for (i, w) in cached.iter().enumerate() {
                    if base + i < words.len() {
                        words[base + i] = *w;
                    }
                }
            } else {
                let pw = Self::read_page(&dev, source_start, page)?;
                let base = page as usize * WORDS_PER_PAGE;
                for (i, w) in pw.iter().enumerate() {
                    if base + i < words.len() {
                        words[base + i] = *w;
                    }
                }
            }
        }
        Ok(Bitmap::from_words(words, self.nbits, self.total_free))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P3 exit criteria: counter equals popcount after random op storm.
    #[test]
    fn p3_free_count_matches_popcount() {
        let mut bm = Bitmap::new(1000);
        // Simple deterministic PRNG (xorshift).
        let mut state: u64 = 0x12345678;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..10000 {
            let op = next() % 4;
            let bit = next() % 1000;
            match op {
                0 => bm.set(bit),
                1 => bm.clear(bit),
                2 => {
                    let _ = bm.alloc();
                }
                3 => {
                    let _ = bm.alloc_at(bit);
                }
                _ => unreachable!(),
            }
            // Verify counter matches actual popcount.
            let mut actual_free = 0u64;
            for i in 0..1000 {
                if !bm.test(i) {
                    actual_free += 1;
                }
            }
            assert_eq!(
                bm.free_count(),
                actual_free,
                "free_count mismatch after op {op} on bit {bit}"
            );
        }

        // Also test from_bytes recomputes correctly.
        let bytes = bm.to_bytes();
        let bm2 = Bitmap::from_bytes(1000, &bytes);
        assert_eq!(bm2.free_count(), bm.free_count());
    }

    #[test]
    fn p3_set_word_maintains_counter() {
        let mut bm = Bitmap::new(128);
        // Set a word to all ones (0 free in that word).
        bm.set_word(0, u64::MAX);
        assert_eq!(bm.free_count(), 64); // 128 - 64 = 64 free
                                         // Set to all zeros.
        bm.set_word(0, 0);
        assert_eq!(bm.free_count(), 128);
        // Set to half.
        bm.set_word(0, 0x0000_FFFF_0000_FFFF);
        // 32 bits set in the word, so 128 - 32 = 96 free.
        assert_eq!(bm.free_count(), 96);
    }

    #[test]
    fn p3_idempotence() {
        // Setting an already-set bit / clearing an already-clear bit
        // must not change the counter.
        let mut bm = Bitmap::new(100);
        assert_eq!(bm.free_count(), 100);
        bm.set(5);
        assert_eq!(bm.free_count(), 99);
        bm.set(5); // idempotent
        assert_eq!(bm.free_count(), 99);
        bm.clear(5);
        assert_eq!(bm.free_count(), 100);
        bm.clear(5); // idempotent
        assert_eq!(bm.free_count(), 100);
        // alloc_at on already-allocated bit should fail and not change count.
        bm.set(10);
        assert_eq!(bm.free_count(), 99);
        assert!(!bm.alloc_at(10));
        assert_eq!(bm.free_count(), 99);
    }

    #[test]
    fn p3_padding_bits() {
        // Bits beyond block_count are padding; they must not affect
        // free_count, and set_word must not count them.
        let mut bm = Bitmap::new(100); // 100 bits, 28 padding bits in word 1
        assert_eq!(bm.free_count(), 100);
        // Set word 1 (bits 64..128) to all ones. Only bits 64..100 are
        // real; padding bits 100..128 must not reduce free_count below 0.
        bm.set_word(1, u64::MAX);
        // Bits 0..64 free (64), bits 64..100 used (36), padding ignored.
        assert_eq!(bm.free_count(), 64);
        // Clear word 1.
        bm.set_word(1, 0);
        assert_eq!(bm.free_count(), 100);
    }
}
