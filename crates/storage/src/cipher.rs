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
//!
//! Two ciphers seal pages and records ([`Aead`]), both deterministic and
//! misuse resistant: AES-256-GCM-SIV (RFC 8452, `nucleus_kernels::aes`)
//! where the CPU has AES instructions — several times faster there — and
//! `nucleus_crypto::siv` (ChaCha20) where it has not.  A new file takes
//! the one this CPU runs fast; a file keeps its cipher, named in its
//! header, wherever it is opened (AES without the instructions by a
//! portable, constant-time copy: correct, but slow).  The key check
//! value and the header tags are ChaCha20-SIV's whichever cipher seals
//! the pages; AES seals under a key of its own derived from the database
//! key (HKDF-Expand, RFC 5869), so no key serves two algorithms.

use nucleus_crypto::siv;
use nucleus_kernels::aes;

use crate::page::{PAGE_HEADER_SIZE, PAGE_SIZE, Page, PageId, read_u64, write_u64};

/// A file in the clear.
pub const CIPHER_NONE: u32 = 0;
/// Pages sealed by `nucleus_crypto::siv` (ChaCha20, Poly1305's hash; the
/// structure of AES-GCM-SIV).
pub const CIPHER_CHACHA20_SIV: u32 = 1;
/// Pages sealed by AES-256-GCM-SIV (RFC 8452) under a key derived from
/// the database key.
pub const CIPHER_AES256_GCM_SIV: u32 = 2;

/// A deterministic, misuse-resistant AEAD under a 256-bit key: one of the
/// file ciphers ([`CIPHER_CHACHA20_SIV`], [`CIPHER_AES256_GCM_SIV`]).
/// Its keys are zeroed when it is dropped and never printed.
#[derive(Clone)]
pub struct Aead {
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    ChaCha([u8; 32]),
    Aes(Box<aes::Key>),
}

impl Drop for Kind {
    fn drop(&mut self) {
        if let Kind::ChaCha(key) = self {
            key.fill(0);
            std::hint::black_box(key);
        }
    }
}

impl std::fmt::Debug for Aead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Aead({})", self.code())
    }
}

impl Aead {
    /// Under `key`, the cipher this CPU runs fastest: AES-256-GCM-SIV
    /// where it has AES instructions, else ChaCha20-SIV.
    pub fn new(key: &[u8; 32]) -> Self {
        let code = if aes::hardware() {
            CIPHER_AES256_GCM_SIV
        } else {
            CIPHER_CHACHA20_SIV
        };
        Self::of(code, key).expect("a known cipher")
    }

    /// Cipher `code` under `key`; `None` for a code not known.
    pub fn of(code: u32, key: &[u8; 32]) -> Option<Self> {
        let kind = match code {
            CIPHER_CHACHA20_SIV => Kind::ChaCha(*key),
            CIPHER_AES256_GCM_SIV => {
                // HKDF-Expand with the key as the pseudorandom key (it is
                // one: 32 uniform bytes), one block.
                let mut sub =
                    nucleus_crypto::hmac_sha256(key, b"nucleus page cipher: AES-256-GCM-SIV\x01");
                let k = aes::Key::new(&sub);
                sub.fill(0);
                std::hint::black_box(&sub);
                Kind::Aes(Box::new(k))
            }
            _ => return None,
        };
        Some(Self { kind })
    }

    /// The cipher's code in a file header.
    pub fn code(&self) -> u32 {
        match self.kind {
            Kind::ChaCha(_) => CIPHER_CHACHA20_SIV,
            Kind::Aes(_) => CIPHER_AES256_GCM_SIV,
        }
    }

