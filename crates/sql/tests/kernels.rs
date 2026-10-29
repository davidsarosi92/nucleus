//! Every batch kernel (PLAN.md §M14.20 V3), at every CPU level this
//! machine has, against its scalar reference — on random columns with the
//! edge values mixed in (the extremes of the integers; `NaN`, `-0`, the
//! infinities; empty and repeated strings) and every length up to past a
//! vector register's — and the references against the SQL layer's own
//! definitions (the order of doubles, `LIKE`).

use nucleus_platform::{Rng, SplitMix64};
use nucleus_sql::kernels::{self, Anchor, Arith, Cmp, FArith, Level, Rhs, reference};

const CMPS: [Cmp; 6] = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];

fn ints(rng: &SplitMix64, n: usize) -> Vec<i64> {
    let edges = [
        0,
        1,
        -1,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        i64::MIN + 1,
        1 << 32,
        -(1 << 32),
    ];
    (0..n)
        .map(|_| match rng.below(4) {
            0 => edges[rng.below(edges.len() as u64) as usize],
            1 => rng.below(7) as i64 - 3,
            _ => rng.next_u64() as i64,
        })
        .collect()
}

fn floats(rng: &SplitMix64, n: usize) -> Vec<f64> {
    let edges = [
        0.0,
        -0.0,
        1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        f64::MIN_POSITIVE,
    ];
    (0..n)
        .map(|_| match rng.below(3) {
            0 => edges[rng.below(edges.len() as u64) as usize],
            1 => (rng.below(5) as f64 - 2.0) / 2.0,
            _ => f64::from_bits(rng.next_u64()),
        })
        .collect()
}

fn nulls(rng: &SplitMix64, n: usize) -> Option<Vec<bool>> {
    (rng.below(2) == 0).then(|| (0..n).map(|_| rng.below(3) == 0).collect())
}

fn strings(rng: &SplitMix64, n: usize) -> (Vec<u32>, Vec<u8>) {
    let words = [
        "", "a", "ab", "abc", "abcabc", "ba", "xabcx", "héllo", "llo",
    ];
    let mut offsets = vec![0u32];
    let mut bytes = Vec::new();
    for _ in 0..n {
        bytes.extend_from_slice(words[rng.below(words.len() as u64) as usize].as_bytes());
        offsets.push(bytes.len() as u32);
    }
    (offsets, bytes)
}

/// Floats compared bit for bit (NaN's payload aside).
fn same_f64(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits())
}

fn lengths() -> impl Iterator<Item = usize> {
    (0..70).chain([127, 128, 129, 1000, 1024])
}

#[test]
fn the_cpu_levels_are_detected() {
    let levels = kernels::levels();
    assert_eq!(levels[0], Level::Base);
    assert_eq!(kernels::level(), *levels.last().unwrap());
    eprintln!(
        "kernel levels: {:?}",
        levels
            .iter()
            .map(|&l| kernels::level_name(l))
            .collect::<Vec<_>>()
    );
}

#[test]
fn integer_kernels_match_their_reference() {
    let rng = SplitMix64::new(1);
    for level in kernels::levels() {
        for n in lengths() {
            let (a, b) = (ints(&rng, n), ints(&rng, n));
            let c = ints(&rng, 1)[0];
            for rhs in [Rhs::Col(&b[..]), Rhs::Const(c)] {
                for op in CMPS {
                    let mut out = vec![false; n];
                    kernels::cmp_i64_at(level, op, &a, rhs, &mut out);
                    assert_eq!(out, reference::cmp_i64(op, &a, rhs), "{level:?} {op:?} {n}");
                }
                for op in [Arith::Add, Arith::Sub, Arith::Mul] {
                    let (mut out, mut over) = (vec![0; n], vec![false; n]);
                    kernels::arith_i64_at(level, op, &a, rhs, &mut out, &mut over);
                    assert_eq!(
                        (out, over),
                        reference::arith_i64(op, &a, rhs),
                        "{level:?} {op:?}"
                    );
                }
            }
            let small: Vec<i64> = a.iter().map(|x| x >> 3).collect();
            for data in [&a, &small] {
                let nulls = nulls(&rng, n);
                let nulls = nulls.as_deref();
                assert_eq!(
                    kernels::sum_i64_at(level, data, nulls),
                    reference::sum_i64(data, nulls),
                    "{level:?} sum {n}"
                );
                for max in [false, true] {
                    assert_eq!(
                        kernels::minmax_i64_at(level, data, nulls, max),
                        reference::minmax_i64(data, nulls, max),
                        "{level:?} max={max} {n}"
                    );
                }
            }
            let mask: Vec<bool> = (0..n).map(|_| rng.below(2) == 0).collect();
            let mut rows = vec![7];
            kernels::select_at(level, &mask, &mut rows);
            assert_eq!(rows, reference::select(&mask));
            let (mut h1, mut h2) = (vec![0; n], vec![0; n]);
            kernels::hash_i64_at(level, &a, &mut h1);
            kernels::hash_i64_at(Level::Base, &a, &mut h2);
            assert_eq!(h1, h2);
        }
    }
}

