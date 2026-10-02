//! pNFS file layout state (RFC 5661 §12).
//!
//! v1 scope: single data server, used as a staging area. LAYOUTGET
//! allocates a run of DS block IDs for a file range; the client writes
//! blocks to the DS directly; LAYOUTCOMMIT verifies checksums from the
//! DS and swings the CoW B-tree pointers. No multi-DS striping yet.

/// An outstanding layout: a file range mapped to DS block IDs.
#[derive(Debug, Clone)]
pub struct Layout {
    pub session_id: [u8; 16],
    pub file_ino: u64,
    pub offset: u64,
    pub length: u64,
    pub first_block_id: u64,
    pub nblocks: u64,
}

pub struct LayoutTable {
    next_block_id: u64,
    layouts: Vec<Layout>,
}

impl LayoutTable {
    pub fn new() -> Self {
        LayoutTable {
            // DS block IDs start at 1<<32 to avoid colliding with any
            // small IDs a test might use directly.
            next_block_id: 1 << 32,
            layouts: Vec::new(),
        }
    }

    /// Allocate DS block IDs for [offset, offset+length) on file_ino.
    pub fn layout_get(
        &mut self,
        session_id: [u8; 16],
        file_ino: u64,
        offset: u64,
        length: u64,
    ) -> Layout {
        let nblocks = length.div_ceil(4096);
        let first = self.next_block_id;
        self.next_block_id += nblocks;
        let l = Layout {
            session_id,
            file_ino,
            offset,
            length,
            first_block_id: first,
            nblocks,
        };
        self.layouts.push(l.clone());
        l
    }

    /// Find the layout covering a commit range. Does not remove it;
    /// LAYOUTRETURN releases.
    pub fn find(
        &self,
        session_id: &[u8; 16],
        file_ino: u64,
        offset: u64,
        length: u64,
    ) -> Option<&Layout> {
        self.layouts.iter().find(|l| {
            l.session_id == *session_id
                && l.file_ino == file_ino
                && l.offset <= offset
                && offset + length <= l.offset + l.length
        })
    }

    /// Release layouts overlapping [offset, offset+length).
    /// Returns the number released.
    pub fn layout_return(
        &mut self,
        session_id: &[u8; 16],
        file_ino: u64,
        offset: u64,
        length: u64,
    ) -> usize {
        let before = self.layouts.len();
        self.layouts.retain(|l| {
            !(l.session_id == *session_id
                && l.file_ino == file_ino
                && l.offset < offset + length
                && offset < l.offset + l.length)
        });
        before - self.layouts.len()
    }

    pub fn outstanding(&self) -> usize {
        self.layouts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_find_return() {
        let mut t = LayoutTable::new();
        let sid = [7u8; 16];
        let l = t.layout_get(sid, 100, 0, 8192);
        assert_eq!(l.nblocks, 2);
        assert!(t.find(&sid, 100, 0, 8192).is_some());
        assert!(t.find(&sid, 100, 0, 8193).is_none());
        assert_eq!(t.layout_return(&sid, 100, 0, 8192), 1);
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn ids_are_unique_across_layouts() {
        let mut t = LayoutTable::new();
        let sid = [7u8; 16];
        let a = t.layout_get(sid, 100, 0, 4096);
        let b = t.layout_get(sid, 101, 0, 4096);
        assert_ne!(a.first_block_id, b.first_block_id);
    }
}
