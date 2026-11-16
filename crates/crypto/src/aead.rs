//! ChaCha20-Poly1305 (RFC 8439): authenticated encryption, for values the
//! database keeps that must not be readable from its files (PLAN.md
//! §M14.22: secrets).  Written from the RFC and checked by its test
//! vectors; no table lookups or branches on secret data.

/// A key's length.
pub const KEY_LEN: usize = 32;
/// A nonce's length.
pub const NONCE_LEN: usize = 12;
/// The tag's length.
pub const TAG_LEN: usize = 16;

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The ChaCha20 block function (RFC 8439 §2.3).
pub(crate) fn block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        init[4 + i] = le32(&key[4 * i..]);
    }
    init[12] = counter;
    for i in 0..3 {
        init[13 + i] = le32(&nonce[4 * i..]);
    }
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// Blocks computed side by side: each state word a row of `LANES`
/// values, one a block, so the compiler turns the rounds into vector
/// instructions (NEON, SSE/AVX) without `unsafe` — the layout of
/// Bernstein's and Goll–Gueron's vectorized ChaCha20.
const LANES: usize = 4;

type Row = [u32; LANES];

#[inline(always)]
fn add(a: &mut Row, b: &Row) {
    for i in 0..LANES {
        a[i] = a[i].wrapping_add(b[i]);
    }
}

#[inline(always)]
fn xor_rot(a: &mut Row, b: &Row, n: u32) {
    for i in 0..LANES {
        a[i] = (a[i] ^ b[i]).rotate_left(n);
    }
}

#[inline(always)]
fn quarter_rows(s: &mut [Row; 16], a: usize, b: usize, c: usize, d: usize) {
    let (mut ra, mut rb, mut rc, mut rd) = (s[a], s[b], s[c], s[d]);
    add(&mut ra, &rb);
    xor_rot(&mut rd, &ra, 16);
    add(&mut rc, &rd);
    xor_rot(&mut rb, &rc, 12);
    add(&mut ra, &rb);
    xor_rot(&mut rd, &ra, 8);
    add(&mut rc, &rd);
    xor_rot(&mut rb, &rc, 7);
    (s[a], s[b], s[c], s[d]) = (ra, rb, rc, rd);
}

/// `LANES` blocks from `counter` on, XORed into `data` (`LANES` × 64
/// bytes).
fn blocks_xor(
    key: &[u8; KEY_LEN],
    counter: u32,
    nonce: &[u8; NONCE_LEN],
    data: &mut [u8; 64 * LANES],
) {
    let mut init = [[0u32; LANES]; 16];
    let words = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    for i in 0..4 {
        init[i] = [words[i]; LANES];
    }
    for i in 0..8 {
        init[4 + i] = [le32(&key[4 * i..]); LANES];
    }
    for (l, c) in init[12].iter_mut().enumerate() {
        *c = counter.wrapping_add(l as u32);
    }
    for i in 0..3 {
        init[13 + i] = [le32(&nonce[4 * i..]); LANES];
    }
    let mut s = init;
    for _ in 0..10 {
        quarter_rows(&mut s, 0, 4, 8, 12);
        quarter_rows(&mut s, 1, 5, 9, 13);
        quarter_rows(&mut s, 2, 6, 10, 14);
        quarter_rows(&mut s, 3, 7, 11, 15);
        quarter_rows(&mut s, 0, 5, 10, 15);
        quarter_rows(&mut s, 1, 6, 11, 12);
        quarter_rows(&mut s, 2, 7, 8, 13);
        quarter_rows(&mut s, 3, 4, 9, 14);
    }
    for w in 0..16 {
        add(&mut s[w], &init[w]);
    }
    for (l, chunk) in data.as_chunks_mut::<64>().0.iter_mut().enumerate() {
        for w in 0..16 {
            let k = s[w][l].to_le_bytes();
            for j in 0..4 {
                chunk[4 * w + j] ^= k[j];
            }
        }
    }
}

