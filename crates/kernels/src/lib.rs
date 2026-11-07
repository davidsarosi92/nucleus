//! Batch kernels (PLAN.md §M14.20 V3): the vectorized executor's inner
//! loops over typed columns, compiled once per CPU feature level and chosen
//! at run time — AVX2 or AVX-512 on x86-64 (detected), NEON on AArch64
//! (always there), plain code elsewhere.  Each loop is written so the
//! compiler vectorizes it: dense, over every position of a batch, branch
//! free; positions outside a batch's selection are computed and ignored,
//! so a kernel never fails — what can fail (an overflow, a division by
//! zero) comes back as a mask the caller checks on the rows in play.
//!
//! [`reference`] holds the scalar definitions; the tests run every kernel
//! at every level the CPU has against them, and the definitions against
//! the SQL layer's own (`crates/sql/tests/kernels.rs`).
//!
//! The workspace forbids unsafe code; this crate allows it in one place:
//! calling a kernel's copy compiled for a CPU feature, once the CPU was
//! found to have it ([`levels`]) — which is what makes that call sound.

use std::sync::OnceLock;

/// A CPU feature level kernels are compiled for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Whatever the target's baseline has (NEON on AArch64).
    Base,
    /// x86-64 with AVX2.
    Avx2,
    /// x86-64 with AVX-512 (F, BW, DQ, VL).
    Avx512,
}

/// The best level this CPU has.
pub fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| levels().last().copied().unwrap_or(Level::Base))
}

/// Every level this CPU has, worst first.
pub fn levels() -> Vec<Level> {
    #[allow(unused_mut)]
    let mut out = vec![Level::Base];
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            out.push(Level::Avx2);
        }
        if std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512dq")
            && std::arch::is_x86_feature_detected!("avx512vl")
        {
            out.push(Level::Avx512);
        }
    }
    out
}

/// The level's name.
pub fn level_name(l: Level) -> &'static str {
    match l {
        Level::Base if cfg!(target_arch = "aarch64") => "neon",
        Level::Base => "scalar",
        Level::Avx2 => "avx2",
        Level::Avx512 => "avx512",
    }
}

/// Defines a kernel: `$name` at the best level, `$at` at a given one (for
/// the tests), the body compiled for each.
macro_rules! kernel {
    ($(#[$doc:meta])* fn $name:ident / $at:ident ($($arg:ident: $ty:ty),*) $(-> $ret:ty)? $body:block) => {
        $(#[$doc])*
        pub fn $name($($arg: $ty),*) $(-> $ret)? {
            $at(level(), $($arg),*)
        }

        #[doc = concat!("[`", stringify!($name), "`] at `level` (one this CPU has).")]
        #[allow(unused_variables)]
        pub fn $at(level: Level, $($arg: $ty),*) $(-> $ret)? {
            #[inline(always)]
            fn imp($($arg: $ty),*) $(-> $ret)? $body
            #[cfg(target_arch = "x86_64")]
            #[allow(unsafe_code)]
            {
                #[target_feature(enable = "avx2")]
                fn avx2($($arg: $ty),*) $(-> $ret)? { imp($($arg),*) }
                #[target_feature(enable = "avx512f,avx512bw,avx512dq,avx512vl")]
                fn avx512($($arg: $ty),*) $(-> $ret)? { imp($($arg),*) }
                match level {
                    // SAFETY: the level was detected on this CPU.
                    Level::Avx512 => return unsafe { avx512($($arg),*) },
                    Level::Avx2 => return unsafe { avx2($($arg),*) },
                    Level::Base => {}
                }
            }
            imp($($arg),*)
        }
    };
}

// ---------------------------------------------------------------- CRC-32C

/// The CRC-32C (Castagnoli) of `data` — the storage layer's page and WAL
/// checksum — by the CPU's instructions where it has them (ARMv8 CRC,
/// SSE 4.2; detected once), else portably: the same value either way.
pub fn crc32c(data: &[u8]) -> u32 {
    static HARDWARE: OnceLock<bool> = OnceLock::new();
    if *HARDWARE.get_or_init(crc_hardware) {
        return crc32c_hardware(data);
    }
    crc32c_portable(data)
}

/// Whether this CPU has CRC-32C instructions.
pub fn crc_hardware() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("crc")
    }
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("sse4.2")
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

/// [`crc32c`] by the CPU's instructions (only when [`crc_hardware`]).
///
/// The instruction takes eight bytes, but each step waits for the one
/// before (three cycles on current cores), so three independent streams
/// over three neighbouring blocks run at once and are joined after:
/// `crc(A ‖ B) = shift(crc(A), |B|) ⊕ crc(B)` on the raw register, the
/// shift by a block's length a table lookup per byte ([`ZEROS_LONG`],
/// [`ZEROS_SHORT`]).  Mark Adler's `crc32c.c` and Intel's "Fast CRC
/// Computation for iSCSI Polynomial Using CRC32 Instruction" (2011): an
/// 8 KiB page in 353 ns rather than one stream's 934 (Apple M-series).
pub fn crc32c_hardware(data: &[u8]) -> u32 {
    assert!(crc_hardware(), "no CRC-32C instructions on this CPU");
    // The interleaved loop over `$step` (eight bytes) and `$byte`.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    macro_rules! interleaved {
        ($data:expr, $step:expr, $byte:expr) => {{
            let mut data: &[u8] = $data;
            let mut c0 = !0u32;
            for (block, zeros) in [(LONG, &ZEROS_LONG), (SHORT, &ZEROS_SHORT)] {
                while data.len() >= 3 * block {
                    let (a, rest) = data.split_at(block);
                    let (b, rest) = rest.split_at(block);
                    let (c, rest) = rest.split_at(block);
                    let (mut c1, mut c2) = (0u32, 0u32);
                    for ((x, y), z) in a
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .zip(b.as_chunks::<8>().0)
                        .zip(c.as_chunks::<8>().0)
                    {
                        c0 = $step(c0, u64::from_le_bytes(*x));
                        c1 = $step(c1, u64::from_le_bytes(*y));
                        c2 = $step(c2, u64::from_le_bytes(*z));
                    }
                    c0 = shift(zeros, c0) ^ c1;
                    c0 = shift(zeros, c0) ^ c2;
                    data = rest;
                }
            }
            let (chunks, rest) = data.as_chunks::<8>();
            for b in chunks {
                c0 = $step(c0, u64::from_le_bytes(*b));
            }
            for &b in rest {
                c0 = $byte(c0, b);
            }
            !c0
        }};
    }
    #[cfg(target_arch = "aarch64")]
    #[allow(unsafe_code)]
    {
        #[target_feature(enable = "crc")]
        fn imp(data: &[u8]) -> u32 {
            use std::arch::aarch64::{__crc32cb, __crc32cd};
            interleaved!(data, __crc32cd, __crc32cb)
        }
        // SAFETY: the CPU has the instructions (checked above).
        unsafe { imp(data) }
    }
    #[cfg(target_arch = "x86_64")]
    #[allow(unsafe_code)]
    {
        #[target_feature(enable = "sse4.2")]
        fn imp(data: &[u8]) -> u32 {
            use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
            interleaved!(
                data,
                |c: u32, w: u64| _mm_crc32_u64(u64::from(c), w) as u32,
                _mm_crc32_u8
            )
        }
        // SAFETY: the CPU has the instructions (checked above).
        unsafe { imp(data) }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        crc32c_portable(data)
    }
}

