//! Scratch space of an encrypted database (PLAN.md §M14.30 K2d): a sort's
//! runs, a hash join's partitions — rows of the database, so sealed like
//! its pages, under a key of the scratch file's own, made at random when
//! the file is and never written anywhere: the file is unreadable once
//! its handle is gone (after a crash too; opening removes such files).
//!
//! The bytes are kept in 4 KiB blocks, each sealed by
//! `nucleus_crypto::siv` — its tag, then its bytes encrypted, the block's
//! number as nonce and, with its length, associated data — so a block
//! moved, cut short, or changed does not open.  A block written again
//! (the last one, growing) is sealed again under the same nonce, which
//! the SIV allows.  A write or read of any range touches the blocks it
//! covers in one read or write of the file; a write that covers part of
//! a block reads it first.

use nucleus_crypto::siv;

use crate::{Error, Result, Scratch};

/// Bytes in a block.
const BLOCK: u64 = 4096;
/// A block's tag.
const TAG: u64 = 16;
/// A block in the file: its tag and its bytes.
const SLOT: u64 = BLOCK + TAG;

/// Scratch space whose blocks are sealed into another (a file).
pub(crate) struct SealedScratch {
    raw: Box<dyn Scratch>,
    key: [u8; 32],
    /// Bytes written (the highest end).
    len: u64,
}

impl Drop for SealedScratch {
    fn drop(&mut self) {
        self.key.fill(0);
        std::hint::black_box(&self.key);
    }
}

fn nonce(i: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&i.to_le_bytes());
    n[8..].copy_from_slice(b"scr\0");
    n
}

fn aad(i: u64, len: u64) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&i.to_le_bytes());
    a[8..].copy_from_slice(&len.to_le_bytes());
    a
}

fn damaged() -> Error {
    Error::Storage("a scratch block does not open (damaged or changed)".into())
}

impl SealedScratch {
    /// Scratch space sealed under `key` into `raw` (empty).
    pub(crate) fn new(raw: Box<dyn Scratch>, key: [u8; 32]) -> Self {
        Self { raw, key, len: 0 }
    }

    /// The bytes block `i` holds now.
    fn held(&self, i: u64) -> u64 {
        self.len.saturating_sub(i * BLOCK).min(BLOCK)
    }

    /// The bytes of blocks `first..=last` (all held), opened.
    fn read_blocks(&self, first: u64, last: u64) -> Result<Vec<u8>> {
        let phys = (last - first) * SLOT + TAG + self.held(last);
        let mut buf = vec![0u8; phys as usize];
        self.raw.read_at(first * SLOT, &mut buf)?;
        let mut out = Vec::with_capacity(((last - first + 1) * BLOCK) as usize);
        for i in first..=last {
            let at = ((i - first) * SLOT) as usize;
            let n = self.held(i) as usize;
            let (tag, rest) = buf[at..at + TAG as usize + n].split_at_mut(TAG as usize);
            let tag: [u8; 16] = (&*tag).try_into().expect("16 bytes");
            if !siv::open_in_place(&self.key, &nonce(i), &aad(i, n as u64), rest, &tag) {
                return Err(damaged());
            }
            out.extend_from_slice(rest);
        }
        Ok(out)
    }

    /// Bytes `from..to` of block `i` as held now.
    fn held_bytes(&self, i: u64, from: u64, to: u64) -> Result<Vec<u8>> {
        let block = self.read_blocks(i, i)?;
        Ok(block[from as usize..to as usize].to_vec())
    }
}