/// XORs `data` with ChaCha20's key stream from block `counter` (§2.4).
pub(crate) fn chacha20(
    key: &[u8; KEY_LEN],
    counter: u32,
    nonce: &[u8; NONCE_LEN],
    data: &mut [u8],
) {
    let (wide, tail) = data.as_chunks_mut::<{ 64 * LANES }>();
    for (i, chunk) in wide.iter_mut().enumerate() {
        blocks_xor(key, counter.wrapping_add((i * LANES) as u32), nonce, chunk);
    }
    let first = counter.wrapping_add((wide.len() * LANES) as u32);
    for (i, chunk) in tail.chunks_mut(64).enumerate() {
        let ks = block(key, first.wrapping_add(i as u32), nonce);
        for (b, k) in chunk.iter_mut().zip(ks) {
            *b ^= k;
        }
    }
}

/// Poly1305 (§2.5), in three limbs of 44, 44 and 42 bits multiplied
/// into 128-bit products (poly1305-donna's 64-bit form: a 64×64 multiply
/// is one or two instructions where 64-bit integers are native).
pub(crate) struct Poly1305 {
    r: [u64; 3],
    h: [u64; 3],
    pad: [u64; 2],
}

const M44: u64 = (1 << 44) - 1;
const M42: u64 = (1 << 42) - 1;

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().expect("8 bytes"))
}

impl Poly1305 {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        let (t0, t1) = (le64(&key[0..]), le64(&key[8..]));
        let r = [
            t0 & 0xffc_0fff_ffff,
            ((t0 >> 44) | (t1 << 20)) & 0xfff_ffc0_ffff,
            (t1 >> 24) & 0x00f_ffff_fc0f,
        ];
        Poly1305 {
            r,
            h: [0; 3],
            pad: [le64(&key[16..]), le64(&key[24..])],
        }
    }

    /// One 16-byte block: `full` for a whole 16 bytes of message (the
    /// 2^128 bit added); not for a short last one of a raw message, padded
    /// with its 1 byte by the caller.
    pub(crate) fn block(&mut self, m: &[u8; 16], full: bool) {
        let [r0, r1, r2] = self.r.map(u128::from);
        let (s1, s2) = (r1 * 20, r2 * 20);
        let (t0, t1) = (le64(&m[0..]), le64(&m[8..]));
        let hibit = if full { 1 << 40 } else { 0 };
        let h0 = u128::from(self.h[0] + (t0 & M44));
        let h1 = u128::from(self.h[1] + (((t0 >> 44) | (t1 << 20)) & M44));
        let h2 = u128::from(self.h[2] + (((t1 >> 24) & M42) | hibit));
        let d0 = h0 * r0 + h1 * s2 + h2 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0;
        let mut c = (d0 >> 44) as u64;
        let mut h0 = (d0 as u64) & M44;
        d1 += u128::from(c);
        c = (d1 >> 44) as u64;
        let mut h1 = (d1 as u64) & M44;
        d2 += u128::from(c);
        c = (d2 >> 42) as u64;
        let h2 = (d2 as u64) & M42;
        h0 += c * 5;
        c = h0 >> 44;
        h0 &= M44;
        h1 += c;
        self.h = [h0, h1, h2];
    }

    pub(crate) fn update_padded(&mut self, data: &[u8]) {
        let (blocks, rest) = data.as_chunks::<16>();
        for b in blocks {
            self.block(b, true);
        }
        if !rest.is_empty() {
            let mut m = [0u8; 16];
            m[..rest.len()].copy_from_slice(rest);
            self.block(&m, true);
        }
    }

    pub(crate) fn finish(self) -> [u8; TAG_LEN] {
        let [mut h0, mut h1, mut h2] = self.h;
        // Carry fully, twice round.
        for _ in 0..2 {
            let mut c = h1 >> 44;
            h1 &= M44;
            h2 += c;
            c = h2 >> 42;
            h2 &= M42;
            h0 += c * 5;
            c = h0 >> 44;
            h0 &= M44;
            h1 += c;
        }
        // g = h + 5 - 2^130; take g when h ≥ p, without a branch.
        let mut g0 = h0 + 5;
        let mut c = g0 >> 44;
        g0 &= M44;
        let mut g1 = h1 + c;
        c = g1 >> 44;
        g1 &= M44;
        let g2 = (h2 + c).wrapping_sub(1 << 42);
        let mask = (g2 >> 63).wrapping_sub(1); // all ones when g ≥ 0
        h0 = (h0 & !mask) | (g0 & mask);
        h1 = (h1 & !mask) | (g1 & mask);
        h2 = (h2 & !mask) | (g2 & mask);
        // h + pad, mod 2^128.
        let [t0, t1] = self.pad;
        h0 += t0 & M44;
        c = h0 >> 44;
        h0 &= M44;
        h1 += (((t0 >> 44) | (t1 << 20)) & M44) + c;
        c = h1 >> 44;
        h1 &= M44;
        h2 = (h2 + ((t1 >> 24) & M42) + c) & M42;
        let lo = h0 | (h1 << 44);
        let hi = (h1 >> 20) | (h2 << 24);
        let mut out = [0u8; TAG_LEN];
        out[..8].copy_from_slice(&lo.to_le_bytes());
        out[8..].copy_from_slice(&hi.to_le_bytes());
        out
    }
}

