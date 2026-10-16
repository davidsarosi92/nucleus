use std::sync::atomic::{AtomicU64, Ordering};

use crate::Rng;

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64 (Steele, Lea & Flood, 2014): a small, fast generator whose
/// whole state is one `u64`, so a seed fully determines its output.
///
/// Lock-free and shareable.  The sequence is deterministic as long as one
/// thread draws from it; with several threads each value is still drawn
/// exactly once, but which thread gets which depends on scheduling.
/// **Not cryptographic.**
#[derive(Debug)]
pub struct SplitMix64 {
    state: AtomicU64,
}

impl SplitMix64 {
    /// A generator starting from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }
}

impl Rng for SplitMix64 {
    fn next_u64(&self) -> u64 {
        let s = self
            .state
            .fetch_add(GOLDEN_GAMMA, Ordering::Relaxed)
            .wrapping_add(GOLDEN_GAMMA);
        mix(s)
    }
}

/// The SplitMix64 output function: a bijective scramble of 64 bits.  Also
/// used to derive independent seeds from one seed.
pub(crate) fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let a = SplitMix64::new(42);
        let b = SplitMix64::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn reference_values() {
        // Seed 0, from the reference implementation (Vigna, splitmix64.c).
        let r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn below_stays_in_range() {
        let r = SplitMix64::new(7);
        for bound in [1, 2, 3, 10, 1 << 40, u64::MAX] {
            for _ in 0..100 {
                assert!(r.below(bound) < bound);
            }
        }
    }
}
