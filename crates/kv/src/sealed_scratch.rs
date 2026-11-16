//! Scratch space of an encrypted database (PLAN.md §M14.30 K2d): a sort's
//! runs, a hash join's partitions — rows of the database, so sealed like
//! its pages, under a key of the scratch file's own, made at random when
//! the file is and never written anywhere: the file is unreadable once
//! its handle is gone (after a crash too; opening removes such files).
//!
//! The bytes are kept in 4 KiB blocks, each sealed by the cipher this CPU
//! runs fastest (`nucleus_storage::cipher::Aead`: AES-256-GCM-SIV where it
//! has AES instructions, ChaCha20-SIV where not; the file never outlives
//! the process) — its tag, then its bytes encrypted, the block's number
//! as nonce and associated data — so a block moved or changed does not
//! open.  The last block, while it is not full, stays in memory in the
//! clear and goes to the file sealed once it is: appending — what sorts
//! and partitions do — seals every block once and never reads one back.
//! A write over blocks in the file opens them, changes them and seals
//! them again (under the same nonce, which the SIV allows); a read opens
//! the blocks it covers, read in one read of the file, into the caller's
//! buffer.

use nucleus_storage::cipher::Aead;

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
    aead: Aead,
    /// Bytes written (the highest end).
    len: u64,
    /// The last block's bytes while it is not full (`len % BLOCK` of
    /// them): blocks before it are full and in the file.
    tail: Vec<u8>,
}

fn nonce(i: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&i.to_le_bytes());
    n[8..].copy_from_slice(b"scr\0");
    n
}

fn aad(i: u64) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&i.to_le_bytes());
    a[8..].copy_from_slice(&BLOCK.to_le_bytes());
    a
}

fn damaged() -> Error {
    Error::Storage("a scratch block does not open (damaged or changed)".into())
}

impl SealedScratch {
    /// Scratch space sealed under `key` into `raw` (empty).
    pub(crate) fn new(raw: Box<dyn Scratch>, key: [u8; 32]) -> Self {
        Self {
            raw,
            aead: Aead::new(&key),
            len: 0,
            tail: Vec::with_capacity(BLOCK as usize),
        }
    }

    /// Bytes in the file's blocks (all full).
    fn sealed_len(&self) -> u64 {
        self.len - self.tail.len() as u64
    }

    /// Block `i`'s bytes, `parts` together (`BLOCK` of them), sealed onto
    /// the end of `out`: its tag, then them encrypted.
    fn seal_onto(&self, out: &mut Vec<u8>, i: u64, parts: &[&[u8]]) {
        let at = out.len();
        out.extend_from_slice(&[0u8; TAG as usize]);
        for p in parts {
            out.extend_from_slice(p);
        }
        let tag = self
            .aead
            .seal_in_place(&nonce(i), &aad(i), &mut out[at + TAG as usize..]);
        out[at..at + TAG as usize].copy_from_slice(&tag);
    }

    /// Blocks `first..=last` of the file, read and opened in place: slot
    /// `k` (block `first + k`) its tag, then its bytes.
    fn open_blocks(&self, first: u64, last: u64) -> Result<Vec<u8>> {
        let mut slots = vec![0u8; ((last - first + 1) * SLOT) as usize];
        self.raw.read_at(first * SLOT, &mut slots)?;
        for (k, slot) in slots
            .as_chunks_mut::<{ SLOT as usize }>()
            .0
            .iter_mut()
            .enumerate()
        {
            let i = first + k as u64;
            let (tag, bytes) = slot.split_at_mut(TAG as usize);
            let tag: [u8; 16] = (&*tag).try_into().expect("16 bytes");
            if !self.aead.open_in_place(&nonce(i), &aad(i), bytes, &tag) {
                return Err(damaged());
            }
        }
        Ok(slots)
    }