fn tag(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], aad: &[u8], ct: &[u8]) -> [u8; TAG_LEN] {
    let otk = block(key, 0, nonce);
    let mut p = Poly1305::new(otk[..32].try_into().expect("32 bytes"));
    p.update_padded(aad);
    p.update_padded(ct);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..].copy_from_slice(&(ct.len() as u64).to_le_bytes());
    p.block(&lens, true);
    p.finish()
}

/// Encrypts `plaintext` with `aad` authenticated beside it (§2.8): the
/// ciphertext followed by the 16-byte tag.  A nonce must never be used
/// twice with one key.
pub fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut out = plaintext.to_vec();
    chacha20(key, 1, nonce, &mut out);
    let t = tag(key, nonce, aad, &out);
    out.extend_from_slice(&t);
    out
}

/// Decrypts what [`seal`] made; `None` when the key, the nonce, the
/// `aad` or a byte of it is not what it was sealed with.
pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    let split = sealed.len().checked_sub(TAG_LEN)?;
    let (ct, t) = sealed.split_at(split);
    if !crate::constant_time_eq(&tag(key, nonce, aad, ct), t) {
        return None;
    }
    let mut out = ct.to_vec();
    chacha20(key, 1, nonce, &mut out);
    Some(out)
}

/// An extended nonce's length (XChaCha20-Poly1305).
pub const XNONCE_LEN: usize = 24;

/// HChaCha20 (draft-irtf-cfrg-xchacha §2.2): a subkey from `key` and the
/// first 16 bytes of an extended nonce — ChaCha20's rounds without the
/// final addition, the state's first and last rows.
pub fn hchacha20(key: &[u8; KEY_LEN], nonce: &[u8; 16]) -> [u8; KEY_LEN] {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        s[4 + i] = le32(&key[4 * i..]);
    }
    for i in 0..4 {
        s[12 + i] = le32(&nonce[4 * i..]);
    }
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; KEY_LEN];
    for (i, w) in s[..4].iter().chain(&s[12..]).enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// XChaCha20-Poly1305's subkey and nonce: HChaCha20 of the nonce's first
/// 16 bytes, and four zero bytes before its last 8.
fn extended(key: &[u8; KEY_LEN], nonce: &[u8; XNONCE_LEN]) -> ([u8; KEY_LEN], [u8; NONCE_LEN]) {
    let mut head = [0u8; 16];
    head.copy_from_slice(&nonce[..16]);
    let mut n = [0u8; NONCE_LEN];
    n[4..].copy_from_slice(&nonce[16..]);
    (hchacha20(key, &head), n)
}

/// [`seal`] with a 24-byte nonce (XChaCha20-Poly1305): long enough to be
/// drawn at random for every message under one key (PLAN.md §M14.30
/// K5's encrypted columns).
pub fn xseal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; XNONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let (k, n) = extended(key, nonce);
    seal(&k, &n, aad, plaintext)
}

