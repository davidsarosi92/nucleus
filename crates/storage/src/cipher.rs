//! Pages sealed on disk (PLAN.md §M14.30 K2b).
//!
//! An encrypted database file seals every data page — free-list pages
//! included — with `nucleus_crypto::siv`, the deterministic, nonce-misuse
//! resistant AEAD: a page is written again under the same id and birth
//! before a commit, and after a crash a lost commit's pages are written
//! again under the same birth, which a nonce-based AEAD must never see.
//!
//! On disk an encrypted page is
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 16 | the SIV tag |
//! | 16 | 8  | `birth << 8 \| page type`, little-endian, in the clear |
//! | 24 | 8168 | the payload, encrypted |
//!
//! The page's id is not stored: it is the nonce's first 8 bytes and part
//! of the associated data, so a page read anywhere but where it was
//! written does not open (the misdirected write a plain page's stored id
//! catches).  The birth and type are authenticated with it.  In memory
//! the page keeps its usual header (type, birth in the LSN field, id), so
//! nothing above the pager knows.
//!
//! A WAL record (K2c) is sealed the same way: its length, the tag in
//! place of the checksum, its LSN — a 28-byte header instead of 16 —
//! and the payload encrypted, the LSN the nonce and, with the length,
//! the associated data ([`crate::wal`]).
//!
//! The two header pages stay readable without the key — they say the file
//! is encrypted, with which cipher, and which key (a key check value) —
//! and are authenticated under it: a tag over the header's bytes, so
//! tree roots and the free list's head cannot be changed unseen.

use nucleus_crypto::siv;

use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageId, read_u64, write_u64};

/// A file in the clear.
pub const CIPHER_NONE: u32 = 0;
/// Pages sealed by `nucleus_crypto::siv` (ChaCha20, Poly1305's hash; the
/// structure of AES-GCM-SIV).
pub const CIPHER_CHACHA20_SIV: u32 = 1;

/// The key a database file's pages are sealed under.  Its bytes are
/// zeroed when it is dropped and never printed.
#[derive(Clone)]
pub struct PageCipher {
    key: [u8; 32],
}

impl std::fmt::Debug for PageCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageCipher").finish_non_exhaustive()
    }
}

impl Drop for PageCipher {
    fn drop(&mut self) {
        self.key.fill(0);
        std::hint::black_box(&self.key);
    }
}

/// The nonce of page `id`, or of a header slot or the key check
/// (`label` keeps them apart).
fn nonce(id: u64, label: &[u8; 4]) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&id.to_le_bytes());
    n[8..].copy_from_slice(label);
    n
}

impl PageCipher {
    /// The cipher of a 256-bit key.
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// The cipher's code in a file header ([`CIPHER_CHACHA20_SIV`]).
    pub fn code(&self) -> u32 {
        CIPHER_CHACHA20_SIV
    }

    /// A value that tells this key from another without giving anything
    /// of it away (the tag of a fixed, empty message): recorded in the
    /// file header, so the wrong key is refused by name rather than as
    /// damage.
    pub fn check_value(&self) -> [u8; 16] {
        siv::seal_in_place(
            &self.key,
            &nonce(0, b"kchk"),
            b"nucleus page key check",
            &mut [],
        )
    }

    /// The tag of header slot `slot`'s bytes `bytes`.
    pub(crate) fn header_tag(&self, slot: PageId, bytes: &[u8]) -> [u8; 16] {
        siv::seal_in_place(&self.key, &nonce(slot.0, b"head"), bytes, &mut [])
    }

    /// `page`, to be written at `id`, sealed into `out` (a whole page).
    pub(crate) fn seal(&self, id: PageId, page: &Page, out: &mut [u8]) -> Result<(), &'static str> {
        let b = page.bytes();
        let birth = read_u64(b, 8);
        if birth >> 56 != 0 {
            return Err("a birth past 2^56 does not fit an encrypted page");
        }
        let packed = birth << 8 | u64::from(b[4]);
        write_u64(out, 16, packed);
        out[PAGE_HEADER_SIZE..PAGE_SIZE].copy_from_slice(&b[PAGE_HEADER_SIZE..]);
        let tag = siv::seal_in_place(
            &self.key,
            &nonce(id.0, b"page"),
            &aad(id, packed),
            &mut out[PAGE_HEADER_SIZE..PAGE_SIZE],
        );
        out[..16].copy_from_slice(&tag);
        Ok(())
    }

    /// The page sealed at `id` as `bytes`, opened.
    pub(crate) fn open(&self, id: PageId, bytes: &[u8]) -> Result<Page, &'static str> {
        if bytes.iter().all(|&b| b == 0) {
            return Err("never written (all zeros)");
        }
        let mut page = Page::from_bytes(bytes);
        let b = page.bytes_mut();
        let tag: [u8; 16] = b[..16].try_into().expect("16 bytes");
        let packed = read_u64(b, 16);
        if !siv::open_in_place(
            &self.key,
            &nonce(id.0, b"page"),
            &aad(id, packed),
            &mut b[PAGE_HEADER_SIZE..],
            &tag,
        ) {
            return Err("does not open: damaged, moved, or sealed under another key");
        }
        b[..8].fill(0);
        b[4] = packed as u8;
        write_u64(b, 8, packed >> 8);
        write_u64(b, 16, id.0);
        Ok(page)
    }
}