    /// Seals `data` in place; its tag.
    pub fn seal_in_place(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> [u8; 16] {
        match &self.kind {
            Kind::ChaCha(key) => siv::seal_in_place(key, nonce, aad, data),
            Kind::Aes(key) => aes::seal_in_place(key, nonce, aad, data),
        }
    }

    /// Opens in place what [`seal_in_place`](Self::seal_in_place) sealed:
    /// `false`, `data` zeroed, if it is not what was sealed.
    pub fn open_in_place(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8; 16],
    ) -> bool {
        match &self.kind {
            Kind::ChaCha(key) => siv::open_in_place(key, nonce, aad, data, tag),
            Kind::Aes(key) => aes::open_in_place(key, nonce, aad, data, tag),
        }
    }
}

/// The key a database file's pages are sealed under, and the cipher
/// ([`Aead`]) they are sealed with.  Its bytes are zeroed when it is
/// dropped and never printed.
#[derive(Clone)]
pub struct PageCipher {
    key: [u8; 32],
    aead: Aead,
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
    /// The cipher of a 256-bit key: for a new file, the one this CPU runs
    /// fastest ([`Aead::new`]).
    pub fn new(key: [u8; 32]) -> Self {
        let aead = Aead::new(&key);
        Self { key, aead }
    }

    /// The same key with cipher `code` — a file's, from its header.
    pub fn for_code(&self, code: u32) -> Result<Self, String> {
        let aead = Aead::of(code, &self.key)
            .ok_or_else(|| format!("cipher {code} is not known (a file of a newer nucleus?)"))?;
        Ok(Self {
            key: self.key,
            aead,
        })
    }

    /// The cipher's code in a file header ([`CIPHER_CHACHA20_SIV`] or
    /// [`CIPHER_AES256_GCM_SIV`]).
    pub fn code(&self) -> u32 {
        self.aead.code()
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
        let tag = self.aead.seal_in_place(
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
        if !self.aead.open_in_place(
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
        self.aead.seal_in_place(&n, &a, payload)
    }

    /// Opens in place a record [`seal_record`](Self::seal_record) sealed:
    /// `false` (the payload zeroed) if it is not what was sealed at `lsn`.
    pub(crate) fn open_record(&self, lsn: u64, payload: &mut [u8], tag: &[u8; 16]) -> bool {
        let (n, a) = record_nonce_aad(lsn, payload.len() as u32);
        self.aead.open_in_place(&n, &a, payload, tag)
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

    /// Both ciphers seal and open; neither opens the other's pages; the
    /// key check is the same whichever seals; a code not known is
    /// refused.
    #[test]
    fn each_cipher_opens_its_own_pages_only() {
        let base = cipher(1);
        let chacha = base.for_code(CIPHER_CHACHA20_SIV).unwrap();
        let aes = base.for_code(CIPHER_AES256_GCM_SIV).unwrap();
        assert_eq!((chacha.code(), aes.code()), (1, 2));
        let expected = if nucleus_kernels::aes::hardware() {
            2
        } else {
            1
        };
        assert_eq!(base.code(), expected, "a new file takes the faster");
        let page = sample();
        let mut a = vec![0u8; PAGE_SIZE];
        let mut c = vec![0u8; PAGE_SIZE];
        aes.seal(PageId(9), &page, &mut a).unwrap();
        chacha.seal(PageId(9), &page, &mut c).unwrap();
        assert_ne!(a, c);
        assert_eq!(aes.open(PageId(9), &a).unwrap().payload(), page.payload());
        assert_eq!(
            chacha.open(PageId(9), &c).unwrap().payload(),
            page.payload()
        );
        assert!(chacha.open(PageId(9), &a).is_err());
        assert!(aes.open(PageId(9), &c).is_err());
        assert_eq!(aes.check_value(), chacha.check_value());
        let mut r = *b"a log record";
        let tag = aes.seal_record(77, &mut r);
        assert!(!chacha.open_record(77, &mut r.clone(), &tag));
        assert!(aes.open_record(77, &mut r, &tag));
        assert_eq!(&r, b"a log record");
        assert!(base.for_code(3).is_err());
        assert!(base.for_code(CIPHER_NONE).is_err());
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