/// The interleaved blocks' lengths (powers of two): three of [`LONG`]
/// while they fit, then of [`SHORT`].
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const LONG: usize = 2048;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const SHORT: usize = 256;

/// The raw CRC register followed by [`LONG`] / [`SHORT`] zero bytes, a
/// byte of the register at a time (`[k][b]`: byte `k` being `b`).
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
static ZEROS_LONG: [[u32; 256]; 4] = zeros_table(LONG);
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
static ZEROS_SHORT: [[u32; 256]; 4] = zeros_table(SHORT);

/// `crc` followed by the zeros `zeros` is the table of.
#[inline(always)]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn shift(zeros: &[[u32; 256]; 4], crc: u32) -> u32 {
    zeros[0][(crc & 0xFF) as usize]
        ^ zeros[1][((crc >> 8) & 0xFF) as usize]
        ^ zeros[2][((crc >> 16) & 0xFF) as usize]
        ^ zeros[3][(crc >> 24) as usize]
}

/// The 32 × 32 GF(2) matrix `mat` times `vec`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const fn gf2_times(mat: &[u32; 32], mut vec: u32) -> u32 {
    let (mut sum, mut i) = (0, 0);
    while vec != 0 {
        if vec & 1 != 0 {
            sum ^= mat[i];
        }
        vec >>= 1;
        i += 1;
    }
    sum
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const fn gf2_square(mat: &[u32; 32]) -> [u32; 32] {
    let mut sq = [0u32; 32];
    let mut n = 0;
    while n < 32 {
        sq[n] = gf2_times(mat, mat[n]);
        n += 1;
    }
    sq
}

/// The table of appending `len` zero bytes (a power of two) to the raw
/// register: the operator for one zero bit squared up to it.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const fn zeros_table(len: usize) -> [[u32; 256]; 4] {
    let mut op = [0u32; 32];
    op[0] = POLY;
    let mut n = 1;
    while n < 32 {
        op[n] = 1 << (n - 1);
        n += 1;
    }
    // One bit → eight bits (a byte) → `len` bytes.
    let mut bits = 1;
    while bits < 8 * len {
        op = gf2_square(&op);
        bits *= 2;
    }
    let mut t = [[0u32; 256]; 4];
    let mut b = 0;
    while b < 256 {
        let mut k = 0;
        while k < 4 {
            t[k][b] = gf2_times(&op, (b as u32) << (8 * k));
            k += 1;
        }
        b += 1;
    }
    t
}

const POLY: u32 = 0x82F6_3B78; // reflected Castagnoli polynomial

/// Slicing-by-8: `TABLES[k][b]` is the CRC of byte `b` followed by `k`
/// zero bytes, so eight bytes go in per step.
const TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// The CRC-32C of `data`, portably (slicing-by-8, eight bytes a step).
pub fn crc32c_portable(data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut c = !0u32;
    let (chunks, rest) = data.as_chunks::<8>();
    for b in chunks {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = t[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// Integer arithmetic that can overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
}

/// The second operand of a binary kernel: a column or a constant.
#[derive(Clone, Copy, Debug)]
pub enum Rhs<'a, T> {
    /// A value per position.
    Col(&'a [T]),
    /// One value.
    Const(T),
}

macro_rules! each_cmp {
    ($op:expr, $out:expr, $a:expr, $b:expr, |$x:ident, $y:ident| $eq:expr, $lt:expr) => {{
        let out: &mut [bool] = $out;
        match $b {
            Rhs::Col(b) => {
                let (a, b) = ($a, &b[..$a.len()]);
                match $op {
                    Cmp::Eq => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$x, &$y))| *o = $eq),
                    Cmp::Ne => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$x, &$y))| *o = !$eq),
                    Cmp::Lt => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$x, &$y))| *o = $lt),
                    Cmp::Ge => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$x, &$y))| *o = !$lt),
                    Cmp::Gt => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$y, &$x))| *o = $lt),
                    Cmp::Le => out
                        .iter_mut()
                        .zip(a.iter().zip(b))
                        .for_each(|(o, (&$y, &$x))| *o = !$lt),
                }
            }
            Rhs::Const(c) => {
                let a = $a;
                match $op {
                    Cmp::Eq => out.iter_mut().zip(a).for_each(|(o, &$x)| {
                        let $y = c;
                        *o = $eq
                    }),
                    Cmp::Ne => out.iter_mut().zip(a).for_each(|(o, &$x)| {
                        let $y = c;
                        *o = !$eq
                    }),
                    Cmp::Lt => out.iter_mut().zip(a).for_each(|(o, &$x)| {
                        let $y = c;
                        *o = $lt
                    }),
                    Cmp::Ge => out.iter_mut().zip(a).for_each(|(o, &$x)| {
                        let $y = c;
                        *o = !$lt
                    }),
                    Cmp::Gt => out.iter_mut().zip(a).for_each(|(o, &$y)| {
                        let $x = c;
                        *o = $lt
                    }),
                    Cmp::Le => out.iter_mut().zip(a).for_each(|(o, &$y)| {
                        let $x = c;
                        *o = !$lt
                    }),
                }
            }
        }
    }};
}

