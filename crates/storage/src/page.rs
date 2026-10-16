//! The on-disk page format.
//!
//! A database file is an array of [`PAGE_SIZE`]-byte pages; page `n` lives
//! at offset `n * PAGE_SIZE`.  Every page — the file header included —
//! starts with the same 24-byte page header (all integers little-endian):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 4 | CRC-32C of bytes `4..PAGE_SIZE` |
//! | 4  | 1 | page type ([`PageType`]) |
//! | 5  | 3 | reserved, zero |
//! | 8  | 8 | LSN — the last WAL record that changed the page (unused until §M4) |
//! | 16 | 8 | the page's own id |
//!
//! The stored id catches a *misdirected* write — correct bytes at the wrong
//! offset — which a checksum alone would accept.

use crate::crc32c::crc32c;

/// Size of every page in bytes.  Recorded in the file header; a file with a
/// different page size is refused rather than misread.
pub const PAGE_SIZE: usize = 8192;

/// Size of the header every page starts with.
pub const PAGE_HEADER_SIZE: usize = 24;

/// Bytes available to a page's owner after the page header.
pub const PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

/// The number of a page in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u64);

impl PageId {
    /// The byte offset of this page in the file.
    pub fn offset(self) -> u64 {
        self.0 * PAGE_SIZE as u64
    }
}

/// What a page holds.  Types below [`PageType::FIRST_USER`] belong to the
/// pager; the layers above choose their own from there on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageType(pub u8);

impl PageType {
    /// Never written (all zeros).
    pub const UNUSED: Self = Self(0);
    /// One of the two file header copies.
    pub const HEADER: Self = Self(1);
    /// A page of the free list (holds free page numbers).
    pub const FREELIST: Self = Self(2);
    /// The first type available to layers above the pager.
    pub const FIRST_USER: Self = Self(16);
}

/// One page in memory.
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    bytes: Box<[u8]>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("type", &self.page_type())
            .field("lsn", &self.lsn())
            .finish_non_exhaustive()
    }
}

impl Page {
    /// A zeroed page of type `ty`.
    pub fn new(ty: PageType) -> Self {
        let mut page = Self {
            bytes: vec![0u8; PAGE_SIZE].into_boxed_slice(),
        };
        page.set_page_type(ty);
        page
    }

    /// The page type.
    pub fn page_type(&self) -> PageType {
        PageType(self.bytes[4])
    }

    /// Sets the page type.
    pub fn set_page_type(&mut self, ty: PageType) {
        self.bytes[4] = ty.0;
    }

    /// The LSN field.
    pub fn lsn(&self) -> u64 {
        read_u64(&self.bytes, 8)
    }

    /// Sets the LSN field.
    pub fn set_lsn(&mut self, lsn: u64) {
        write_u64(&mut self.bytes, 8, lsn);
    }

    /// The bytes after the page header.
    pub fn payload(&self) -> &[u8] {
        &self.bytes[PAGE_HEADER_SIZE..]
    }

    /// The bytes after the page header, writable.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[PAGE_HEADER_SIZE..]
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Stamps the page with its id and checksum, ready to be written.
    pub(crate) fn seal(&mut self, id: PageId) {
        self.bytes[5..8].fill(0);
        write_u64(&mut self.bytes, 16, id.0);
        let crc = crc32c(&self.bytes[4..]);
        self.bytes[0..4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Checks a page read from position `id`.
    pub(crate) fn verify(&self, id: PageId) -> Result<(), &'static str> {
        let stored = u32::from_le_bytes(self.bytes[0..4].try_into().unwrap());
        if stored != crc32c(&self.bytes[4..]) {
            return Err(if self.bytes.iter().all(|&b| b == 0) {
                "never written (all zeros)"
            } else {
                "checksum mismatch"
            });
        }
        if read_u64(&self.bytes, 16) != id.0 {
            return Err("page id mismatch (misdirected write)");
        }
        Ok(())
    }
}

/// Reads a little-endian `u64` at `at` (for page payloads).
pub fn read_u64_at(b: &[u8], at: usize) -> u64 {
    read_u64(b, at)
}

/// Writes a little-endian `u64` at `at` (for page payloads).
pub fn write_u64_at(b: &mut [u8], at: usize, v: u64) {
    write_u64(b, at, v)
}

pub(crate) fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub(crate) fn write_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

pub(crate) fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub(crate) fn write_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_then_verify() {
        let mut p = Page::new(PageType::FIRST_USER);
        p.payload_mut()[..5].copy_from_slice(b"hello");
        p.set_lsn(77);
        p.seal(PageId(9));
        assert_eq!(p.verify(PageId(9)), Ok(()));
        assert_eq!(p.lsn(), 77);
        assert!(p.verify(PageId(8)).is_err());
        p.payload_mut()[100] ^= 1;
        assert_eq!(p.verify(PageId(9)), Err("checksum mismatch"));
    }

    #[test]
    fn zeros_are_reported_as_never_written() {
        let p = Page {
            bytes: vec![0u8; PAGE_SIZE].into_boxed_slice(),
        };
        assert_eq!(p.verify(PageId(3)), Err("never written (all zeros)"));
    }
}