    /// `data` after the end.
    fn append(&mut self, mut data: &[u8]) -> Result<()> {
        let room = BLOCK as usize - self.tail.len();
        if data.len() < room {
            self.tail.extend_from_slice(data);
            self.len += data.len() as u64;
            return Ok(());
        }
        let first = self.sealed_len() / BLOCK;
        let blocks = 1 + (data.len() - room) / BLOCK as usize;
        let mut out = Vec::with_capacity(blocks * SLOT as usize);
        self.seal_onto(&mut out, first, &[&self.tail, &data[..room]]);
        self.len += data.len() as u64;
        data = &data[room..];
        let (whole, rest) = data.split_at(data.len() / BLOCK as usize * BLOCK as usize);
        for (k, chunk) in whole.as_chunks::<{ BLOCK as usize }>().0.iter().enumerate() {
            self.seal_onto(&mut out, first + 1 + k as u64, &[&chunk[..]]);
        }
        self.raw.write_at(first * SLOT, &out)?;
        self.tail.clear();
        self.tail.extend_from_slice(rest);
        Ok(())
    }

    /// `data` over bytes there are, from `offset`.
    fn overwrite(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset + data.len() as u64;
        let sealed = self.sealed_len();
        if offset < sealed {
            let to = end.min(sealed);
            let (first, last) = (offset / BLOCK, (to - 1) / BLOCK);
            let mut slots = self.open_blocks(first, last)?;
            let mut pos = offset;
            while pos < to {
                let (i, within) = (pos / BLOCK, pos % BLOCK);
                let n = (BLOCK - within).min(to - pos) as usize;
                let at = ((i - first) * SLOT + TAG + within) as usize;
                let from = (pos - offset) as usize;
                slots[at..at + n].copy_from_slice(&data[from..from + n]);
                pos += n as u64;
            }
            for (k, slot) in slots
                .as_chunks_mut::<{ SLOT as usize }>()
                .0
                .iter_mut()
                .enumerate()
            {
                let i = first + k as u64;
                let (tag, bytes) = slot.split_at_mut(TAG as usize);
                tag.copy_from_slice(&self.aead.seal_in_place(&nonce(i), &aad(i), bytes));
            }
            self.raw.write_at(first * SLOT, &slots)?;
        }
        if end > sealed {
            let from = offset.max(sealed);
            self.tail[(from - sealed) as usize..(end - sealed) as usize]
                .copy_from_slice(&data[(from - offset) as usize..]);
        }
        Ok(())
    }
}