kernel! {
    /// `out[i] = a[i] op b[i]` over integers.
    fn cmp_i64 / cmp_i64_at(op: Cmp, a: &[i64], b: Rhs<i64>, out: &mut [bool]) {
        each_cmp!(op, out, a, b, |x, y| x == y, x < y)
    }
}

kernel! {
    /// `out[i] = a[i] op b[i]` over doubles, in SQL's total order: `NaN`
    /// equal to itself and above every other value, `-0` equal to `0`.
    fn cmp_f64 / cmp_f64_at(op: Cmp, a: &[f64], b: Rhs<f64>, out: &mut [bool]) {
        each_cmp!(
            op, out, a, b,
            |x, y| (x == y) | (x.is_nan() & y.is_nan()),
            (x < y) | (!x.is_nan() & y.is_nan())
        )
    }
}

kernel! {
    /// `out[i] = a[i] op b[i]` over integers, wrapping, `over[i]` set where
    /// it overflowed.
    fn arith_i64 / arith_i64_at(op: Arith, a: &[i64], b: Rhs<i64>, out: &mut [i64], over: &mut [bool]) {
        let f = match op {
            Arith::Add => i64::overflowing_add,
            Arith::Sub => i64::overflowing_sub,
            Arith::Mul => i64::overflowing_mul,
        };
        match b {
            Rhs::Col(b) => {
                for (i, (&x, &y)) in a.iter().zip(&b[..a.len()]).enumerate() {
                    let (r, o) = f(x, y);
                    out[i] = r;
                    over[i] = o;
                }
            }
            Rhs::Const(c) => {
                for (i, &x) in a.iter().enumerate() {
                    let (r, o) = f(x, c);
                    out[i] = r;
                    over[i] = o;
                }
            }
        }
    }
}

/// Double arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FArith {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
}

kernel! {
    /// `out[i] = a[i] op b[i]` over doubles (IEEE 754; the caller checks
    /// overflow and division by zero on the rows in play).
    fn arith_f64 / arith_f64_at(op: FArith, a: &[f64], b: Rhs<f64>, out: &mut [f64]) {
        macro_rules! go {
            ($f:expr) => {
                match b {
                    Rhs::Col(b) => out.iter_mut().zip(a.iter().zip(&b[..a.len()])).for_each(|(o, (&x, &y))| *o = $f(x, y)),
                    Rhs::Const(c) => out.iter_mut().zip(a).for_each(|(o, &x)| *o = $f(x, c)),
                }
            };
        }
        match op {
            FArith::Add => go!(|x: f64, y: f64| x + y),
            FArith::Sub => go!(|x: f64, y: f64| x - y),
            FArith::Mul => go!(|x: f64, y: f64| x * y),
            FArith::Div => go!(|x: f64, y: f64| x / y),
        }
    }
}

kernel! {
    /// The sum of the integers not `NULL` (exact), and how many they are.
    fn sum_i64 / sum_i64_at(a: &[i64], nulls: Option<&[bool]>) -> (i128, u64) {
        // Halves summed apart, each in 64 bits that cannot overflow for
        // fewer than 2^31 values: the high (signed) 32 bits and the low
        // (unsigned) 32 bits.
        let mut out = (0i128, 0u64);
        for block in 0..a.len().div_ceil(1 << 30) {
            let range = block << 30..((block + 1) << 30).min(a.len());
            let (mut hi, mut lo, mut n) = (0i64, 0u64, 0u64);
            match nulls {
                None => {
                    for &x in &a[range.clone()] {
                        hi += x >> 32;
                        lo += x as u64 & 0xFFFF_FFFF;
                    }
                    n = range.len() as u64;
                }
                Some(nulls) => {
                    for (&x, &null) in a[range.clone()].iter().zip(&nulls[range]) {
                        let keep = i64::from(!null);
                        hi += (x >> 32) * keep;
                        lo += (x as u64 & 0xFFFF_FFFF) * keep as u64;
                        n += keep as u64;
                    }
                }
            }
            out.0 += (i128::from(hi) << 32) + i128::from(lo);
            out.1 += n;
        }
        out
    }
}

kernel! {
    /// The least (`max` false) or greatest integer not `NULL`.
    fn minmax_i64 / minmax_i64_at(a: &[i64], nulls: Option<&[bool]>, max: bool) -> Option<i64> {
        match (nulls, max) {
            (None, false) => a.iter().copied().min(),
            (None, true) => a.iter().copied().max(),
            (Some(n), false) => {
                let m = a.iter().zip(n).map(|(&x, &null)| if null { i64::MAX } else { x }).min();
                n.contains(&false).then_some(m).flatten()
            }
            (Some(n), true) => {
                let m = a.iter().zip(n).map(|(&x, &null)| if null { i64::MIN } else { x }).max();
                n.contains(&false).then_some(m).flatten()
            }
        }
    }
}