#[test]
fn double_kernels_match_their_reference() {
    let rng = SplitMix64::new(2);
    for level in kernels::levels() {
        for n in lengths() {
            let (a, b) = (floats(&rng, n), floats(&rng, n));
            let c = floats(&rng, 1)[0];
            for rhs in [Rhs::Col(&b[..]), Rhs::Const(c)] {
                for op in CMPS {
                    let mut out = vec![false; n];
                    kernels::cmp_f64_at(level, op, &a, rhs, &mut out);
                    assert_eq!(out, reference::cmp_f64(op, &a, rhs), "{level:?} {op:?} {n}");
                }
                for op in [FArith::Add, FArith::Sub, FArith::Mul, FArith::Div] {
                    let mut out = vec![0.0; n];
                    kernels::arith_f64_at(level, op, &a, rhs, &mut out);
                    assert!(
                        same_f64(&out, &reference::arith_f64(op, &a, rhs)),
                        "{level:?} {op:?} {n}"
                    );
                }
            }
        }
    }
}

#[test]
fn string_kernels_match_their_reference() {
    let rng = SplitMix64::new(3);
    for level in kernels::levels() {
        for n in lengths() {
            let (offsets, bytes) = strings(&rng, n);
            for needle in ["", "a", "abc", "llo", "héllo", "zz"] {
                let mut out = vec![false; n];
                kernels::eq_bytes_at(level, &offsets, &bytes, needle.as_bytes(), &mut out);
                assert_eq!(
                    out,
                    reference::eq_bytes(&offsets, &bytes, needle.as_bytes())
                );
                for anchor in [Anchor::Prefix, Anchor::Suffix, Anchor::Contains] {
                    kernels::like_bytes_at(
                        level,
                        &offsets,
                        &bytes,
                        needle.as_bytes(),
                        anchor,
                        &mut out,
                    );
                    assert_eq!(
                        out,
                        reference::like_bytes(&offsets, &bytes, needle.as_bytes(), anchor),
                        "{level:?} {anchor:?} {needle:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_references_are_sql_s_definitions() {
    let rng = SplitMix64::new(4);
    let (a, b) = (floats(&rng, 500), floats(&rng, 500));
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(
            reference::total_cmp(*x, *y),
            nucleus_sql::types::total_cmp(*x, *y)
        );
    }
    let (offsets, bytes) = strings(&rng, 300);
    for needle in ["", "a", "abc", "llo", "héllo"] {
        for (anchor, pattern) in [
            (Anchor::Prefix, format!("{needle}%")),
            (Anchor::Suffix, format!("%{needle}")),
            (Anchor::Contains, format!("%{needle}%")),
        ] {
            let want: Vec<bool> = offsets
                .windows(2)
                .map(|w| {
                    let s = std::str::from_utf8(&bytes[w[0] as usize..w[1] as usize]).unwrap();
                    nucleus_sql::expr::like(s, &pattern)
                })
                .collect();
            assert_eq!(
                reference::like_bytes(&offsets, &bytes, needle.as_bytes(), anchor),
                want
            );
        }
    }
}
