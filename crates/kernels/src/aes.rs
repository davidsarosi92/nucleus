//! AES-256-GCM-SIV (RFC 8452): the misuse-resistant AEAD of encrypted
//! pages, logs and scratch where the CPU has AES instructions (PLAN.md
//! §M14.30, the speed after K2) — ARMv8's AES and PMULL, x86-64's AES-NI
//! and PCLMULQDQ — and a portable, constant-time copy for the rest (a
//! file made on one machine read on another; and the reference the
//! instructions are tested against).
//!
//! For a key `K`, a nonce `N`, associated data `A` and a plaintext `P`:
//! the message keys from `AES(K, le32(i) ‖ N)`, `i` = 0…5, the first 8
//! bytes of each — the authentication key `H` (0, 1), the encryption key
//! (2…5); `S` = POLYVAL(`H`, `A` and `P` zero-padded to 16 bytes, their
//! lengths in bits as two little-endian 64-bit words), its first 12 bytes
//! XORed with `N`, its top bit cleared; the tag `T` = AES(encryption key,
//! `S`); the ciphertext `P` XOR AES-CTR under the encryption key from `T`
//! with its top bit set, the first 32 bits a little-endian counter.
//!
//! POLYVAL multiplies in GF(2^128) modulo x^128 + x^127 + x^126 + x^121 +
//! 1 with the bits in little-endian order, so a carry-less multiply needs
//! no reflection; `dot(a, b)` = a·b·x^-128, a Montgomery product —
//! reduced 64 bits at a time by a multiply with x^57 + x^62 + x^63
//! (`0xC200…`), since the modulus is 1 modulo x^64.
//!
//! The instructions' copy keeps blocks in vector registers and takes the
//! known speed-ups of GCM and GCM-SIV implementations (Gueron et al.):
//! POLYVAL eight blocks at a time — the products with H^8 … H^1 summed
//! unreduced and reduced once, as the reduction is linear and
//! dot(dot(a, H), H) = dot(a, dot(H, H)) — and the counter blocks
//! encrypted eight at a time, round by round, so the AES units never wait.
//!
//! Blocks are `u128`s read little-endian.  The portable copy's S-box is
//! the inverse in GF(2^8) (x^254, by multiplies without branches or
//! tables) and the affine map, its carry-less multiply a masked
//! shift-and-add: no secret-dependent branch or memory access, and slow —
//! it is for portability, not speed.

use std::sync::OnceLock;

/// The key's bytes.
pub const KEY_LEN: usize = 32;
/// The nonce's bytes.
pub const NONCE_LEN: usize = 12;
/// The tag's bytes.
pub const TAG_LEN: usize = 16;

/// Whether this CPU has the instructions the fast copy uses.
pub fn hardware() -> bool {
    static HW: OnceLock<bool> = OnceLock::new();
    *HW.get_or_init(|| {
        #[cfg(target_arch = "aarch64")]
        {
            std::arch::is_aarch64_feature_detected!("aes")
        }
        #[cfg(target_arch = "x86_64")]
        {
            std::arch::is_x86_feature_detected!("aes")
                && std::arch::is_x86_feature_detected!("pclmulqdq")
                && std::arch::is_x86_feature_detected!("sse4.1")
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            false
        }
    })
}

/// A key, its AES round keys expanded once; zeroed when dropped.
#[derive(Clone)]
pub struct Key {
    rk: [u128; 15],
}

impl Key {
    /// The key of these bytes.
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        #[allow(unsafe_code)]
        if hardware() {
            // SAFETY: the CPU has the instructions (`hardware`).
            return Self {
                rk: unsafe { hard::expand(key) },
            };
        }
        Self {
            rk: soft::expand(key),
        }
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("aes::Key(..)")
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.rk.fill(0);
        std::hint::black_box(&self.rk);
    }
}

/// Seals `data` in place: it becomes the ciphertext; the tag is returned.
pub fn seal_in_place(
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
) -> [u8; TAG_LEN] {
    seal_with(hardware(), key, nonce, aad, data)
}

