//! A deterministic, nonce-misuse-resistant AEAD (PLAN.md §M14.30 K2):
//! the structure of AES-GCM-SIV (RFC 8452) with ChaCha20 in place of AES,
//! for what is sealed where a nonce cannot be kept unique — a page of the
//! data file written again under the same (id, birth), a WAL record
//! written again at the same LSN after a crash.
//!
//! For a key `K`, a nonce `N`, associated data `A` and a plaintext `P`:
//!
//! 1. Keys for this nonce, from ChaCha20's block function:
//!    `block(K, 0, N)` gives the encryption key (bytes 0..32) and the
//!    hash key `r` (bytes 32..48); `block(K, 1, N)` gives the PRF key
//!    (bytes 0..32).
//! 2. `S` = Poly1305's polynomial under `r` alone (no `s` added) over `A`
//!    and `P`, each zero-padded to 16 bytes, then their lengths in bits
//!    as two little-endian 64-bit words — the universal hash (POLYVAL's
//!    place in GCM-SIV).
//! 3. The tag `T` = the first 16 bytes of `block(prf key, le32(S[12..16]),
//!    S[0..12])` — the hash through a PRF, the synthetic IV.
//! 4. The ciphertext = `P` XOR ChaCha20 under the encryption key, the
//!    nonce `T[0..12]` and the first block counter `le32(T[12..16])`.
//!
//! Opening decrypts with `T`, computes `T` again from the plaintext and
//! compares in constant time; nothing is released when they differ.
//!
//! The same `(K, N, A, P)` always give the same bytes — the only thing a
//! repeat shows is that the same was sealed twice.  Two different inputs
//! share a key stream only if their tags' 128 bits collide (the birthday
//! bound: ~2^-64 after 2^32 seals under one nonce, far less across
//! nonces, since each nonce has keys of its own).  `S` is a polynomial
//! over GF(2^130 - 5) in a secret point: two inputs of `L` 16-byte blocks
//! collide with probability at most `L / 2^102` (Poly1305's bound,
//! Bernstein 2005), and the PRF hides `S` itself.

use crate::aead::{KEY_LEN, NONCE_LEN, Poly1305, block, chacha20};

/// The tag's length: the synthetic IV.
pub const TAG_LEN: usize = 16;

struct Keys {
    enc: [u8; KEY_LEN],
    hash: [u8; 32],
    prf: [u8; KEY_LEN],
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.enc.fill(0);
        self.hash.fill(0);
        self.prf.fill(0);
        std::hint::black_box(&self.enc);
        std::hint::black_box(&self.hash);
        std::hint::black_box(&self.prf);
    }
}

fn keys(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN]) -> Keys {
    let mut b0 = block(key, 0, nonce);
    let mut b1 = block(key, 1, nonce);
    let mut k = Keys {
        enc: [0; KEY_LEN],
        hash: [0; 32],
        prf: [0; KEY_LEN],
    };
    k.enc.copy_from_slice(&b0[..32]);
    // `r` only: the second half — Poly1305's `s` — stays zero.
    k.hash[..16].copy_from_slice(&b0[32..48]);
    k.prf.copy_from_slice(&b1[..32]);
    b0.fill(0);
    b1.fill(0);
    std::hint::black_box(&b0);
    std::hint::black_box(&b1);
    k
}

fn tag(k: &Keys, aad: &[u8], plaintext: &[u8]) -> [u8; TAG_LEN] {
    let mut p = Poly1305::new(&k.hash);
    p.update_padded(aad);
    p.update_padded(plaintext);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&((aad.len() as u64) * 8).to_le_bytes());
    lens[8..].copy_from_slice(&((plaintext.len() as u64) * 8).to_le_bytes());
    p.block(&lens, true);
    let s = p.finish();
    let nonce: [u8; NONCE_LEN] = s[..12].try_into().expect("12 bytes");
    let counter = u32::from_le_bytes(s[12..16].try_into().expect("4 bytes"));
    let b = block(&k.prf, counter, &nonce);
    b[..TAG_LEN].try_into().expect("16 bytes")
}

fn crypt(k: &Keys, t: &[u8; TAG_LEN], data: &mut [u8]) {
    let nonce: [u8; NONCE_LEN] = t[..12].try_into().expect("12 bytes");
    let counter = u32::from_le_bytes(t[12..16].try_into().expect("4 bytes"));
    chacha20(&k.enc, counter, &nonce, data);
}