impl Scratch for SealedScratch {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        // A gap: zeros, as scratch space in memory reads there.
        while self.len < offset {
            let n = (offset - self.len).min(1 << 20);
            self.write_at(self.len, &vec![0u8; n as usize])?;
        }
        if data.is_empty() {
            return Ok(());
        }
        let end = offset + data.len() as u64;
        let first = offset / BLOCK;
        let last = (end - 1) / BLOCK;
        let start = first * BLOCK;
        // What the blocks will hold: what they held before `offset` in the
        // first, `data`, what they held after `end` in the last.
        let tail_end = self.len.min((last + 1) * BLOCK).max(end);
        let mut plain = Vec::with_capacity((tail_end - start) as usize);
        if offset > start {
            plain.extend(self.held_bytes(first, 0, offset - start)?);
        }
        plain.extend_from_slice(data);
        if tail_end > end {
            let at = last * BLOCK;
            plain.extend(self.held_bytes(last, end - at, tail_end - at)?);
        }
        let mut out = Vec::with_capacity(plain.len() + ((last - first + 1) * TAG) as usize);
        for (k, chunk) in plain.chunks(BLOCK as usize).enumerate() {
            let i = first + k as u64;
            let at = out.len();
            out.extend_from_slice(&[0u8; TAG as usize]);
            out.extend_from_slice(chunk);
            let tag = siv::seal_in_place(
                &self.key,
                &nonce(i),
                &aad(i, chunk.len() as u64),
                &mut out[at + TAG as usize..],
            );
            out[at..at + TAG as usize].copy_from_slice(&tag);
        }
        self.raw.write_at(first * SLOT, &out)?;
        self.len = self.len.max(end);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset + buf.len() as u64;
        if end > self.len {
            return Err(Error::Storage("read past the end of scratch space".into()));
        }
        let first = offset / BLOCK;
        let plain = self.read_blocks(first, (end - 1) / BLOCK)?;
        let from = (offset - first * BLOCK) as usize;
        buf.copy_from_slice(&plain[from..from + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryScratch;

    /// Writes of every shape — appends, overwrites inside and across
    /// blocks, a gap — read back as scratch space in memory does, and
    /// nothing written shows in the file.
    #[test]
    fn reads_back_what_memory_would() {
        let mut sealed = SealedScratch::new(Box::new(MemoryScratch::default()), [4; 32]);
        let mut model = MemoryScratch::default();
        let mut seed = 7u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for step in 0..300 {
            let len = sealed.len;
            let offset = match step % 4 {
                0 | 1 => len,             // append
                2 => next() % (len + 1),  // overwrite somewhere
                _ => len + next() % 9000, // past the end: a gap
            };
            let n = (next() % 20_000) as usize;
            let data: Vec<u8> = (0..n).map(|k| (k as u64 * 13 + step) as u8 | 1).collect();
            sealed.write_at(offset, &data).unwrap();
            model.write_at(offset, &data).unwrap();
            assert_eq!(sealed.len, model.0.len() as u64);
            for _ in 0..4 {
                let from = next() % (sealed.len + 1);
                let to = from + next() % (sealed.len - from + 1);
                let mut a = vec![0u8; (to - from) as usize];
                let mut b = a.clone();
                sealed.read_at(from, &mut a).unwrap();
                model.read_at(from, &mut b).unwrap();
                assert_eq!(a, b, "step {step}: {from}..{to}");
            }
        }
        let mut past = [0u8; 1];
        assert!(sealed.read_at(sealed.len, &mut past).is_err());
    }

    /// A byte of the file changed: the block holding it does not open.
    #[test]
    fn a_changed_block_does_not_open() {
        let data: Vec<u8> = (0..10_000u32).map(|k| (k * 7) as u8).collect();
        let mut sealed = SealedScratch::new(Box::new(MemoryScratch::default()), [4; 32]);
        sealed.write_at(0, &data).unwrap();
        let mut raw = MemoryScratch::default();
        let mut file = vec![0u8; (2 * SLOT + TAG + 10_000 - 2 * BLOCK) as usize];
        sealed.raw.read_at(0, &mut file).unwrap();
        assert!(
            !file
                .windows(64)
                .any(|w| data.windows(64).take(200).any(|d| d == w)),
            "plain bytes in the file"
        );
        file[SLOT as usize + 100] ^= 1;
        raw.write_at(0, &file).unwrap();
        let tampered = SealedScratch {
            raw: Box::new(raw),
            key: [4; 32],
            len: 10_000,
        };
        let mut buf = vec![0u8; 100];
        assert!(tampered.read_at(0, &mut buf).is_ok(), "block 0 is intact");
        assert!(tampered.read_at(BLOCK + 10, &mut buf).is_err());
    }
}