/// Decrypts what [`xseal`] made.
pub fn xopen(
    key: &[u8; KEY_LEN],
    nonce: &[u8; XNONCE_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    let (k, n) = extended(key, nonce);
    open(&k, &n, aad, sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// draft-irtf-cfrg-xchacha-03 §2.2.1 and A.3.1.
    #[test]
    fn xchacha20_poly1305() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let nonce: [u8; 16] = hex("000000090000004a0000000031415927").try_into().unwrap();
        assert_eq!(
            hchacha20(&key, &nonce).to_vec(),
            hex("82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc")
        );
        let key: [u8; 32] = std::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; 24] = std::array::from_fn(|i| 0x40 + i as u8);
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let sealed = xseal(&key, &nonce, &aad, plain);
        let mut want = hex(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb
             731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452
             2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9
             21f9664c97637da9768812f615c68b13b52e",
        );
        want.extend(hex("c0875924c1c7987947deafd8780acf49"));
        assert_eq!(sealed, want);
        assert_eq!(xopen(&key, &nonce, &aad, &sealed).unwrap(), plain);
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(xopen(&key, &nonce, &aad, &bad).is_none());
    }

    #[test]
    fn chacha20_block() {
        // RFC 8439 §2.3.2.
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce = hex("000000090000004a00000000").try_into().unwrap();
        let out = block(&key, 1, &nonce);
        assert_eq!(
            out.to_vec(),
            hex(
                "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e
                 d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
            )
        );
    }

    #[test]
    fn chacha20_encryption() {
        // §2.4.2.
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce = hex("000000000000004a00000000").try_into().unwrap();
        let mut data = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.".to_vec();
        chacha20(&key, 1, &nonce, &mut data);
        assert_eq!(
            data,
            hex(
                "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b
                 f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8
                 07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736
                 5af90bbf74a35be6b40b8eedf2785e42874d"
            )
        );
    }

    /// Raw-message Poly1305, short last block padded as §2.5 says.
    fn poly_message(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
        let mut p = Poly1305::new(key);
        let (blocks, rest) = msg.as_chunks::<16>();
        for c in blocks {
            p.block(c, true);
        }
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            last[..rest.len()].copy_from_slice(rest);
            last[rest.len()] = 1;
            p.block(&last, false);
        }
        p.finish()
    }

    /// Keys and messages that drive every carry and the final reduction,
    /// against `crates/crypto/tests/siv_reference.py poly` (Python
    /// integers, no limbs).
    #[test]
    fn poly1305_edges() {
        for (key, len, byte, want) in POLY_EDGES {
            let key: [u8; 32] = hex(key).try_into().unwrap();
            let msg = vec![*byte; *len];
            assert_eq!(
                poly_message(&key, &msg).to_vec(),
                hex(want),
                "key {key:02x?} len {len}"
            );
        }
    }

    const POLY_EDGES: &[(&str, usize, u8, &str)] = &[
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            16,
            255,
            "fbffff17faffff17faffff17faffff17",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            17,
            255,
            "7cfe7ff768f81f2763f8bf565df85f86",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            32,
            255,
            "5400801f3f00204f3900c07e330060ae",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            63,
            255,
            "900f0bfaca5fd0a5c6a817b3d1e3a687",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            64,
            255,
            "900fe32bc15fa8d7bca8efe4c7e37eb1",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            160,
            255,
            "99514d0a5a9b209c6a3dd0a4f64d95ae",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            48,
            0,
            "f87c2e84ee7cc5ea0ebd6d8c2a3d2769",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            16,
            255,
            "fcffff17faffff17faffff17faffff17",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            17,
            255,
            "7dfe7ff768f81f2763f8bf565df85f86",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            32,
            255,
            "5500801f3f00204f3900c07e330060ae",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            63,
            255,
            "910f0bfaca5fd0a5c6a817b3d1e3a687",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            64,
            255,
            "910fe32bc15fa8d7bca8efe4c7e37eb1",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            160,
            255,
            "9a514d0a5a9b209c6a3dd0a4f64d95ae",
        ),
        (
            "ffffffffffffffffffffffffffffffff00000000000000000000000000000000",
            48,
            0,
            "f97c2e84ee7cc5ea0ebd6d8c2a3d2769",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            16,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            17,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            32,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            63,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            64,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            160,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            48,
            0,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            16,
            255,
            "ffffffffffffffffffffffffffffffff",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            17,
            255,
            "fe010000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            32,
            255,
            "03000000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            63,
            255,
            "01000000000000000000000000000002",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            64,
            255,
            "06000000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            160,
            255,
            "0f000000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            48,
            0,
            "00000000000000000000000000000000",
        ),
    ];

    #[test]
    fn poly1305() {
        // §2.5.2.
        let key = hex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        let mut p = Poly1305::new(key.as_slice().try_into().unwrap());
        let msg = b"Cryptographic Forum Research Group";
        let (blocks, rest) = msg.as_chunks::<16>();
        for c in blocks {
            p.block(c, true);
        }
        let mut last = [0u8; 16];
        last[..rest.len()].copy_from_slice(rest);
        last[rest.len()] = 1;
        p.block(&last, false);
        assert_eq!(p.finish().to_vec(), hex("a8061dc1305136c6c22b8baf0c0127a9"));
    }

    #[test]
    fn aead() {
        // §2.8.2.
        let key: [u8; 32] = core::array::from_fn(|i| 0x80 + i as u8);
        let nonce = hex("070000004041424344454647").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let p = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let sealed = seal(&key, &nonce, &aad, p);
        assert_eq!(
            sealed,
            hex(
                "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6
                 3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36
                 92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc
                 3ff4def08e4b7a9de576d26586cec64b6116
                 1ae10b594f09e26a7e902ecbd0600691"
            )
        );
        assert_eq!(open(&key, &nonce, &aad, &sealed).unwrap(), p);
        // Any change is refused.
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(open(&key, &nonce, &aad, &bad).is_none(), "byte {i}");
        }
        assert!(open(&key, &nonce, b"other", &sealed).is_none());
        let mut other = key;
        other[0] ^= 1;
        assert!(open(&other, &nonce, &aad, &sealed).is_none());
        assert!(open(&key, &nonce, &aad, &sealed[..10]).is_none());
        // Lengths around the block and the tag.
        for n in [0, 1, 15, 16, 17, 63, 64, 65, 200] {
            let p: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let s = seal(&key, &nonce, b"", &p);
            assert_eq!(s.len(), n + TAG_LEN);
            assert_eq!(open(&key, &nonce, b"", &s).unwrap(), p);
        }
    }
}