kernel! {
    /// `out[i]`: whether string `i` (bytes `offsets[i]..offsets[i + 1]`)
    /// equals `needle`.
    fn eq_bytes / eq_bytes_at(offsets: &[u32], bytes: &[u8], needle: &[u8], out: &mut [bool]) {
        for (i, o) in out.iter_mut().enumerate() {
            let (s, e) = (offsets[i] as usize, offsets[i + 1] as usize);
            *o = &bytes[s..e] == needle;
        }
    }
}

/// Where a `LIKE` pattern without wildcards inside it must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// `'abc%'`.
    Prefix,
    /// `'%abc'`.
    Suffix,
    /// `'%abc%'`.
    Contains,
}

kernel! {
    /// `out[i]`: whether string `i` matches `needle` as `anchor` says.
    fn like_bytes / like_bytes_at(offsets: &[u32], bytes: &[u8], needle: &[u8], anchor: Anchor, out: &mut [bool]) {
        for (i, o) in out.iter_mut().enumerate() {
            let s = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
            *o = match anchor {
                Anchor::Prefix => s.starts_with(needle),
                Anchor::Suffix => s.ends_with(needle),
                Anchor::Contains => contains(s, needle),
            };
        }
    }
}

kernel! {
    /// `out[i]`: whether string `i` matches the `LIKE` pattern whose only
    /// wildcard is `%`, given as the text between them (`parts`: the
    /// pattern split at each `%`, so at least two) — the first part a
    /// prefix, the last a suffix, the ones between found in order, each
    /// after the last (the leftmost match of each leaves the most room for
    /// the rest, so it is the only one to try).
    fn like_parts / like_parts_at(offsets: &[u32], bytes: &[u8], parts: &[&[u8]], out: &mut [bool]) {
        let (first, last) = (parts[0], parts[parts.len() - 1]);
        let middle = &parts[1..parts.len() - 1];
        for (i, o) in out.iter_mut().enumerate() {
            let s = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
            *o = s.len() >= first.len() + last.len()
                && s.starts_with(first)
                && s.ends_with(last)
                && {
                    let mut rest = &s[first.len()..s.len() - last.len()];
                    middle.iter().all(|m| match find(rest, m) {
                        Some(at) => {
                            rest = &rest[at + m.len()..];
                            true
                        }
                        None => false,
                    })
                };
        }
    }
}

/// Whether `needle` occurs in `hay`: candidates by its first byte, then
/// compared.
#[inline(always)]
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

/// Where `needle` first occurs in `hay`: candidates where its first and
/// its last byte both are, 32 starts at a time (compares the compiler
/// makes vector instructions of), then compared.
#[inline(always)]
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    match n {
        0 => return Some(0),
        _ if hay.len() < n => return None,
        1 => return hay.iter().position(|&b| b == needle[0]),
        _ => {}
    }
    let (first, last, inner) = (needle[0], needle[n - 1], &needle[1..n - 1]);
    // Candidate starts: `0..end`.
    let end = hay.len() - n + 1;
    const W: usize = 32;
    let mut i = 0;
    while i + W <= end {
        let (a, b) = (&hay[i..i + W], &hay[i + n - 1..i + n - 1 + W]);
        let mut mask = 0u32;
        for j in 0..W {
            mask |= u32::from((a[j] == first) & (b[j] == last)) << j;
        }
        while mask != 0 {
            let j = mask.trailing_zeros() as usize;
            if &hay[i + j + 1..i + j + n - 1] == inner {
                return Some(i + j);
            }
            mask &= mask - 1;
        }
        i += W;
    }
    (i..end).find(|&k| hay[k] == first && hay[k + n - 1] == last && &hay[k + 1..k + n - 1] == inner)
}

kernel! {
    /// Hashes of integer keys (a multiplicative hash: well spread, cheap).
    fn hash_i64 / hash_i64_at(a: &[i64], out: &mut [u64]) {
        for (o, &x) in out.iter_mut().zip(a) {
            *o = mix(x as u64);
        }
    }
}

/// A 64-bit mix (the finalizer of SplitMix64).
#[inline(always)]
pub fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A hasher for the executor's hash tables (group keys): [`mix`] over the
/// words written, from a random seed ([`MixState`]) — far cheaper than the
/// standard library's SipHash.
#[derive(Clone, Copy, Debug, Default)]
pub struct MixHasher(u64);

impl std::hash::Hasher for MixHasher {
    fn finish(&self) -> u64 {
        mix(self.0)
    }

    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.0 = mix(self.0 ^ u64::from_le_bytes(w)).wrapping_add(c.len() as u64);
        }
    }

    fn write_u64(&mut self, x: u64) {
        self.0 = mix(self.0 ^ x);
    }

    fn write_i64(&mut self, x: i64) {
        self.write_u64(x as u64);
    }

    fn write_usize(&mut self, x: usize) {
        self.write_u64(x as u64);
    }
}

/// [`MixHasher`]s for `HashMap`s, seeded once per process at random — so
/// keys cannot be chosen in advance to collide.
#[derive(Clone, Copy, Debug)]
pub struct MixState(u64);

impl Default for MixState {
    fn default() -> Self {
        static SEED: OnceLock<u64> = OnceLock::new();
        MixState(*SEED.get_or_init(|| {
            use std::hash::{BuildHasher, Hasher};
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        }))
    }
}

impl std::hash::BuildHasher for MixState {
    type Hasher = MixHasher;

    fn build_hasher(&self) -> MixHasher {
        MixHasher(self.0)
    }
}