impl Scratch for SealedScratch {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        // A gap: zeros, as scratch space in memory reads there.
        while self.len < offset {
            let n = (offset - self.len).min(1 << 20);
            self.append(&vec![0u8; n as usize])?;
        }
        let over = (self.len.min(offset + data.len() as u64) - offset) as usize;
        if over > 0 {
            self.overwrite(offset, &data[..over])?;
        }
        self.append(&data[over..])
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset + buf.len() as u64;
        if end > self.len {
            return Err(Error::Storage("read past the end of scratch space".into()));
        }
        let sealed = self.sealed_len();
        if offset < sealed {
            let to = end.min(sealed);
            let first = offset / BLOCK;
            let slots = self.open_blocks(first, (to - 1) / BLOCK)?;
            let mut pos = offset;
            while pos < to {
                let (i, within) = (pos / BLOCK, pos % BLOCK);
                let n = (BLOCK - within).min(to - pos) as usize;
                let at = ((i - first) * SLOT + TAG + within) as usize;
                let into = (pos - offset) as usize;
                buf[into..into + n].copy_from_slice(&slots[at..at + n]);
                pos += n as u64;
            }
        }
        if end > sealed {
            let from = offset.max(sealed);
            buf[(from - offset) as usize..]
                .copy_from_slice(&self.tail[(from - sealed) as usize..(end - sealed) as usize]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryScratch;

    /// Writes of every shape — appends, overwrites inside and across
    /// blocks and into the last one, a gap — read back as scratch space
    /// in memory does, and nothing written shows in the file.
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
            let offset = match step % 5 {
                0 | 1 => len,                           // append
                2 => next() % (len + 1),                // overwrite somewhere
                3 => len.saturating_sub(next() % 6000), // over the end
                _ => len + next() % 9000,               // past the end: a gap
            };
            let n = (next() % 20_000) as usize;
            let data: Vec<u8> = (0..n).map(|k| (k as u64 * 13 + step) as u8 | 1).collect();
            sealed.write_at(offset, &data).unwrap();
            model.write_at(offset, &data).unwrap();
            assert_eq!(sealed.len, model.0.len() as u64);
            assert_eq!(sealed.tail.len() as u64, sealed.len % BLOCK);
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

    /// The file holds whole blocks only, none of them readable; a byte of
    /// it changed, the block holding it does not open.
    #[test]
    fn a_changed_block_does_not_open() {
        let data: Vec<u8> = (0..10_000u32).map(|k| (k * 7) as u8).collect();
        let mut sealed = SealedScratch::new(Box::new(MemoryScratch::default()), [4; 32]);
        sealed.write_at(0, &data).unwrap();
        let mut file = vec![0u8; 2 * SLOT as usize];
        sealed.raw.read_at(0, &mut file).unwrap();
        assert!(
            sealed.raw.read_at(2 * SLOT, &mut [0u8; 1]).is_err(),
            "the tail is not in the file"
        );
        assert!(
            !file
                .windows(64)
                .any(|w| data.windows(64).take(200).any(|d| d == w)),
            "plain bytes in the file"
        );
        file[SLOT as usize + 100] ^= 1;
        let mut raw = MemoryScratch::default();
        raw.write_at(0, &file).unwrap();
        let tampered = SealedScratch {
            raw: Box::new(raw),
            aead: Aead::new(&[4; 32]),
            len: 10_000,
            tail: data[2 * BLOCK as usize..].to_vec(),
        };
        let mut buf = vec![0u8; 100];
        assert!(tampered.read_at(0, &mut buf).is_ok(), "block 0 is intact");
        assert!(
            tampered.read_at(9_000, &mut buf).is_ok(),
            "the tail is in memory"
        );
        assert!(tampered.read_at(BLOCK + 10, &mut buf).is_err());
    }
}

#[cfg(test)]
mod speed {
    use super::*;
    use crate::MemoryScratch;

    /// What sealing costs scratch space: 32 MiB written as a sort's run
    /// is (unaligned 64 KiB pieces) and read back so, sealed and in
    /// memory.  `cargo test --release -p nucleus-kv sealed_scratch_speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement: run it in release"]
    fn sealed_scratch_speed() {
        let piece: Vec<u8> = (0..65_000u32).map(|i| (i * 7) as u8).collect();
        for sealed in [false, true] {
            let mut s: Box<dyn Scratch> = if sealed {
                Box::new(SealedScratch::new(
                    Box::new(MemoryScratch::default()),
                    [1; 32],
                ))
            } else {
                Box::new(MemoryScratch::default())
            };
            let t = std::time::Instant::now();
            let mut at = 0u64;
            while at < 32 << 20 {
                s.write_at(at, &piece).unwrap();
                at += piece.len() as u64;
            }
            let w = t.elapsed();
            let t = std::time::Instant::now();
            let mut buf = vec![0u8; 65_000];
            let mut r = 0u64;
            while r + 65_000 <= at {
                s.read_at(r, &mut buf).unwrap();
                r += 60_000;
            }
            let how = if sealed { "sealed" } else { "memory" };
            metric(&format!("{how}: write"), &[w.as_secs_f64() * 1e3], "ms");
            metric(
                &format!("{how}: read"),
                &[t.elapsed().as_secs_f64() * 1e3],
                "ms",
            );
            println!(
                "{:6} write {:6.1} ms   read {:6.1} ms",
                how,
                w.as_secs_f64() * 1e3,
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    /// Numbers on stdout as `bench/method.py run` gathers them (PLAN.md
    /// §M14.31: the test run again, each number with its spread).
    fn metric(name: &str, values: &[f64], unit: &str) {
        let values: Vec<String> = values.iter().map(f64::to_string).collect();
        println!(
            "@metric {{\"name\": {name:?}, \"values\": [{}], \"unit\": {unit:?}, \
             \"system\": \"nucleus\", \"phase\": \"warm\"}}",
            values.join(", ")
        );
    }
}