/// Opens in place what [`seal_in_place`] sealed: `true` with `data` the
/// plaintext; `false` — anything not as sealed — with `data` zeroed.
pub fn open_in_place(
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> bool {
    open_with(hardware(), key, nonce, aad, data, tag)
}

/// [`seal_in_place`] by the instructions (`hw`, which the CPU must have)
/// or the portable copy.
#[doc(hidden)]
pub fn seal_with(
    hw: bool,
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
) -> [u8; TAG_LEN] {
    assert!(!hw || hardware(), "the CPU lacks AES instructions");
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[allow(unsafe_code)]
    if hw {
        // SAFETY: the CPU has the instructions (`hardware`, checked above).
        return unsafe { hard::seal(&key.rk, nonce, aad, data) };
    }
    soft::seal(&key.rk, nonce, aad, data)
}

/// [`open_in_place`] by the instructions or the portable copy.
#[doc(hidden)]
pub fn open_with(
    hw: bool,
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> bool {
    assert!(!hw || hardware(), "the CPU lacks AES instructions");
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[allow(unsafe_code)]
    if hw {
        // SAFETY: the CPU has the instructions (`hardware`, checked above).
        return unsafe { hard::open(&key.rk, nonce, aad, data, tag) };
    }
    soft::open(&key.rk, nonce, aad, data, tag)
}

/// Equal in time independent of where they differ.
fn ct_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    std::hint::black_box(d) == 0
}

/// The reduction constant: x^57 + x^62 + x^63.
const POLY: u64 = 0xC200_0000_0000_0000;

/// The round constants of AES's key schedule.
const RCON: [u32; 7] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40];

/// Plaintexts from this length on hash eight blocks at a time (the powers
/// of H cost seven products).
const WIDE: usize = 128;