kernel! {
    /// The positions where `mask` is set, among `0..mask.len()`.
    fn select / select_at(mask: &[bool], out: &mut Vec<u32>) {
        out.clear();
        out.reserve(mask.len());
        for (i, &m) in mask.iter().enumerate() {
            if m {
                out.push(i as u32);
            }
        }
    }
}

// ------------------------------------------------------------- distances

/// Floats a distance kernel adds in parallel: sixteen running sums, each
/// over every sixteenth component, added together at the end in a fixed
/// order — so a distance is the same bits at every level (no level fuses
/// a multiply into an add), and NEON, AVX2 and AVX-512 each keep the
/// sixteen sums in their registers.
pub const LANES: usize = 16;

/// Sixteen running sums of `term(x, y)` over the components of `a` and `b`
/// (little-endian `f32`s, a vector's stored form; as many as the shorter
/// has), component `i` in sum `i % 16`, added up in a fixed order (as
/// `f64`).  A whole block of sixteen at a time, as fixed-size arrays, so
/// the compiler keeps the sums in registers without bounds checks.
#[inline(always)]
fn lanes(a: &[u8], b: &[u8], term: impl Fn(f32, f32) -> f32) -> f64 {
    let n = a.len().min(b.len()) / 4;
    let (a, b) = (&a[..4 * n], &b[..4 * n]);
    let mut acc = [0f32; LANES];
    let (ab, ra) = a.as_chunks::<{ 4 * LANES }>();
    let (bb, rb) = b.as_chunks::<{ 4 * LANES }>();
    for (x, y) in ab.iter().zip(bb) {
        for l in 0..LANES {
            let xi = f32::from_le_bytes([x[4 * l], x[4 * l + 1], x[4 * l + 2], x[4 * l + 3]]);
            let yi = f32::from_le_bytes([y[4 * l], y[4 * l + 1], y[4 * l + 2], y[4 * l + 3]]);
            acc[l] += term(xi, yi);
        }
    }
    let (ra, rb) = (ra.as_chunks::<4>().0, rb.as_chunks::<4>().0);
    for (l, (x, y)) in ra.iter().zip(rb).enumerate() {
        acc[l] += term(f32::from_le_bytes(*x), f32::from_le_bytes(*y));
    }
    acc.iter().map(|&s| f64::from(s)).sum()
}

kernel! {
    /// The squared Euclidean distance of two vectors stored as
    /// little-endian `f32`s (the same number of them).
    fn l2_squared / l2_squared_at(a: &[u8], b: &[u8]) -> f64 {
        lanes(a, b, |x, y| (x - y) * (x - y))
    }
}

kernel! {
    /// The inner product of two vectors stored as little-endian `f32`s.
    fn dot / dot_at(a: &[u8], b: &[u8]) -> f64 {
        lanes(a, b, |x, y| x * y)
    }
}

kernel! {
    /// The taxicab (L1) distance of two vectors stored as little-endian
    /// `f32`s.
    fn l1 / l1_at(a: &[u8], b: &[u8]) -> f64 {
        lanes(a, b, |x, y| (x - y).abs())
    }
}

kernel! {
    /// The inner product of two vectors and the squares of their norms
    /// (the cosine distance's parts).
    fn dot_norms / dot_norms_at(a: &[u8], b: &[u8]) -> (f64, f64, f64) {
        (
            lanes(a, b, |x, y| x * y),
            lanes(a, a, |x, y| x * y),
            lanes(b, b, |x, y| x * y),
        )
    }
}

/// An IEEE half (binary16, finite) as an `f32`: its bits moved to an
/// `f32`'s places, then scaled by 2¹¹² (the exponents' bias apart) — exact,
/// subnormals too, and without a branch, so the loops over halves
/// vectorize (Giesen's conversion).
#[inline(always)]
pub fn from_half(h: u16) -> f32 {
    let h = u32::from(h);
    let magnitude = f32::from_bits((h & 0x7fff) << 13) * f32::from_bits(0x7780_0000);
    f32::from_bits(((h & 0x8000) << 16) | magnitude.to_bits())
}

/// `x` (finite) as the nearest IEEE half, ties to even: magnitudes under
/// the least normal half (2⁻¹⁴) become zero, over the greatest (65504)
/// the greatest.
pub fn to_half(x: f32) -> u16 {
    let sign = ((x.to_bits() >> 16) & 0x8000) as u16;
    let a = x.abs();
    if a < f32::from_bits(0x3880_0000) {
        return sign;
    }
    if a >= 65504.0 {
        return sign | 0x7bff;
    }
    let b = a.to_bits();
    let rounded = (b + 0xfff + ((b >> 13) & 1)) >> 13;
    sign | (rounded - (112 << 10)).min(0x7bff) as u16
}

/// [`lanes`] over vectors stored as little-endian IEEE halves.
#[inline(always)]
fn half_lanes(a: &[u8], b: &[u8], term: impl Fn(f32, f32) -> f32) -> f64 {
    let n = a.len().min(b.len()) / 2;
    let (a, b) = (&a[..2 * n], &b[..2 * n]);
    let mut acc = [0f32; LANES];
    let (ab, ra) = a.as_chunks::<{ 2 * LANES }>();
    let (bb, rb) = b.as_chunks::<{ 2 * LANES }>();
    for (x, y) in ab.iter().zip(bb) {
        for l in 0..LANES {
            let xi = from_half(u16::from_le_bytes([x[2 * l], x[2 * l + 1]]));
            let yi = from_half(u16::from_le_bytes([y[2 * l], y[2 * l + 1]]));
            acc[l] += term(xi, yi);
        }
    }
    let (ra, rb) = (ra.as_chunks::<2>().0, rb.as_chunks::<2>().0);
    for (l, (x, y)) in ra.iter().zip(rb).enumerate() {
        acc[l] += term(
            from_half(u16::from_le_bytes(*x)),
            from_half(u16::from_le_bytes(*y)),
        );
    }
    acc.iter().map(|&s| f64::from(s)).sum()
}