#[cfg(test)]
mod speed {
    use super::*;

    /// ChaCha20 and Poly1305 apart, on 8 KiB pages (PLAN.md §M14.30 K2a).
    /// `cargo test --release -p nucleus-crypto speed -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement: run it in release"]
    fn halves() {
        let key = [0x42u8; 32];
        let mut page = vec![7u8; 8192];
        let n = 4096;
        let t = std::time::Instant::now();
        for i in 0..n {
            chacha20(&key, i, &[0; 12], &mut page);
        }
        let c = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        let mut x = 0u8;
        for _ in 0..n {
            let mut p = Poly1305::new(&key);
            p.update_padded(&page);
            x ^= p.finish()[0];
        }
        let p = t.elapsed().as_secs_f64();
        let t = std::time::Instant::now();
        for i in 0..n {
            for (j, chunk) in page.chunks_mut(64).enumerate() {
                let ks = block(&key, i.wrapping_add(j as u32), &[0; 12]);
                for (b, k) in chunk.iter_mut().zip(ks) {
                    *b ^= k;
                }
            }
        }
        let one = t.elapsed().as_secs_f64();
        let mb = 8192.0 * f64::from(n) / 1e6;
        println!(
            "chacha20 {:.0} MB/s ({LANES} lanes; a block at a time {:.0}), poly1305 {:.0} MB/s ({x})",
            mb / c,
            mb / one,
            mb / p
        );
    }
}
