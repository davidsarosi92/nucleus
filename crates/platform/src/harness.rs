//! Running a simulation over many seeds, and reproducing a failure from one.
//!
//! ```
//! use nucleus_platform::harness;
//! use nucleus_platform::sim::SimPlatform;
//!
//! harness::run("example", 16, |seed| {
//!     let p = SimPlatform::new(seed);
//!     // ... drive `p`, assert invariants ...
//! #   let _ = p;
//! });
//! ```
//!
//! When a case fails, the seed is printed with the command that reruns only
//! that case.  Environment variables:
//!
//! - `NUCLEUS_SEED=<n>` (decimal or `0x…` hex) — run only that seed.
//! - `NUCLEUS_SIM_CASES=<n>` — run `n` seeds instead of the test's default.
//!
//! The seeds a test runs are fixed (derived from its name), so `cargo test`
//! is reproducible; raise `NUCLEUS_SIM_CASES` to search further.

use std::panic::{self, AssertUnwindSafe};

use crate::rng::mix;

/// Environment variable that selects a single seed.
pub const SEED_ENV: &str = "NUCLEUS_SEED";
/// Environment variable that overrides the number of seeds.
pub const CASES_ENV: &str = "NUCLEUS_SIM_CASES";

/// Runs `case` once per seed; see the module documentation.
pub fn run(name: &str, default_cases: u64, mut case: impl FnMut(u64)) {
    for seed in seeds(name, default_cases) {
        if let Err(panic) = panic::catch_unwind(AssertUnwindSafe(|| case(seed))) {
            eprintln!(
                "\nsimulation `{name}` failed with seed {seed:#x}\n\
                 reproduce with: {SEED_ENV}={seed:#x} cargo test {name}\n"
            );
            panic::resume_unwind(panic);
        }
    }
}

/// The seeds [`run`] would use.
pub fn seeds(name: &str, default_cases: u64) -> Vec<u64> {
    if let Ok(s) = std::env::var(SEED_ENV) {
        return vec![parse_u64(&s).unwrap_or_else(|| panic!("{SEED_ENV}={s:?} is not a number"))];
    }
    let cases = std::env::var(CASES_ENV)
        .ok()
        .map(|s| parse_u64(&s).unwrap_or_else(|| panic!("{CASES_ENV}={s:?} is not a number")))
        .unwrap_or(default_cases);
    let base = fnv1a(name);
    (0..cases).map(|i| mix(base.wrapping_add(i))).collect()
}

fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xCBF2_9CE4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_and_hex() {
        assert_eq!(parse_u64("42"), Some(42));
        assert_eq!(parse_u64("0x2a"), Some(42));
        assert_eq!(parse_u64("zz"), None);
    }

    #[test]
    fn seeds_are_stable_and_distinct_per_name() {
        // Not run under NUCLEUS_SEED, which would collapse both lists.
        if std::env::var(SEED_ENV).is_ok() {
            return;
        }
        let a = seeds("a", 4);
        assert_eq!(a, seeds("a", 4));
        assert_ne!(a, seeds("b", 4));
    }
}