kernel! {
    /// [`l2_squared`] of two vectors stored as little-endian halves.
    fn l2_squared_half / l2_squared_half_at(a: &[u8], b: &[u8]) -> f64 {
        half_lanes(a, b, |x, y| (x - y) * (x - y))
    }
}

kernel! {
    /// [`dot`] of two vectors stored as little-endian halves.
    fn dot_half / dot_half_at(a: &[u8], b: &[u8]) -> f64 {
        half_lanes(a, b, |x, y| x * y)
    }
}

kernel! {
    /// [`l1`] of two vectors stored as little-endian halves.
    fn l1_half / l1_half_at(a: &[u8], b: &[u8]) -> f64 {
        half_lanes(a, b, |x, y| (x - y).abs())
    }
}

kernel! {
    /// The sum of the products of two runs of bytes as unsigned integers
    /// (as many as the shorter has; exact up to 66,000 of them).
    fn dot_u8 / dot_u8_at(a: &[u8], b: &[u8]) -> u32 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let mut acc = [0u32; 2 * LANES];
        let (ab, ra) = a.as_chunks::<{ 2 * LANES }>();
        let (bb, rb) = b.as_chunks::<{ 2 * LANES }>();
        for (x, y) in ab.iter().zip(bb) {
            for l in 0..2 * LANES {
                acc[l] += u32::from(x[l]) * u32::from(y[l]);
            }
        }
        let rest: u32 = ra.iter().zip(rb).map(|(x, y)| u32::from(*x) * u32::from(*y)).sum();
        acc.iter().sum::<u32>() + rest
    }
}

kernel! {
    /// The taxicab distance of two vectors of bytes, each component `i`
    /// standing for `m + s · byte` (`a`'s `ma`, `sa`; `b`'s `mb`, `sb`).
    fn l1_u8 / l1_u8_at(a: &[u8], b: &[u8], ma: f32, sa: f32, mb: f32, sb: f32) -> f64 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let m = ma - mb;
        let mut acc = [0f32; LANES];
        let (ab, ra) = a.as_chunks::<LANES>();
        let (bb, rb) = b.as_chunks::<LANES>();
        for (x, y) in ab.iter().zip(bb) {
            for l in 0..LANES {
                acc[l] += (m + sa * f32::from(x[l]) - sb * f32::from(y[l])).abs();
            }
        }
        for (l, (x, y)) in ra.iter().zip(rb).enumerate() {
            acc[l] += (m + sa * f32::from(*x) - sb * f32::from(*y)).abs();
        }
        acc.iter().map(|&s| f64::from(s)).sum()
    }
}

kernel! {
    /// The bits that differ between two runs of bytes (the Hamming
    /// distance; as many bytes as the shorter has).
    fn hamming / hamming_at(a: &[u8], b: &[u8]) -> u32 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        let (aw, ra) = a.as_chunks::<8>();
        let (bw, rb) = b.as_chunks::<8>();
        let words: u32 = aw
            .iter()
            .zip(bw)
            .map(|(x, y)| (u64::from_le_bytes(*x) ^ u64::from_le_bytes(*y)).count_ones())
            .sum();
        words + ra.iter().zip(rb).map(|(x, y)| (x ^ y).count_ones()).sum::<u32>()
    }
}

/// The kernels' definitions, a value at a time — what the tests hold the
/// kernels to.
pub mod reference {
    use super::{Anchor, Arith, Cmp, FArith, LANES, Rhs};
    use std::cmp::Ordering;