/// The keys derived from a key and one nonce (step 1), derived once for
/// many messages sealed under that nonce — each told apart by its
/// associated data (a sealed file's chunks, PLAN.md §M14.28 issue #3).
/// Sealing under it gives the bytes [`seal_in_place`] gives under the key
/// and the nonce.  Zeroed when dropped.
pub struct Derived {
    k: Keys,
}

impl Derived {
    /// The keys of `key` for `nonce`.
    pub fn new(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN]) -> Self {
        Self {
            k: keys(key, nonce),
        }
    }

    /// Seals `data` in place under the nonce: its tag.
    pub fn seal_in_place(&self, aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        let t = tag(&self.k, aad, data);
        crypt(&self.k, &t, data);
        t
    }

    /// Opens in place what [`seal_in_place`](Self::seal_in_place) sealed:
    /// `false`, `data` zeroed, if it is not what was sealed.
    pub fn open_in_place(&self, aad: &[u8], data: &mut [u8], t: &[u8; TAG_LEN]) -> bool {
        crypt(&self.k, t, data);
        if crate::constant_time_eq(&tag(&self.k, aad, data), t) {
            return true;
        }
        data.fill(0);
        false
    }
}

impl std::fmt::Debug for Derived {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("siv::Derived(..)")
    }
}

/// Seals `data` in place under `key` and `nonce`, `aad` authenticated
/// beside it: `data` becomes the ciphertext; the tag is returned.
pub fn seal_in_place(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
) -> [u8; TAG_LEN] {
    Derived::new(key, nonce).seal_in_place(aad, data)
}