/// The construction, over a backend's primitives — `sub_word`,
/// `encrypt_block`, `encrypt_blocks` (several, independent), `Powers` /
/// `powers` (of H, as many as `polyval`
/// uses), `polyval` (over whole blocks of a slice, the last zero-padded)
/// and `ctr_xor` — written once, compiled in each backend's module with
/// its target features.
macro_rules! construction {
    ($(#[$a:meta])*) => {
        $(#[$a])*
        pub(super) fn expand(key: &[u8; 32]) -> [u128; 15] {
            let mut w = [0u32; 60];
            for (i, c) in key.chunks_exact(4).enumerate() {
                w[i] = u32::from_le_bytes(c.try_into().expect("4 bytes"));
            }
            for i in 8..60 {
                let mut t = w[i - 1];
                if i % 8 == 0 {
                    t = sub_word(t.rotate_right(8)) ^ super::RCON[i / 8 - 1];
                } else if i % 8 == 4 {
                    t = sub_word(t);
                }
                w[i] = w[i - 8] ^ t;
            }
            let mut rk = [0u128; 15];
            for (r, k) in rk.iter_mut().enumerate() {
                *k = (0..4).fold(0u128, |acc, j| acc | (u128::from(w[4 * r + j]) << (32 * j)));
            }
            w.fill(0);
            std::hint::black_box(&w);
            rk
        }

        $(#[$a])*
        fn derive(rk: &[u128; 15], nonce: &[u8; 12]) -> (u128, [u128; 15]) {
            let mut n = [0u8; 16];
            n[4..].copy_from_slice(nonce);
            let base = u128::from_le_bytes(n);
            let mut blocks = [0u128; 6];
            for (i, b) in blocks.iter_mut().enumerate() {
                *b = base | i as u128;
            }
            encrypt_blocks(rk, &mut blocks);
            let mut out = [0u8; 48];
            for (i, b) in blocks.iter().enumerate() {
                out[8 * i..8 * i + 8].copy_from_slice(&b.to_le_bytes()[..8]);
            }
            blocks.fill(0);
            let h = u128::from_le_bytes(out[..16].try_into().expect("16 bytes"));
            let enc = expand(out[16..48].try_into().expect("32 bytes"));
            out.fill(0);
            std::hint::black_box(&out);
            (h, enc)
        }

        $(#[$a])*
        fn tag_of(h: u128, enc: &[u128; 15], nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> u128 {
            let p = powers(h, if plain.len() >= super::WIDE { 8 } else { 1 });
            let mut s = polyval(&p, 0, aad);
            s = polyval(&p, s, plain);
            let lengths = (aad.len() as u128 * 8) | ((plain.len() as u128 * 8) << 64);
            s = polyval(&p, s, &lengths.to_le_bytes());
            let mut n = [0u8; 16];
            n[..12].copy_from_slice(nonce);
            s ^= u128::from_le_bytes(n);
            s &= !(1u128 << 127);
            encrypt_block(enc, s)
        }

        $(#[$a])*
        pub(super) fn seal(
            rk: &[u128; 15],
            nonce: &[u8; 12],
            aad: &[u8],
            data: &mut [u8],
        ) -> [u8; 16] {
            let (h, enc) = derive(rk, nonce);
            let tag = tag_of(h, &enc, nonce, aad, data);
            ctr_xor(&enc, tag | (1u128 << 127), data);
            tag.to_le_bytes()
        }

        $(#[$a])*
        pub(super) fn open(
            rk: &[u128; 15],
            nonce: &[u8; 12],
            aad: &[u8],
            data: &mut [u8],
            tag: &[u8; 16],
        ) -> bool {
            let (h, enc) = derive(rk, nonce);
            let t = u128::from_le_bytes(*tag);
            ctr_xor(&enc, t | (1u128 << 127), data);
            if super::ct_eq(&tag_of(h, &enc, nonce, aad, data).to_le_bytes(), tag) {
                return true;
            }
            data.fill(0);
            false
        }
    };
}

/// POLYVAL and CTR over vector registers, from a backend's primitives:
/// the vector `V`; `from` / `into` a `u128`; `load` / `store` 16 bytes;
/// `xor`; `zero`; `clmul_lo` / `clmul_hi` (the carry-less products of
/// the low and of the high halves); `swap` (the halves); `shl64` (the low
/// half moved up, zeros below) and `shr64`; `aes` (a block) and `aes8`
/// (eight, round by round); `next` (the counter's low 32 bits plus one,
/// wrapping).
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
macro_rules! vector {
    ($(#[$a:meta])*) => {
        /// H^1 … H^n (Montgomery powers), each also with its halves
        /// swapped (for the middle products).
        pub(super) struct Powers {
            h: [V; 8],
            hs: [V; 8],
            n: usize,
        }

        /// `s` (low, middle, high partial products) plus a·h, unreduced.
        $(#[$a])*
        fn mul_into(s: &mut [V; 3], a: V, h: V, hs: V) {
            s[0] = xor(s[0], clmul_lo(a, h));
            s[2] = xor(s[2], clmul_hi(a, h));
            s[1] = xor(s[1], xor(clmul_lo(a, hs), clmul_hi(a, hs)));
        }

        /// The partial products' 256 bits times x^-128 (two folds by POLY).
        $(#[$a])*
        fn reduce(s: [V; 3]) -> V {
            let x = xor(s[0], shl64(s[1]));
            let y = xor(s[2], shr64(s[1]));
            let p = from(u128::from(super::POLY));
            let w = xor(swap(x), clmul_lo(x, p));
            xor(y, xor(swap(w), clmul_lo(w, p)))
        }

        $(#[$a])*
        fn dot(a: V, h: V, hs: V) -> V {
            let mut s = [zero(); 3];
            mul_into(&mut s, a, h, hs);
            reduce(s)
        }

        $(#[$a])*
        pub(super) fn powers(h: u128, n: usize) -> Powers {
            let h1 = from(h);
            let mut p = Powers {
                h: [h1; 8],
                hs: [swap(h1); 8],
                n,
            };
            // H^k = H^a · H^b, a the largest power of two below k: three
            // products deep for H^8, not seven.
            for i in 1..n {
                let k = i + 1;
                let a = 1 << (usize::BITS - 1 - (k - 1).leading_zeros());
                p.h[i] = dot(p.h[a - 1], p.h[k - a - 1], p.hs[k - a - 1]);
                p.hs[i] = swap(p.h[i]);
            }
            p
        }

        $(#[$a])*
        pub(super) fn polyval(p: &Powers, acc: u128, bytes: &[u8]) -> u128 {
            let mut acc = from(acc);
            let (blocks, rest) = bytes.as_chunks::<16>();
            let split = if p.n == 8 { blocks.len() / 8 * 8 } else { 0 };
            for eight in blocks[..split].chunks_exact(8) {
                let mut s = [zero(); 3];
                for (j, b) in eight.iter().enumerate() {
                    let x = if j == 0 { xor(acc, load(b)) } else { load(b) };
                    mul_into(&mut s, x, p.h[7 - j], p.hs[7 - j]);
                }
                acc = reduce(s);
            }
            for b in &blocks[split..] {
                acc = dot(xor(acc, load(b)), p.h[0], p.hs[0]);
            }
            if !rest.is_empty() {
                let mut b = [0u8; 16];
                b[..rest.len()].copy_from_slice(rest);
                acc = dot(xor(acc, load(&b)), p.h[0], p.hs[0]);
            }
            into(acc)
        }

        $(#[$a])*
        fn keys(rk: &[u128; 15]) -> [V; 15] {
            rk.map(|k| from(k))
        }

        $(#[$a])*
        pub(super) fn encrypt_block(rk: &[u128; 15], b: u128) -> u128 {
            into(aes(&keys(rk), from(b)))
        }

        $(#[$a])*
        fn encrypt_blocks(rk: &[u128; 15], blocks: &mut [u128]) {
            let k = keys(rk);
            for b in blocks {
                *b = into(aes(&k, from(*b)));
            }
        }

        $(#[$a])*
        pub(super) fn ctr_xor(rk: &[u128; 15], first: u128, data: &mut [u8]) {
            let k = keys(rk);
            let mut ctr = from(first);
            let (wide, rest) = data.as_chunks_mut::<128>();
            for chunk in wide {
                let mut s = [zero(); 8];
                for x in &mut s {
                    *x = ctr;
                    ctr = next(ctr);
                }
                aes8(&k, &mut s);
                let (blocks, _) = chunk.as_chunks_mut::<16>();
                for (b, x) in blocks.iter_mut().zip(s) {
                    let y = xor(load(b), x);
                    store(y, b);
                }
            }
            for chunk in rest.chunks_mut(16) {
                let mut b = [0u8; 16];
                b[..chunk.len()].copy_from_slice(chunk);
                let y = xor(load(&b), aes(&k, ctr));
                store(y, &mut b);
                chunk.copy_from_slice(&b[..chunk.len()]);
                ctr = next(ctr);
            }
        }
    };
}

/// The portable, constant-time copy.
mod soft {
    /// a·b in GF(2^8) (AES's field), without branches.
    fn gmul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        for _ in 0..8 {
            p ^= a & 0u8.wrapping_sub(b & 1);
            let hi = 0u8.wrapping_sub(a >> 7);
            a = (a << 1) ^ (0x1B & hi);
            b >>= 1;
        }
        p
    }

    /// AES's S-box: the inverse (x^254; 0 for 0), then the affine map.
    fn sbox(x: u8) -> u8 {
        let x2 = gmul(x, x);
        let x3 = gmul(x2, x);
        let x6 = gmul(x3, x3);
        let x12 = gmul(x6, x6);
        let x15 = gmul(x12, x3);
        let x30 = gmul(x15, x15);
        let x60 = gmul(x30, x30);
        let x120 = gmul(x60, x60);
        let x126 = gmul(x120, x6);
        let x127 = gmul(x126, x);
        let inv = gmul(x127, x127); // x^254
        inv ^ inv.rotate_left(1)
            ^ inv.rotate_left(2)
            ^ inv.rotate_left(3)
            ^ inv.rotate_left(4)
            ^ 0x63
    }

    fn sub_word(w: u32) -> u32 {
        u32::from_le_bytes(w.to_le_bytes().map(sbox))
    }

    fn xtime(b: u8) -> u8 {
        (b << 1) ^ (0x1B & 0u8.wrapping_sub(b >> 7))
    }

    pub(super) fn encrypt_block(rk: &[u128; 15], b: u128) -> u128 {
        let mut s = (b ^ rk[0]).to_le_bytes();
        for (round, k) in rk.iter().enumerate().skip(1) {
            let mut t = [0u8; 16];
            // SubBytes and ShiftRows: byte (row r, column c) from column c + r.
            for c in 0..4 {
                for r in 0..4 {
                    t[4 * c + r] = sbox(s[4 * ((c + r) % 4) + r]);
                }
            }
            if round < 14 {
                for c in 0..4 {
                    let a = [t[4 * c], t[4 * c + 1], t[4 * c + 2], t[4 * c + 3]];
                    let all = a[0] ^ a[1] ^ a[2] ^ a[3];
                    for r in 0..4 {
                        t[4 * c + r] = a[r] ^ all ^ xtime(a[r] ^ a[(r + 1) % 4]);
                    }
                }
            }
            s = (u128::from_le_bytes(t) ^ k).to_le_bytes();
        }
        u128::from_le_bytes(s)
    }

    fn encrypt_blocks(rk: &[u128; 15], blocks: &mut [u128]) {
        for b in blocks {
            *b = encrypt_block(rk, *b);
        }
    }

    /// The carry-less product of `a` and `b`.
    fn clmul(a: u64, b: u64) -> u128 {
        let mut r = 0u128;
        for i in 0..64 {
            r ^= (u128::from(a) << i) & 0u128.wrapping_sub(u128::from((b >> i) & 1));
        }
        r
    }

    /// dot(a, b) = a·b·x^-128 modulo POLYVAL's polynomial.
    pub(super) fn dot(a: u128, b: u128) -> u128 {
        let (a0, a1, b0, b1) = (a as u64, (a >> 64) as u64, b as u64, (b >> 64) as u64);
        let lo = clmul(a0, b0);
        let hi = clmul(a1, b1);
        let mid = clmul(a0, b1) ^ clmul(a1, b0);
        let t0 = lo as u64;
        let t1 = (lo >> 64) as u64 ^ mid as u64;
        let t2 = hi as u64 ^ (mid >> 64) as u64;
        let t3 = (hi >> 64) as u64;
        let q = clmul(t0, super::POLY);
        let (u0, u1, u2) = (t1 ^ q as u64, t2 ^ (q >> 64) as u64 ^ t0, t3);
        let q = clmul(u0, super::POLY);
        let (v0, v1) = (u1 ^ q as u64, u2 ^ (q >> 64) as u64 ^ u0);
        u128::from(v0) | (u128::from(v1) << 64)
    }

    /// H: the portable copy hashes a block at a time.
    pub(super) struct Powers(pub(super) u128);

    pub(super) fn powers(h: u128, _n: usize) -> Powers {
        Powers(h)
    }

    pub(super) fn polyval(h: &Powers, mut acc: u128, bytes: &[u8]) -> u128 {
        for chunk in bytes.chunks(16) {
            let mut b = [0u8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            acc = dot(acc ^ u128::from_le_bytes(b), h.0);
        }
        acc
    }

    pub(super) fn ctr_xor(rk: &[u128; 15], first: u128, data: &mut [u8]) {
        let top = first & !0xFFFF_FFFFu128;
        let mut ctr = first as u32;
        for chunk in data.chunks_mut(16) {
            let ks = encrypt_block(rk, top | u128::from(ctr)).to_le_bytes();
            for (d, k) in chunk.iter_mut().zip(ks) {
                *d ^= k;
            }
            ctr = ctr.wrapping_add(1);
        }
    }

    construction!();
}

/// The copy by the CPU's instructions: every function compiled with them,
/// the entry points called only once they were detected.
#[cfg(target_arch = "aarch64")]
mod hard {
    use std::arch::aarch64::*;

    type V = uint8x16_t;

    #[target_feature(enable = "aes")]
    fn from(b: u128) -> V {
        vreinterpretq_u8_u64(vcombine_u64(
            vcreate_u64(b as u64),
            vcreate_u64((b >> 64) as u64),
        ))
    }

    #[target_feature(enable = "aes")]
    fn into(x: V) -> u128 {
        let q = vreinterpretq_u64_u8(x);
        u128::from(vgetq_lane_u64::<0>(q)) | (u128::from(vgetq_lane_u64::<1>(q)) << 64)
    }

    #[target_feature(enable = "aes")]
    fn load(b: &[u8; 16]) -> V {
        from(u128::from_le_bytes(*b))
    }

    #[target_feature(enable = "aes")]
    fn store(x: V, b: &mut [u8; 16]) {
        *b = into(x).to_le_bytes();
    }

    #[target_feature(enable = "aes")]
    fn xor(a: V, b: V) -> V {
        veorq_u8(a, b)
    }

    #[target_feature(enable = "aes")]
    fn zero() -> V {
        vdupq_n_u8(0)
    }

    #[target_feature(enable = "aes")]
    fn clmul_lo(a: V, b: V) -> V {
        let (a, b) = (vreinterpretq_p64_u8(a), vreinterpretq_p64_u8(b));
        vreinterpretq_u8_p128(vmull_p64(vgetq_lane_p64::<0>(a), vgetq_lane_p64::<0>(b)))
    }

    #[target_feature(enable = "aes")]
    fn clmul_hi(a: V, b: V) -> V {
        vreinterpretq_u8_p128(vmull_high_p64(
            vreinterpretq_p64_u8(a),
            vreinterpretq_p64_u8(b),
        ))
    }

    #[target_feature(enable = "aes")]
    fn swap(x: V) -> V {
        vextq_u8::<8>(x, x)
    }

    #[target_feature(enable = "aes")]
    fn shl64(x: V) -> V {
        vextq_u8::<8>(zero(), x)
    }

    #[target_feature(enable = "aes")]
    fn shr64(x: V) -> V {
        vextq_u8::<8>(x, zero())
    }

    #[target_feature(enable = "aes")]
    fn aes(k: &[V; 15], mut s: V) -> V {
        for key in &k[..13] {
            s = vaesmcq_u8(vaeseq_u8(s, *key));
        }
        veorq_u8(vaeseq_u8(s, k[13]), k[14])
    }

    #[target_feature(enable = "aes")]
    fn aes8(k: &[V; 15], s: &mut [V; 8]) {
        for key in &k[..13] {
            for x in s.iter_mut() {
                *x = vaesmcq_u8(vaeseq_u8(*x, *key));
            }
        }
        for x in s.iter_mut() {
            *x = veorq_u8(vaeseq_u8(*x, k[13]), k[14]);
        }
    }

    #[target_feature(enable = "aes")]
    fn next(x: V) -> V {
        let one = vsetq_lane_u32::<0>(1, vdupq_n_u32(0));
        vreinterpretq_u8_u32(vaddq_u32(vreinterpretq_u32_u8(x), one))
    }

    /// SubWord by AESE: four copies of the word, a zero round key — the
    /// rows' shifts move equal columns onto each other.
    #[target_feature(enable = "aes")]
    fn sub_word(w: u32) -> u32 {
        let x = vaeseq_u8(vreinterpretq_u8_u32(vdupq_n_u32(w)), vdupq_n_u8(0));
        vgetq_lane_u32::<0>(vreinterpretq_u32_u8(x))
    }

    vector!(#[target_feature(enable = "aes")]);
    construction!(#[target_feature(enable = "aes")]);
}

/// The copy by the CPU's instructions (x86-64).
#[cfg(target_arch = "x86_64")]
mod hard {
    use std::arch::x86_64::*;

    type V = __m128i;

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn from(b: u128) -> V {
        _mm_set_epi64x((b >> 64) as i64, b as i64)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn into(x: V) -> u128 {
        u128::from(_mm_extract_epi64::<0>(x) as u64)
            | (u128::from(_mm_extract_epi64::<1>(x) as u64) << 64)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn load(b: &[u8; 16]) -> V {
        from(u128::from_le_bytes(*b))
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn store(x: V, b: &mut [u8; 16]) {
        *b = into(x).to_le_bytes();
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn xor(a: V, b: V) -> V {
        _mm_xor_si128(a, b)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn zero() -> V {
        _mm_setzero_si128()
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn clmul_lo(a: V, b: V) -> V {
        _mm_clmulepi64_si128::<0x00>(a, b)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn clmul_hi(a: V, b: V) -> V {
        _mm_clmulepi64_si128::<0x11>(a, b)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn swap(x: V) -> V {
        _mm_shuffle_epi32::<0x4E>(x)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn shl64(x: V) -> V {
        _mm_slli_si128::<8>(x)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn shr64(x: V) -> V {
        _mm_srli_si128::<8>(x)
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn aes(k: &[V; 15], s: V) -> V {
        let mut s = _mm_xor_si128(s, k[0]);
        for key in &k[1..14] {
            s = _mm_aesenc_si128(s, *key);
        }
        _mm_aesenclast_si128(s, k[14])
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn aes8(k: &[V; 15], s: &mut [V; 8]) {
        for x in s.iter_mut() {
            *x = _mm_xor_si128(*x, k[0]);
        }
        for key in &k[1..14] {
            for x in s.iter_mut() {
                *x = _mm_aesenc_si128(*x, *key);
            }
        }
        for x in s.iter_mut() {
            *x = _mm_aesenclast_si128(*x, k[14]);
        }
    }

    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn next(x: V) -> V {
        _mm_add_epi32(x, _mm_set_epi32(0, 0, 0, 1))
    }

    /// SubWord by AESKEYGENASSIST: the word in lane 1, its SubWord in
    /// lane 0 of the result.
    #[target_feature(enable = "aes,pclmulqdq,sse4.1")]
    fn sub_word(w: u32) -> u32 {
        let x = _mm_aeskeygenassist_si128::<0>(_mm_set_epi32(0, 0, w as i32, 0));
        _mm_cvtsi128_si32(x) as u32
    }

    vector!(#[target_feature(enable = "aes,pclmulqdq,sse4.1")]);
    construction!(#[target_feature(enable = "aes,pclmulqdq,sse4.1")]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    /// Every copy this CPU can run: the portable one, and the
    /// instructions' where it has them.
    fn copies() -> Vec<bool> {
        if hardware() {
            vec![false, true]
        } else {
            vec![false]
        }
    }

    /// FIPS-197's AES-256 example (C.3).
    #[test]
    fn aes256_fips197() {
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let p = u128::from_le_bytes(hex("00112233445566778899aabbccddeeff").try_into().unwrap());
        let want = hex("8ea2b7ca516745bfeafc49904b496089");
        assert_eq!(
            soft::encrypt_block(&soft::expand(&key), p)
                .to_le_bytes()
                .to_vec(),
            want
        );
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        #[allow(unsafe_code)]
        if hardware() {
            // SAFETY: detected.
            let c = unsafe { hard::encrypt_block(&hard::expand(&key), p) };
            assert_eq!(c.to_le_bytes().to_vec(), want);
        }
    }

    /// POLYVAL's example (RFC 8452, appendix A), and the Montgomery
    /// product against its definition bit by bit.
    #[test]
    fn polyval_rfc8452_and_its_definition() {
        let h = u128::from_le_bytes(hex("25629347589242761d31f826ba4b757b").try_into().unwrap());
        let x = hex("4f4f95668c83dfb6401762bb2d01a262d1a24ddd2721d006bbe45f20d3c9f362");
        let want = hex("f7a3b47b846119fae5b7866cf5e5b77e");
        assert_eq!(
            soft::polyval(&soft::Powers(h), 0, &x)
                .to_le_bytes()
                .to_vec(),
            want
        );
        // a·b·x^-128: the product, then x^-1 a hundred and twenty-eight
        // times (add the modulus when odd, halve).
        let reference = |a: u128, b: u128| {
            let (mut lo, mut hi) = (0u128, 0u128);
            for i in 0..128 {
                if (b >> i) & 1 == 1 {
                    lo ^= a << i;
                    hi ^= if i == 0 { 0 } else { a >> (128 - i) };
                }
            }
            // The modulus: x^128 + x^127 + x^126 + x^121 + 1.
            let p_lo: u128 = 1 | (1 << 121) | (1 << 126) | (1 << 127);
            for _ in 0..128 {
                if lo & 1 == 1 {
                    lo ^= p_lo;
                    hi ^= 1;
                }
                lo = (lo >> 1) | (hi << 127);
                hi >>= 1;
            }
            lo
        };
        let mut a = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128;
        let mut b = !0u128;
        for _ in 0..50 {
            assert_eq!(soft::dot(a, b), reference(a, b));
            a = a
                .rotate_left(7)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835);
            b = b.rotate_left(13) ^ a;
        }
    }

    /// Known answers from OpenSSL's AES-256-GCM-SIV (the Python
    /// `cryptography` package): RFC 8452's own first vector, and data of
    /// several lengths (sample(n): byte i = i·31 + 7) with and without
    /// associated data — the whole ciphertext, or for the page's length
    /// its tag and last 16 bytes.
    #[test]
    fn known_answers() {
        let rfc_key: [u8; 32] =
            hex("0100000000000000000000000000000000000000000000000000000000000000")
                .try_into()
                .unwrap();
        let rfc_key = Key::new(&rfc_key);
        let rfc_nonce: [u8; 12] = hex("030000000000000000000000").try_into().unwrap();
        for hw in copies() {
            let t = seal_with(hw, &rfc_key, &rfc_nonce, b"", &mut []);
            assert_eq!(
                t.to_vec(),
                hex("07f5f4169bbf55a8400cd47ea6fd400f"),
                "hw {hw}"
            );
        }
        let key: [u8; 32] = (0..32u8).collect::<Vec<_>>().try_into().unwrap();
        let key = Key::new(&key);
        let nonce: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        for &(aad, n, want_tag, want_ct) in KATS {
            for hw in copies() {
                let mut d = sample(n);
                let t = seal_with(hw, &key, &nonce, aad.as_bytes(), &mut d);
                assert_eq!(t.to_vec(), hex(want_tag), "len {n} hw {hw}");
                let tail = &d[d.len().saturating_sub(16)..];
                let ct = if n > 64 { tail } else { &d[..] };
                assert_eq!(ct.to_vec(), hex(want_ct), "len {n} hw {hw}");
                assert!(open_with(hw, &key, &nonce, aad.as_bytes(), &mut d, &t));
                assert_eq!(d, sample(n));
            }
        }
    }

    /// The instructions and the portable copy agree on every length up to
    /// a few blocks past a page, and refuse every changed bit.
    #[test]
    fn the_copies_agree_and_refuse_changes() {
        let key = Key::new(&[0x5Au8; 32]);
        let nonce = [0x11u8; 12];
        for n in (0..80).chain([127, 128, 129, 143, 144, 255, 256, 257, 1000, 8168, 8191]) {
            let mut ref_data = sample(n);
            let ref_tag = seal_with(false, &key, &nonce, b"page", &mut ref_data);
            for hw in copies() {
                let mut d = sample(n);
                assert_eq!(
                    seal_with(hw, &key, &nonce, b"page", &mut d),
                    ref_tag,
                    "len {n}"
                );
                assert_eq!(d, ref_data, "len {n}");
            }
        }
        let mut d = sample(100);
        let t = seal_in_place(&key, &nonce, b"aad", &mut d);
        for i in 0..d.len() * 8 {
            let mut c = d.clone();
            c[i / 8] ^= 1 << (i % 8);
            assert!(!open_in_place(&key, &nonce, b"aad", &mut c, &t));
            assert!(c.iter().all(|&b| b == 0), "released when refused");
        }
        for i in 0..128 {
            let mut c = d.clone();
            let mut bad = t;
            bad[i / 8] ^= 1 << (i % 8);
            assert!(!open_in_place(&key, &nonce, b"aad", &mut c, &bad));
        }
        let mut c = d.clone();
        assert!(!open_in_place(&key, &[0x12; 12], b"aad", &mut c, &t));
        let mut c = d.clone();
        assert!(!open_in_place(&key, &nonce, b"aae", &mut c, &t));
    }

    /// The counter's low 32 bits wrap and the rest stays, alike in both
    /// copies; and the key's round keys are the same either way.
    #[test]
    fn the_counter_wraps_alike() {
        let rk = soft::expand(&[9; 32]);
        assert_eq!(Key::new(&[9; 32]).rk, rk);
        let first = (0xABCDu128 << 100) | (1 << 127) | 0xFFFF_FFFA;
        let mut a = sample(300);
        soft::ctr_xor(&rk, first, &mut a);
        // Block 6 is under counter 0 with the same top bits.
        let mut b6 = [0u8; 16];
        soft::ctr_xor(&rk, first & !0xFFFF_FFFF, &mut b6);
        let plain6: Vec<u8> = sample(300)[96..112].to_vec();
        let ks6: Vec<u8> = a[96..112].iter().zip(&plain6).map(|(c, p)| c ^ p).collect();
        assert_eq!(ks6, b6.to_vec());
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        #[allow(unsafe_code)]
        if hardware() {
            let mut h = sample(300);
            // SAFETY: detected.
            unsafe { hard::ctr_xor(&rk, first, &mut h) };
            assert_eq!(h, a);
        }
    }

    /// (aad, length, tag, ciphertext — whole, or its last 16 bytes past
    /// 64), from `crates/kernels/tests/aes_gcm_siv_reference.py`.
    #[rustfmt::skip]
    const KATS: &[(&str, usize, &str, &str)] = &[
        ("", 0, "8da5f9c8d3b8380b12de3def876cec3f", ""),
        ("", 1, "63c0c72073641a1c2da9553fc87f127d", "5c"),
        ("", 15, "bb078d928ffd79c03200ce00c75b2614", "edfd357fe868ea15dbcf584703c097"),
        ("", 16, "9b84a41864bcb63483cceb65a8af316b", "2887ce8d0097fbf8fed484258269495f"),
        ("", 17, "f8eb23ac68c5d592b0c361b1eb5c0269", "1f443d760bdcf84d779713d684e79d8be8"),
        ("", 33, "34eddaf2706d450196f169d1869260fd", "1274351793f265026ed27e8e7c4e755617bb6b4491744465e5b99b326cbae5f0ee"),
        ("", 64, "50d40e6cd582e500c92d076e9bcc8fe6", "4af1b6ebc3fc5221a6016ada2eb553163b27134f9dd2e11aff2ca172e991bc77ad3a88790334f88bc4826533903be3a7b3e7e70cea0ee8deaa840d8de7f7d9b4"),
        ("", 100, "2426b72293a3907e0255b07e33499c14", "64abf2a51ee1737f74820cffe299f45a"),
        ("", 8168, "3732ba2656c16424bd9c6eff34cac01d", "4dbff040b4a374333f96a768587a210a"),
        ("page 5", 0, "2b730d04ba9510e9f3c380723c89a94c", ""),
        ("page 5", 1, "b2f5e41add348341b83b4fad15695efb", "57"),
        ("page 5", 15, "f779dd9ac6bf9404406a3b44e13823af", "e85b994c760f83f0f3377123ab019b"),
        ("page 5", 16, "69daf4fadf45df4e0329eb39b67ea2e7", "9794ff671a3a380c99e49cc5ed0952ff"),
        ("page 5", 17, "0faffbd0c788eb04dc1d4b2a3e848a60", "62baecfcb8dcea1909ab3b6358c95ced19"),
        ("page 5", 33, "f3d325f86c9e34d5e87244cf42df0349", "39fa2933644092c7ea4ffd89afb98c2e66e40110916a8be845342b097fb3e0847a"),
        ("page 5", 64, "bcc948d15ff69acf838a004cbdc5dad9", "4a79c98cb4af7321b9f405c55e6655ec42ddd7e5a3564c7266f99bbbcac63a18b68c1070bd4469944ededf10cc3c7afd9113217f4d8f35e8713795c42059d7b9"),
        ("page 5", 100, "fb60c9a07cab159834ad68fa7b9c247f", "03e115cf575264dcda9996e932f1e077"),
        ("page 5", 8168, "0900292c46929324b847d434c7d83016", "96d7d236d16a9d3eba7320188a7ca4c2"),
    ];
}