    fn floats(b: &[u8]) -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }

    /// The sum of `terms`, component `i` into running sum `i % 16`, the
    /// sums added in order: the kernels' order.
    fn lanes(terms: impl Iterator<Item = f32>) -> f64 {
        let mut acc = [0f32; LANES];
        for (i, t) in terms.enumerate() {
            acc[i % LANES] += t;
        }
        acc.iter().map(|&a| f64::from(a)).sum()
    }

    /// [`super::l2_squared`], a component at a time.
    pub fn l2_squared(a: &[u8], b: &[u8]) -> f64 {
        lanes(
            floats(a)
                .iter()
                .zip(floats(b))
                .map(|(x, y)| (x - y) * (x - y)),
        )
    }

    /// [`super::dot`], a component at a time.
    pub fn dot(a: &[u8], b: &[u8]) -> f64 {
        lanes(floats(a).iter().zip(floats(b)).map(|(x, y)| x * y))
    }

    /// [`super::l1`], a component at a time.
    pub fn l1(a: &[u8], b: &[u8]) -> f64 {
        lanes(floats(a).iter().zip(floats(b)).map(|(x, y)| (x - y).abs()))
    }

    fn halves(b: &[u8]) -> Vec<f32> {
        b.as_chunks::<2>()
            .0
            .iter()
            .map(|c| super::from_half(u16::from_le_bytes(*c)))
            .collect()
    }

    /// [`super::l2_squared_half`], a component at a time.
    pub fn l2_squared_half(a: &[u8], b: &[u8]) -> f64 {
        lanes(
            halves(a)
                .iter()
                .zip(halves(b))
                .map(|(x, y)| (x - y) * (x - y)),
        )
    }

    /// [`super::dot_half`], a component at a time.
    pub fn dot_half(a: &[u8], b: &[u8]) -> f64 {
        lanes(halves(a).iter().zip(halves(b)).map(|(x, y)| x * y))
    }

    /// [`super::l1_half`], a component at a time.
    pub fn l1_half(a: &[u8], b: &[u8]) -> f64 {
        lanes(halves(a).iter().zip(halves(b)).map(|(x, y)| (x - y).abs()))
    }

    /// [`super::dot_u8`], a component at a time.
    pub fn dot_u8(a: &[u8], b: &[u8]) -> u32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| u32::from(*x) * u32::from(*y))
            .sum()
    }

    /// [`super::l1_u8`], a component at a time.
    pub fn l1_u8(a: &[u8], b: &[u8], ma: f32, sa: f32, mb: f32, sb: f32) -> f64 {
        lanes(
            a.iter()
                .zip(b)
                .map(|(x, y)| (ma - mb + sa * f32::from(*x) - sb * f32::from(*y)).abs()),
        )
    }

    /// [`super::hamming`], a byte at a time.
    pub fn hamming(a: &[u8], b: &[u8]) -> u32 {
        a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
    }

    /// [`super::dot_norms`], a component at a time.
    pub fn dot_norms(a: &[u8], b: &[u8]) -> (f64, f64, f64) {
        (dot(a, b), dot(a, a), dot(b, b))
    }

    fn rhs<T: Copy>(b: &Rhs<T>, i: usize) -> T {
        match b {
            Rhs::Col(b) => b[i],
            Rhs::Const(c) => *c,
        }
    }

    fn holds(op: Cmp, o: Ordering) -> bool {
        match op {
            Cmp::Eq => o.is_eq(),
            Cmp::Ne => o.is_ne(),
            Cmp::Lt => o.is_lt(),
            Cmp::Le => o.is_le(),
            Cmp::Gt => o.is_gt(),
            Cmp::Ge => o.is_ge(),
        }
    }

    /// [`super::cmp_i64`].
    pub fn cmp_i64(op: Cmp, a: &[i64], b: Rhs<i64>) -> Vec<bool> {
        (0..a.len())
            .map(|i| holds(op, a[i].cmp(&rhs(&b, i))))
            .collect()
    }

    /// SQL's order of doubles: `NaN` equal to itself and above the rest,
    /// `-0` equal to `0`.
    pub fn total_cmp(a: f64, b: f64) -> Ordering {
        match (a.is_nan(), b.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => a.partial_cmp(&b).expect("not NaN"),
        }
    }

    /// [`super::cmp_f64`].
    pub fn cmp_f64(op: Cmp, a: &[f64], b: Rhs<f64>) -> Vec<bool> {
        (0..a.len())
            .map(|i| holds(op, total_cmp(a[i], rhs(&b, i))))
            .collect()
    }

    /// [`super::arith_i64`]: results (wrapped) and overflows.
    pub fn arith_i64(op: Arith, a: &[i64], b: Rhs<i64>) -> (Vec<i64>, Vec<bool>) {
        (0..a.len())
            .map(|i| {
                let (x, y) = (a[i], rhs(&b, i));
                let checked = match op {
                    Arith::Add => x.checked_add(y),
                    Arith::Sub => x.checked_sub(y),
                    Arith::Mul => x.checked_mul(y),
                };
                let wrapped = match op {
                    Arith::Add => x.wrapping_add(y),
                    Arith::Sub => x.wrapping_sub(y),
                    Arith::Mul => x.wrapping_mul(y),
                };
                (wrapped, checked.is_none())
            })
            .unzip()
    }

    /// [`super::arith_f64`].
    pub fn arith_f64(op: FArith, a: &[f64], b: Rhs<f64>) -> Vec<f64> {
        (0..a.len())
            .map(|i| {
                let (x, y) = (a[i], rhs(&b, i));
                match op {
                    FArith::Add => x + y,
                    FArith::Sub => x - y,
                    FArith::Mul => x * y,
                    FArith::Div => x / y,
                }
            })
            .collect()
    }

    /// [`super::sum_i64`].
    pub fn sum_i64(a: &[i64], nulls: Option<&[bool]>) -> (i128, u64) {
        let mut out = (0i128, 0u64);
        for (i, &x) in a.iter().enumerate() {
            if !nulls.is_some_and(|n| n[i]) {
                out.0 += i128::from(x);
                out.1 += 1;
            }
        }
        out
    }

    /// [`super::minmax_i64`].
    pub fn minmax_i64(a: &[i64], nulls: Option<&[bool]>, max: bool) -> Option<i64> {
        let live = a
            .iter()
            .enumerate()
            .filter(|(i, _)| !nulls.is_some_and(|n| n[*i]))
            .map(|(_, &x)| x);
        if max { live.max() } else { live.min() }
    }

    fn strings<'a>(offsets: &'a [u32], bytes: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
        offsets
            .windows(2)
            .map(move |w| &bytes[w[0] as usize..w[1] as usize])
    }

    /// [`super::eq_bytes`].
    pub fn eq_bytes(offsets: &[u32], bytes: &[u8], needle: &[u8]) -> Vec<bool> {
        strings(offsets, bytes).map(|s| s == needle).collect()
    }

    /// [`super::like_bytes`].
    pub fn like_bytes(offsets: &[u32], bytes: &[u8], needle: &[u8], anchor: Anchor) -> Vec<bool> {
        strings(offsets, bytes)
            .map(|s| match anchor {
                Anchor::Prefix => s.starts_with(needle),
                Anchor::Suffix => s.ends_with(needle),
                Anchor::Contains => {
                    needle.is_empty() || s.windows(needle.len()).any(|w| w == needle)
                }
            })
            .collect()
    }

    /// [`super::like_parts`]: each string matched as the pattern's
    /// characters, by backtracking (`%` the only wildcard).
    pub fn like_parts(offsets: &[u32], bytes: &[u8], parts: &[&[u8]]) -> Vec<bool> {
        fn matches(s: &[u8], p: &[Option<u8>]) -> bool {
            match p.split_first() {
                None => s.is_empty(),
                Some((None, rest)) => (0..=s.len()).any(|k| matches(&s[k..], rest)),
                Some((Some(c), rest)) => s.first() == Some(c) && matches(&s[1..], rest),
            }
        }
        let mut pattern = Vec::new();
        for (k, part) in parts.iter().enumerate() {
            if k > 0 {
                pattern.push(None);
            }
            pattern.extend(part.iter().map(|&b| Some(b)));
        }
        strings(offsets, bytes)
            .map(|s| matches(s, &pattern))
            .collect()
    }

    /// [`super::select`].
    pub fn select(mask: &[bool]) -> Vec<u32> {
        (0..mask.len() as u32)
            .filter(|&i| mask[i as usize])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_at_every_level_are_the_reference() {
        let mut x = 0x9e37_79b9_u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 8.0
        };
        for n in [0, 1, 3, 15, 16, 17, 31, 64, 100, 768, 1536] {
            let bytes = |v: Vec<f32>| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
            let a = bytes((0..n).map(|_| next()).collect());
            let b = bytes((0..n).map(|_| next()).collect());
            for level in levels() {
                let same = |x: f64, y: f64| x.to_bits() == y.to_bits();
                assert!(
                    same(l2_squared_at(level, &a, &b), reference::l2_squared(&a, &b)),
                    "{n}"
                );
                assert!(same(dot_at(level, &a, &b), reference::dot(&a, &b)), "{n}");
                assert!(same(l1_at(level, &a, &b), reference::l1(&a, &b)), "{n}");
                let (k, r) = (dot_norms_at(level, &a, &b), reference::dot_norms(&a, &b));
                assert!(same(k.0, r.0) && same(k.1, r.1) && same(k.2, r.2), "{n}");
            }
            let half = |v: &[u8]| {
                v.as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|c| to_half(f32::from_le_bytes(*c)).to_le_bytes())
                    .collect::<Vec<u8>>()
            };
            let (ha, hb) = (half(&a), half(&b));
            for level in levels() {
                let same = |x: f64, y: f64| x.to_bits() == y.to_bits();
                assert!(
                    same(
                        l2_squared_half_at(level, &ha, &hb),
                        reference::l2_squared_half(&ha, &hb)
                    ),
                    "{n}"
                );
                assert!(same(
                    dot_half_at(level, &ha, &hb),
                    reference::dot_half(&ha, &hb)
                ));
                assert!(same(
                    l1_half_at(level, &ha, &hb),
                    reference::l1_half(&ha, &hb)
                ));
                assert_eq!(dot_u8_at(level, &a, &b), reference::dot_u8(&a, &b), "{n}");
                assert_eq!(hamming_at(level, &a, &b), reference::hamming(&a, &b), "{n}");
                assert!(same(
                    l1_u8_at(level, &a, &b, 0.5, 0.25, -1.0, 0.125),
                    reference::l1_u8(&a, &b, 0.5, 0.25, -1.0, 0.125)
                ));
            }
            // Against plain arithmetic in doubles: within f32's rounding.
            let (fa, fb) = (reference_floats(&a), reference_floats(&b));
            let want: f64 = fa
                .iter()
                .zip(&fb)
                .map(|(x, y)| f64::from(x - y).powi(2))
                .sum();
            assert!(
                (l2_squared(&a, &b) - want).abs() <= 1e-4 * want.max(1.0),
                "{n}"
            );
        }
    }

    #[test]
    fn halves_round_to_nearest_even_and_back() {
        for (x, h) in [
            (0.0f32, 0x0000u16),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (-2.0, 0xc000),
            (65504.0, 0x7bff),
            (1e6, 0x7bff),
            (-1e6, 0xfbff),
            (6.103_515_6e-5, 0x0400),
            (1e-6, 0x0000),
            // 1 + 2⁻¹¹ is halfway between 1 and 1 + 2⁻¹⁰: to the even one.
            (1.000_488_3, 0x3c00),
            (1.001_464_8, 0x3c02),
            (0.1, 0x2e66),
        ] {
            assert_eq!(to_half(x), h, "{x}");
        }
        // Every finite normal half comes back as itself.
        for h in 0x0400u16..0x7c00 {
            for s in [0, 0x8000] {
                assert_eq!(to_half(from_half(h | s)), h | s);
            }
        }
        assert_eq!(from_half(0x3555), 0.333_251_95);
        assert_eq!(from_half(0x0001), f32::from_bits(0x3380_0000));
    }

    fn reference_floats(b: &[u8]) -> Vec<f32> {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }

    #[test]
    fn crc32c_by_the_cpu_is_the_portable_one() {
        assert_eq!(crc32c_portable(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        let data: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        if !crc_hardware() {
            return;
        }
        // Every length and alignment around the eight-byte steps.
        for start in 0..9 {
            for n in 0..200 {
                let d = &data[start..start + n];
                assert_eq!(
                    crc32c_hardware(d),
                    crc32c_portable(d),
                    "start {start}, length {n}"
                );
            }
        }
        assert_eq!(crc32c_hardware(&data), crc32c_portable(&data));
        // Around the interleaved blocks: three of each length, more, and
        // the rest after them.
        let data: Vec<u8> = (0..40_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        for n in [
            767, 768, 769, 775, 776, 1536, 6143, 6144, 6145, 6152, 6913, 8192, 12_288, 18_431,
            18_432, 39_990,
        ] {
            for start in [0, 3] {
                let d = &data[start..start + n];
                assert_eq!(
                    crc32c_hardware(d),
                    crc32c_portable(d),
                    "start {start}, length {n}"
                );
            }
        }
    }
}
