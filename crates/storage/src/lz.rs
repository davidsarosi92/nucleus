//! Value compression (PLAN.md §M14.20 V5): LZ77 in LZ4's block format,
//! written here so the storage layer keeps no dependencies.
//!
//! A block is a series of sequences: a token byte — literal count in its
//! high four bits, match length minus 4 in its low four, 15 meaning more
//! length bytes follow (each added, until one below 255) — then the
//! literals, then the match's offset back into the output (`u16`, little
//! endian, at least 1) and the match's further length bytes.  The last
//! sequence has literals only, and the last five bytes are always
//! literals.  Compression is greedy over a hash of the next four bytes:
//! fast, and good on the repeats rows hold (names, prefixes, zeros).
//! Decompression checks every length and offset against the block and the
//! expected size, so a damaged block is an error, never a bad read.

/// The shortest match encoded.
const MIN_MATCH: usize = 4;
/// Bytes at the end that are always literals (as LZ4).
const LAST_LITERALS: usize = 5;
const HASH_BITS: u32 = 12;

fn read32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn hash(x: u32) -> usize {
    (x.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// `input` compressed (appended to `out`).
pub fn compress(input: &[u8], out: &mut Vec<u8>) {
    let n = input.len();
    let mut table = vec![0u32; 1 << HASH_BITS]; // position + 1 (0: none)
    let mut anchor = 0;
    let mut i = 0;
    let limit = n.saturating_sub(LAST_LITERALS + MIN_MATCH);
    while i < limit {
        let h = hash(read32(input, i));
        let cand = table[h] as usize;
        table[h] = i as u32 + 1;
        if cand == 0
            || i - (cand - 1) > u16::MAX as usize
            || read32(input, cand - 1) != read32(input, i)
        {
            i += 1;
            continue;
        }
        let m = cand - 1;
        let mut len = MIN_MATCH;
        let max = n - LAST_LITERALS - i;
        while len < max && input[m + len] == input[i + len] {
            len += 1;
        }
        // The sequence: literals since the anchor, then the match.
        let lits = i - anchor;
        let ml = len - MIN_MATCH;
        out.push(((lits.min(15) as u8) << 4) | ml.min(15) as u8);
        if lits >= 15 {
            put_len(out, lits - 15);
        }
        out.extend_from_slice(&input[anchor..i]);
        out.extend_from_slice(&((i - m) as u16).to_le_bytes());
        if ml >= 15 {
            put_len(out, ml - 15);
        }
        i += len;
        anchor = i;
    }
    let lits = n - anchor;
    out.push((lits.min(15) as u8) << 4);
    if lits >= 15 {
        put_len(out, lits - 15);
    }
    out.extend_from_slice(&input[anchor..]);
}

/// Bytes the output is given past its end, so that copies of a fixed 16
/// bytes may run over it.
const SLACK: usize = 32;

/// The block `input`, decompressed to exactly `size` bytes (appended to
/// `out`); `None` if it is damaged (and `out` as it was).
///
/// As LZ4's decoder does, it copies in fixed pieces that may overrun what
/// a sequence needs — 16 bytes of literals or of a match at a time, the
/// excess written over by what follows — so a short sequence is a few
/// loads and stores, not a call to `memmove`: the output sized first,
/// with [`SLACK`] past its end, and cut to `size` at the end.  A match
/// that overlaps what it copies (its offset under 16) is copied a byte
/// at a time for 16 bytes, then 8 at a time from a multiple of its offset
/// at least 8 back, which repeats the same period.
pub fn decompress(input: &[u8], size: usize, out: &mut Vec<u8>) -> Option<()> {
    let start = out.len();
    out.resize(start + size + SLACK, 0);
    let done = run(input, size, &mut out[start..]);
    out.truncate(if done { start + size } else { start });
    done.then_some(())
}

/// [`decompress`] into `buf` (`size` + [`SLACK`] bytes); whether the block
/// was whole and gave `size` bytes.
fn run(input: &[u8], size: usize, buf: &mut [u8]) -> bool {
    let len = |i: &mut usize, mut n: usize| -> Option<usize> {
        loop {
            let b = *input.get(*i)?;
            *i += 1;
            n += b as usize;
            if b != 255 {
                return Some(n);
            }
        }
    };
    let mut i = 0;
    let mut o = 0;
    let mut step = || -> Option<bool> {
        let token = *input.get(i)?;
        i += 1;
        let mut lits = (token >> 4) as usize;
        if lits == 15 {
            lits = len(&mut i, 15)?;
        }
        if lits > size - o {
            return None;
        }
        if lits <= 16 && i + 16 <= input.len() {
            buf[o..o + 16].copy_from_slice(&input[i..i + 16]);
        } else {
            buf[o..o + lits].copy_from_slice(input.get(i..i.checked_add(lits)?)?);
        }
        i += lits;
        o += lits;
        if i >= input.len() {
            // The last sequence: literals only (and none read past the
            // block: a short block's 16-byte copy only reached its end).
            return (i == input.len()).then_some(false);
        }
        let off = u16::from_le_bytes([*input.get(i)?, *input.get(i + 1)?]) as usize;
        i += 2;
        let mut ml = (token & 15) as usize;
        if ml == 15 {
            ml = len(&mut i, 15)?;
        }
        let ml = ml + MIN_MATCH;
        if off == 0 || off > o || ml > size - o {
            return None;
        }
        let end = o + ml;
        let mut from = o - off;
        if off >= 16 {
            // No overlap within a piece: 16 at a time.
            while o < end {
                buf.copy_within(from..from + 16, o);
                o += 16;
                from += 16;
            }
        } else {
            // The period repeated a byte at a time first, then 8 at a time
            // from a multiple of it 8 to 16 bytes back.
            let first = ml.min(16);
            for _ in 0..first {
                buf[o] = buf[from];
                o += 1;
                from += 1;
            }
            let back = 16 / off * off;
            while o < end {
                buf.copy_within(o - back..o - back + 8, o);
                o += 8;
            }
        }
        o = end;
        Some(true)
    };
    loop {
        match step() {
            Some(true) => {}
            Some(false) => return o == size,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> usize {
        let mut c = Vec::new();
        compress(data, &mut c);
        let mut d = Vec::new();
        decompress(&c, data.len(), &mut d).expect("decompresses");
        assert_eq!(d, data);
        c.len()
    }

    #[test]
    fn blocks_round_trip_and_shrink_repeats() {
        assert_eq!(roundtrip(b""), 1);
        roundtrip(b"a");
        roundtrip(b"abcdefgh");
        assert!(roundtrip(&[0u8; 10_000]) < 100);
        let text: Vec<u8> = (0..2000)
            .flat_map(|i| format!("row {i} name {} ", i % 17).into_bytes())
            .collect();
        assert!(roundtrip(&text) < text.len() / 2);
        let mut x = 1u64;
        let noise: Vec<u8> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        roundtrip(&noise);
        for n in 0..300 {
            roundtrip(&text[..n]);
            roundtrip(&noise[..n]);
        }
    }

    /// The decoder before the wild copies (an `extend` a sequence): what
    /// the fast one must agree with.
    fn reference(input: &[u8], size: usize, out: &mut Vec<u8>) -> Option<()> {
        let start = out.len();
        let mut i = 0;
        let len = |i: &mut usize, mut n: usize| -> Option<usize> {
            loop {
                let b = *input.get(*i)?;
                *i += 1;
                n += b as usize;
                if b != 255 {
                    return Some(n);
                }
            }
        };
        loop {
            let token = *input.get(i)?;
            i += 1;
            let mut lits = (token >> 4) as usize;
            if lits == 15 {
                lits = len(&mut i, 15)?;
            }
            out.extend_from_slice(input.get(i..i.checked_add(lits)?)?);
            i += lits;
            if out.len() - start > size {
                return None;
            }
            if i == input.len() {
                break;
            }
            let off = u16::from_le_bytes([*input.get(i)?, *input.get(i + 1)?]) as usize;
            i += 2;
            let mut ml = (token & 15) as usize;
            if ml == 15 {
                ml = len(&mut i, 15)?;
            }
            let ml = ml + MIN_MATCH;
            let pos = out.len();
            if off == 0 || off > pos - start || pos - start + ml > size {
                return None;
            }
            let from = pos - off;
            let mut left = ml;
            while left > 0 {
                let n = left.min(out.len() - from);
                out.extend_from_within(from..from + n);
                left -= n;
            }
        }
        (out.len() - start == size).then_some(())
    }

    fn agree(block: &[u8], size: usize) {
        let (mut a, mut b) = (vec![7u8; 3], vec![7u8; 3]);
        let fast = decompress(block, size, &mut a);
        let slow = reference(block, size, &mut b);
        assert_eq!(fast, slow, "{block:?} {size}");
        if fast.is_some() {
            assert_eq!(a, b);
        } else {
            assert_eq!(a, [7, 7, 7], "a failure leaves the output as it was");
        }
    }

    /// The fast decoder against the plain one: periodic runs of every
    /// period to 20 (the overlapping copies), long and short sequences,
    /// blocks cut, damaged and made up.
    #[test]
    fn wild_copies_agree_with_the_plain_decoder() {
        let mut x = 7u64;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut inputs: Vec<Vec<u8>> = Vec::new();
        for period in 1..=20 {
            for n in [5, 17, 40, 300] {
                let pat: Vec<u8> = (0..period).map(|_| rand() as u8).collect();
                let mut v: Vec<u8> = (0..n).map(|_| rand() as u8).collect();
                v.extend(pat.iter().cycle().take(n * 3));
                v.extend((0..n).map(|_| rand() as u8 % 4));
                inputs.push(v);
            }
        }
        for _ in 0..200 {
            let n = (rand() % 3000) as usize;
            let alphabet = 1 + rand() % 40;
            inputs.push((0..n).map(|_| (rand() % alphabet) as u8).collect());
        }
        for data in &inputs {
            let mut c = Vec::new();
            compress(data, &mut c);
            for size in [data.len(), data.len() + 1, data.len().saturating_sub(1)] {
                agree(&c, size);
            }
            for cut in [
                c.len() / 2,
                c.len().saturating_sub(1),
                c.len().saturating_sub(17),
            ] {
                agree(&c[..cut], data.len());
            }
            for _ in 0..8 {
                let mut bad = c.clone();
                let at = (rand() as usize) % bad.len();
                bad[at] ^= 1 << (rand() % 8);
                agree(&bad, data.len());
            }
        }
        for _ in 0..3000 {
            let n = (rand() % 64) as usize;
            let block: Vec<u8> = (0..n).map(|_| rand() as u8).collect();
            agree(&block, (rand() % 200) as usize);
        }
    }

    #[test]
    fn damaged_blocks_are_refused() {
        let text: Vec<u8> = (0..500)
            .flat_map(|i| format!("v{} ", i % 9).into_bytes())
            .collect();
        let mut c = Vec::new();
        compress(&text, &mut c);
        let mut out = Vec::new();
        assert!(decompress(&c, text.len() + 1, &mut out).is_none());
        for cut in 0..c.len() {
            let mut out = Vec::new();
            let _ = decompress(&c[..cut], text.len(), &mut out); // no panic
        }
        for i in 0..c.len() {
            let mut bad = c.clone();
            bad[i] ^= 0x5A;
            let mut out = Vec::new();
            if decompress(&bad, text.len(), &mut out).is_some() {
                assert_eq!(out.len(), text.len());
            }
        }
    }
}