/// Opens in place what [`seal_in_place`] sealed: `true` with `data` the
/// plaintext; `false` — the key, the nonce, the `aad`, the tag or a byte
/// of `data` not what was sealed — with `data` zeroed.
pub fn open_in_place(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    data: &mut [u8],
    t: &[u8; TAG_LEN],
) -> bool {
    Derived::new(key, nonce).open_in_place(aad, data, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        crate::keys::from_hex(s).expect("hex")
    }

    fn sample(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    #[test]
    fn round_trip_and_deterministic() {
        let key = [7u8; 32];
        let nonce = [9u8; 12];
        for n in [0, 1, 15, 16, 17, 63, 64, 65, 8168] {
            let p = sample(n);
            let mut a = p.clone();
            let ta = seal_in_place(&key, &nonce, b"page 5", &mut a);
            let mut b = p.clone();
            let tb = seal_in_place(&key, &nonce, b"page 5", &mut b);
            assert_eq!((&a, ta), (&b, tb), "deterministic");
            if n > 0 {
                assert_ne!(a, p, "encrypted");
            }
            assert!(open_in_place(&key, &nonce, b"page 5", &mut a, &ta));
            assert_eq!(a, p);
        }
    }

    #[test]
    fn every_change_is_refused() {
        let key = [3u8; 32];
        let nonce = [4u8; 12];
        let aad = b"page 9 birth 4".to_vec();
        let p = sample(100);
        let mut c = p.clone();
        let t = seal_in_place(&key, &nonce, &aad, &mut c);
        let opens = |key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], c: &[u8], t: &[u8; 16]| {
            let mut d = c.to_vec();
            let ok = open_in_place(key, nonce, aad, &mut d, t);
            assert!(ok || d.iter().all(|&b| b == 0), "released on failure");
            ok
        };
        assert!(opens(&key, &nonce, &aad, &c, &t));
        for i in 0..c.len() * 8 {
            let mut d = c.clone();
            d[i / 8] ^= 1 << (i % 8);
            assert!(!opens(&key, &nonce, &aad, &d, &t), "ciphertext bit {i}");
        }
        for i in 0..TAG_LEN * 8 {
            let mut u = t;
            u[i / 8] ^= 1 << (i % 8);
            assert!(!opens(&key, &nonce, &aad, &c, &u), "tag bit {i}");
        }
        for i in 0..aad.len() * 8 {
            let mut a = aad.clone();
            a[i / 8] ^= 1 << (i % 8);
            assert!(!opens(&key, &nonce, &a, &c, &t), "aad bit {i}");
        }
        let mut k2 = key;
        k2[31] ^= 1;
        assert!(!opens(&k2, &nonce, &aad, &c, &t));
        let mut n2 = nonce;
        n2[0] ^= 1;
        assert!(!opens(&key, &n2, &aad, &c, &t));
        // Data moved between the AAD and the plaintext: the lengths differ.
        assert!(!opens(&key, &nonce, &aad[..aad.len() - 1], &c, &t));
    }

    #[test]
    fn different_inputs_never_share_a_key_stream() {
        // Two plaintexts differing in one byte under the same (key, nonce,
        // aad): different tags, so their ciphertexts XOR to more than the
        // plaintexts' difference — no key stream reused.
        let key = [1u8; 32];
        let nonce = [2u8; 12];
        let p = sample(256);
        let mut q = p.clone();
        q[200] ^= 0x55;
        let (mut a, mut b) = (p.clone(), q.clone());
        let ta = seal_in_place(&key, &nonce, b"", &mut a);
        let tb = seal_in_place(&key, &nonce, b"", &mut b);
        assert_ne!(ta, tb);
        let same = a
            .iter()
            .zip(&b)
            .zip(p.iter().zip(&q))
            .filter(|((x, y), (u, v))| (*x ^ *y) == (*u ^ *v))
            .count();
        assert!(same < 16, "{same} of 256 bytes as with one key stream");
    }

    /// Known answers, computed by an implementation written apart (Python,
    /// from RFC 8439's definitions, in `crates/crypto/tests/siv_reference.py`).
    #[test]
    fn known_answers() {
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let nonce: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        for (aad, plain, want_tag, want_ct) in KAT {
            let mut d = hex(plain);
            let t = seal_in_place(&key, &nonce, &hex(aad), &mut d);
            assert_eq!(crate::keys::to_hex(&t), *want_tag, "tag of {plain}");
            assert_eq!(crate::keys::to_hex(&d), *want_ct, "ciphertext of {plain}");
        }
    }

    const KAT: &[(&str, &str, &str, &str)] = &[
        ("", "", "0b80f79098bf8688025c817a52bb5ee7", ""),
        ("", "07", "f8bfb4344ba9bb5ca01f1dddc0e37b15", "2e"),
        (
            "706167652035",
            "0726456483a2c1e0ff1e3d5c7b9ab9d8",
            "21fa3bb286cc0798f92b89bebc077b4f",
            "425c5f99649577b92c69c0217d809bf9",
        ),
        (
            "05000000000000000400000000000010",
            "0726456483a2c1e0ff1e3d5c7b9ab9d8f71635547392b1d0ef0e2d4c6b8aa9c8e70625446382a1c0dffe1d3c5b7a99b8d7f61534537291b0cfee0d2c4b6a89a8c7e60524436281a0bfdefd1c3b5a7998b7d6f51433527190afceed0c2b4a6988a7c6e504",
            "9fb807ae46b8d566727ef05e9a9cbb78",
            "8cbe2785ac54daa3f15819653c409d5fcc2a722718b4a2205abfd993b52f0fb9b6976b1aeb24bde56315e874556bcdedcad2052192500f763b413c1c1bcbfa33f99626e1000514e84815cf3ea95fe77cd3f8699dd9743bb0413c01cab8b0df362b7a5311",
        ),
        (
            "6e75636c657573",
            "0726456483a2c1e0ff1e3d5c7b9ab9d8f71635547392b1d0ef0e2d4c6b8aa9c8e70625446382a1c0dffe1d3c5b7a99b8d7f61534537291b0cfee0d2c4b6a89a8c7e60524436281a0bfdefd1c3b5a7998b7d6f51433527190afceed0c2b4a6988a7c6e504234261809fbeddfc1b3a597897b6d5f4133251708faecdec0b2a496887a6",
            "e944cc6edf1bd2103a2750f5c916b08d",
            "d49e15552d107865f3ece5222765c2b94c1ddfba4a376fb884e543b02584726538b18ee2daaca32ea11dfa36cb1e7f2db002256d66baadb5881bbd4384083c2c102dddc1d0362ca5caa9a53d91bddff54dd9725b0e4b1ab2bfbb1527cf421aa74886c1e93259d09b7001e532fb6222abefe0b320054d41f9c59c7c2b4b82ba69eae8",
        ),
    ];
}
