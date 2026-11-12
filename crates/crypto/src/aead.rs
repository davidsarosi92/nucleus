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
fn block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN]) -> [u8; 64] {
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

/// XORs `data` with ChaCha20's key stream from block `counter` (§2.4).
fn chacha20(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN], data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(64).enumerate() {
        let ks = block(key, counter.wrapping_add(i as u32), nonce);
        for (b, k) in chunk.iter_mut().zip(ks) {
            *b ^= k;
        }
    }
}

/// Poly1305 (§2.5), in five 26-bit limbs (poly1305-donna's 32-bit form).
struct Poly1305 {
    r: [u32; 5],
    h: [u32; 5],
    pad: [u32; 4],
}

impl Poly1305 {
    fn new(key: &[u8; 32]) -> Self {
        let r = [
            le32(&key[0..]) & 0x03ff_ffff,
            (le32(&key[3..]) >> 2) & 0x03ff_ff03,
            (le32(&key[6..]) >> 4) & 0x03ff_c0ff,
            (le32(&key[9..]) >> 6) & 0x03f0_3fff,
            (le32(&key[12..]) >> 8) & 0x000f_ffff,
        ];
        let pad = [
            le32(&key[16..]),
            le32(&key[20..]),
            le32(&key[24..]),
            le32(&key[28..]),
        ];
        Poly1305 { r, h: [0; 5], pad }
    }

    /// One 16-byte block; `hibit` is 1 << 24 for a full block, 0 for the
    /// padded last one of a raw message (here every block is full: the
    /// AEAD pads with zeros itself).
    fn block(&mut self, m: &[u8; 16], hibit: u32) {
        let [r0, r1, r2, r3, r4] = self.r.map(u64::from);
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let mut h = self.h.map(u64::from);
        h[0] += u64::from(le32(&m[0..]) & 0x03ff_ffff);
        h[1] += u64::from((le32(&m[3..]) >> 2) & 0x03ff_ffff);
        h[2] += u64::from((le32(&m[6..]) >> 4) & 0x03ff_ffff);
        h[3] += u64::from((le32(&m[9..]) >> 6) & 0x03ff_ffff);
        h[4] += u64::from((le32(&m[12..]) >> 8) | hibit);
        let d0 = h[0] * r0 + h[1] * s4 + h[2] * s3 + h[3] * s2 + h[4] * s1;
        let mut d1 = h[0] * r1 + h[1] * r0 + h[2] * s4 + h[3] * s3 + h[4] * s2;
        let mut d2 = h[0] * r2 + h[1] * r1 + h[2] * r0 + h[3] * s4 + h[4] * s3;
        let mut d3 = h[0] * r3 + h[1] * r2 + h[2] * r1 + h[3] * r0 + h[4] * s4;
        let mut d4 = h[0] * r4 + h[1] * r3 + h[2] * r2 + h[3] * r1 + h[4] * r0;
        let mut c = d0 >> 26;
        let mut h0 = d0 & 0x03ff_ffff;
        d1 += c;
        c = d1 >> 26;
        let h1 = d1 & 0x03ff_ffff;
        d2 += c;
        c = d2 >> 26;
        let h2 = d2 & 0x03ff_ffff;
        d3 += c;
        c = d3 >> 26;
        let h3 = d3 & 0x03ff_ffff;
        d4 += c;
        c = d4 >> 26;
        let h4 = d4 & 0x03ff_ffff;
        h0 += c * 5;
        let c = h0 >> 26;
        h0 &= 0x03ff_ffff;
        let h1 = h1 + c;
        self.h = [h0 as u32, h1 as u32, h2 as u32, h3 as u32, h4 as u32];
    }

    fn update_padded(&mut self, data: &[u8]) {
        for chunk in data.chunks(16) {
            let mut m = [0u8; 16];
            m[..chunk.len()].copy_from_slice(chunk);
            self.block(&m, 1 << 24);
        }
    }

    fn finish(self) -> [u8; TAG_LEN] {
        let mut h = self.h;
        // Carry fully.
        let mut c = h[1] >> 26;
        h[1] &= 0x03ff_ffff;
        for i in [2, 3, 4] {
            h[i] += c;
            c = h[i] >> 26;
            h[i] &= 0x03ff_ffff;
        }
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= 0x03ff_ffff;
        h[1] += c;
        // g = h + 5 - 2^130; take g when h ≥ p, without a branch.
        let mut g = [0u32; 5];
        let mut c = 5u32;
        for i in 0..4 {
            g[i] = h[i].wrapping_add(c);
            c = g[i] >> 26;
            g[i] &= 0x03ff_ffff;
        }
        g[4] = h[4].wrapping_add(c).wrapping_sub(1 << 26);
        let mask = (g[4] >> 31).wrapping_sub(1); // all ones when g ≥ 0
        for i in 0..5 {
            h[i] = (h[i] & !mask) | (g[i] & mask);
        }
        // h mod 2^128, plus pad.
        let w = [
            h[0] | (h[1] << 26),
            (h[1] >> 6) | (h[2] << 20),
            (h[2] >> 12) | (h[3] << 14),
            (h[3] >> 18) | (h[4] << 8),
        ];
        let mut out = [0u8; TAG_LEN];
        let mut carry = 0u64;
        for i in 0..4 {
            let f = u64::from(w[i]) + u64::from(self.pad[i]) + carry;
            out[4 * i..4 * i + 4].copy_from_slice(&(f as u32).to_le_bytes());
            carry = f >> 32;
        }
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
    p.block(&lens, 1 << 24);
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

    #[test]
    fn poly1305() {
        // §2.5.2.
        let key = hex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        let mut p = Poly1305::new(key.as_slice().try_into().unwrap());
        let msg = b"Cryptographic Forum Research Group";
        let (blocks, rest) = msg.as_chunks::<16>();
        for c in blocks {
            p.block(c, 1 << 24);
        }
        let mut last = [0u8; 16];
        last[..rest.len()].copy_from_slice(rest);
        last[rest.len()] = 1;
        p.block(&last, 0);
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
