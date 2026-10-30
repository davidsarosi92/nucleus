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
pub fn crc32c_hardware(data: &[u8]) -> u32 {
    assert!(crc_hardware(), "no CRC-32C instructions on this CPU");
    #[cfg(target_arch = "aarch64")]
    #[allow(unsafe_code)]
    {
        #[target_feature(enable = "crc")]
        fn imp(data: &[u8]) -> u32 {
            use std::arch::aarch64::{__crc32cb, __crc32cd};
            let mut c = !0u32;
            let (chunks, rest) = data.as_chunks::<8>();
            for b in chunks {
                c = __crc32cd(c, u64::from_le_bytes(*b));
            }
            for &b in rest {
                c = __crc32cb(c, b);
            }
            !c
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
            let mut c = u64::from(!0u32);
            let (chunks, rest) = data.as_chunks::<8>();
            for b in chunks {
                c = _mm_crc32_u64(c, u64::from_le_bytes(*b));
            }
            let mut c = c as u32;
            for &b in rest {
                c = _mm_crc32_u8(c, b);
            }
            !c
        }
        // SAFETY: the CPU has the instructions (checked above).
        unsafe { imp(data) }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        crc32c_portable(data)
    }
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

/// Whether `needle` occurs in `hay`: candidates by its first byte, then
/// compared.
#[inline(always)]
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if hay.len() < needle.len() {
        return false;
    }
    let last = hay.len() - needle.len();
    let mut i = 0;
    while i <= last {
        match hay[i..=last].iter().position(|&b| b == first) {
            None => return false,
            Some(p) => {
                i += p;
                if &hay[i + 1..i + needle.len()] == rest {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
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

/// The kernels' definitions, a value at a time — what the tests hold the
/// kernels to.
pub mod reference {
    use super::{Anchor, Arith, Cmp, FArith, Rhs};
    use std::cmp::Ordering;

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
    }
}
