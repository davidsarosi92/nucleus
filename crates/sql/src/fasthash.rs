//! The hash of the executor's own tables — hash joins' and aggregates'
//! (PLAN.md §M14.20 V10): a multiply-rotate hash over 64-bit words, much
//! cheaper than the standard library's SipHash for the short keys these
//! tables hold.  Seeded once per process from the standard library's
//! random state, so the buckets a set of keys falls into differ between
//! processes; it is not meant to resist chosen collisions — a statement
//! that hashes keys of its own choosing slows only itself.

use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

const K: u64 = 0xF135_7AEA_2E62_A9C5;

/// The hasher (see the module).
#[derive(Clone, Copy)]
pub struct FastHasher(u64);

impl FastHasher {
    #[inline]
    fn add(&mut self, x: u64) {
        self.0 = (self.0 ^ x).wrapping_mul(K).rotate_left(26);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 56));
        }
    }

    #[inline]
    fn write_u8(&mut self, x: u8) {
        self.add(u64::from(x));
    }

    #[inline]
    fn write_u32(&mut self, x: u32) {
        self.add(u64::from(x));
    }

    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.add(x);
    }

    #[inline]
    fn write_i64(&mut self, x: i64) {
        self.add(x as u64);
    }

    #[inline]
    fn write_usize(&mut self, x: usize) {
        self.add(x as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // The high bits mixed into the low ones, which pick the bucket.
        let h = self.0.wrapping_mul(K);
        h ^ (h >> 32)
    }
}

/// Builds [`FastHasher`]s from the process's seed.
#[derive(Clone, Copy)]
pub struct FastBuild(u64);

impl Default for FastBuild {
    fn default() -> Self {
        static SEED: OnceLock<u64> = OnceLock::new();
        FastBuild(*SEED.get_or_init(|| {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        }))
    }
}

impl BuildHasher for FastBuild {
    type Hasher = FastHasher;

    #[inline]
    fn build_hasher(&self) -> FastHasher {
        FastHasher(self.0)
    }
}

/// A hash map over [`FastBuild`].
pub type FastMap<K, V> = std::collections::HashMap<K, V, FastBuild>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_sequential_and_equal_keys_agree() {
        let b = FastBuild::default();
        let h = |x: i64| b.hash_one(x);
        assert_eq!(h(42), h(42));
        // Sequential integers fill buckets evenly (low bits pick them).
        let mut buckets = [0u32; 64];
        for i in 0..64_000i64 {
            buckets[(h(i) & 63) as usize] += 1;
        }
        assert!(
            buckets.iter().all(|&n| (700..1300).contains(&n)),
            "{buckets:?}"
        );
        let s = |x: &str| b.hash_one(x);
        assert_eq!(s("abc"), s("abc"));
        assert_ne!(s("abc"), s("abd"));
        assert_ne!(s("abcdefgh"), s("abcdefgh\0"));
    }
}
