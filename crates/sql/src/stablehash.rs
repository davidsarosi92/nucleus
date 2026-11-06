//! A hash that is the same everywhere (PLAN.md §M14.27 S6): xxHash64
//! (Yann Collet's, seed 0) of a value's key encoding.  Sketches keep what
//! it gives — a `hll` value written today is merged with one written by
//! another version, on another machine — so it is a format: the
//! algorithm, its seed and the encoding it hashes do not change without a
//! new version of what stores it.  The executor's own tables use
//! [`crate::fasthash`], which is seeded per process and cannot serve.
//!
//! Hashing the key encoding makes equal values hash alike whatever their
//! form: `1.0` and `1.00`, `-0` and `0`, a `char(n)`'s trailing blanks,
//! an `int` and a `bigint`.  Integers and doubles — what a sketch mostly
//! takes — are hashed from their encoding built on the stack.

use crate::types::Value;

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

#[inline]
fn merge_round(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

#[inline]
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// xxHash64 of `bytes` with `seed`.
pub fn xxh64(bytes: &[u8], seed: u64) -> u64 {
    let len = bytes.len();
    let mut i = 0;
    let mut h = if len >= 32 {
        let mut v = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while i + 32 <= len {
            for (k, acc) in v.iter_mut().enumerate() {
                *acc = round(*acc, u64_at(bytes, i + 8 * k));
            }
            i += 32;
        }
        let mut h = v[0]
            .rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18));
        for x in v {
            h = merge_round(h, x);
        }
        h
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len as u64);
    while i + 8 <= len {
        h ^= round(0, u64_at(bytes, i));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        i += 8;
    }
    if i + 4 <= len {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        h ^= u64::from(w).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        i += 4;
    }
    for &b in &bytes[i..] {
        h ^= u64::from(b).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// The key encoding's tag and bytes of an integer.
#[inline]
pub fn hash_int(i: i64) -> u64 {
    let mut b = [0u8; 9];
    b[0] = 0x02;
    b[1..].copy_from_slice(&((i ^ i64::MIN) as u64).to_be_bytes());
    xxh64(&b, 0)
}

/// The key encoding's tag and bytes of a double (`-0` as `0`, one NaN).
#[inline]
pub fn hash_float(x: f64) -> u64 {
    let bits = if x.is_nan() {
        f64::NAN.to_bits()
    } else if x == 0.0 {
        0
    } else {
        x.to_bits()
    };
    let key = if bits >> 63 == 1 {
        !bits
    } else {
        bits | 1 << 63
    };
    let mut b = [0u8; 9];
    b[0] = 0x03;
    b[1..].copy_from_slice(&key.to_be_bytes());
    xxh64(&b, 0)
}

/// A non-`NULL` value's stable hash.
pub fn hash_value(v: &Value) -> u64 {
    match v {
        Value::Int(i) => hash_int(*i),
        Value::Float(x) => hash_float(*x),
        Value::Real(x) => hash_float(f64::from(*x)),
        _ => {
            let mut b = Vec::with_capacity(16);
            crate::encoding::encode_key_value(&mut b, v);
            xxh64(&b, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh64_is_the_reference() {
        // From the reference implementation (Python's `xxhash`).
        let all: Vec<u8> = (0..100).collect();
        for (bytes, want) in [
            (&b""[..], 0xef46db3751d8e999u64),
            (b"a", 0xd24ec4f1a98c6e5b),
            (b"abc", 0x44bc2cf5ad770999),
            (
                b"Nobody inspects the spammish repetition",
                0xfbcea83c8a378bf1,
            ),
            (&all[..], 0x6ac1e58032166597),
        ] {
            assert_eq!(xxh64(bytes, 0), want, "{} bytes", bytes.len());
        }
    }

    #[test]
    fn the_quick_forms_are_the_key_encoding() {
        let encoded = |v: &Value| {
            let mut b = Vec::new();
            crate::encoding::encode_key_value(&mut b, v);
            xxh64(&b, 0)
        };
        for i in [0, 1, -1, i64::MIN, i64::MAX, 1 << 40, -12345] {
            assert_eq!(hash_value(&Value::Int(i)), encoded(&Value::Int(i)));
        }
        for x in [
            0.0,
            -0.0,
            1.5,
            -2.25,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -f64::NAN,
            f64::MIN_POSITIVE,
            1e300,
        ] {
            assert_eq!(
                hash_value(&Value::Float(x)),
                encoded(&Value::Float(x)),
                "{x}"
            );
        }
        assert_eq!(
            hash_value(&Value::Float(-0.0)),
            hash_value(&Value::Float(0.0))
        );
        assert_eq!(
            hash_value(&Value::Real(1.5)),
            hash_value(&Value::Float(1.5))
        );
    }
}