/// A WAL record's nonce and associated data: its LSN, and its LSN and
/// payload length (PLAN.md §M14.30 K2c).
fn record_nonce_aad(lsn: u64, len: u32) -> ([u8; 12], [u8; 12]) {
    let mut a = [0u8; 12];
    a[..8].copy_from_slice(&lsn.to_le_bytes());
    a[8..].copy_from_slice(&len.to_le_bytes());
    (nonce(lsn, b"wal\0"), a)
}

impl PageCipher {
    /// Seals a WAL record's payload at `lsn` in place; its tag.  A record
    /// torn and written again at the same LSN is sealed again under the
    /// same nonce, which the SIV allows.
    pub(crate) fn seal_record(&self, lsn: u64, payload: &mut [u8]) -> [u8; 16] {
        let (n, a) = record_nonce_aad(lsn, payload.len() as u32);
        siv::seal_in_place(&self.key, &n, &a, payload)
    }

    /// Opens in place a record [`seal_record`](Self::seal_record) sealed:
    /// `false` (the payload zeroed) if it is not what was sealed at `lsn`.
    pub(crate) fn open_record(&self, lsn: u64, payload: &mut [u8], tag: &[u8; 16]) -> bool {
        let (n, a) = record_nonce_aad(lsn, payload.len() as u32);
        siv::open_in_place(&self.key, &n, &a, payload, tag)
    }
}

/// What a page's seal authenticates beside its payload: its id, its
/// birth and its type.
fn aad(id: PageId, packed: u64) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&id.0.to_le_bytes());
    a[8..].copy_from_slice(&packed.to_le_bytes());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageType;

    fn cipher(k: u8) -> PageCipher {
        PageCipher::new([k; 32])
    }

    fn sample() -> Page {
        let mut p = Page::new(PageType::FIRST_USER);
        for (i, b) in p.payload_mut().iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        p.set_lsn(41);
        p
    }

    #[test]
    fn a_page_sealed_opens_as_it_was() {
        let c = cipher(1);
        let page = sample();
        let mut out = vec![0u8; PAGE_SIZE];
        c.seal(PageId(9), &page, &mut out).unwrap();
        let same = out[PAGE_HEADER_SIZE..]
            .iter()
            .zip(page.payload())
            .filter(|(a, b)| a == b)
            .count();
        assert!(same < 100, "the payload shows through ({same} bytes equal)");
        let back = c.open(PageId(9), &out).unwrap();
        assert_eq!(back.page_type(), PageType::FIRST_USER);
        assert_eq!(back.lsn(), 41);
        assert_eq!(back.payload(), page.payload());
        assert_eq!(read_u64(back.bytes(), 16), 9);
    }

    #[test]
    fn moved_changed_or_under_another_key_it_does_not_open() {
        let c = cipher(1);
        let mut out = vec![0u8; PAGE_SIZE];
        c.seal(PageId(9), &sample(), &mut out).unwrap();
        assert!(c.open(PageId(10), &out).is_err(), "moved");
        assert!(cipher(2).open(PageId(9), &out).is_err(), "another key");
        for at in [0, 15, 16, 20, 23, 24, 4000, PAGE_SIZE - 1] {
            let mut bad = out.clone();
            bad[at] ^= 0x10;
            assert!(c.open(PageId(9), &bad).is_err(), "byte {at} changed");
        }
        assert_eq!(
            c.open(PageId(9), &vec![0u8; PAGE_SIZE]).unwrap_err(),
            "never written (all zeros)"
        );
    }

    #[test]
    fn a_birth_too_large_is_refused_and_key_checks_differ() {
        let mut page = sample();
        page.set_lsn(1 << 56);
        let mut out = vec![0u8; PAGE_SIZE];
        assert!(cipher(1).seal(PageId(9), &page, &mut out).is_err());
        assert_ne!(cipher(1).check_value(), cipher(2).check_value());
        assert_eq!(cipher(1).check_value(), cipher(1).check_value());
    }
}
