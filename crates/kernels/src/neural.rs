//! The kernels of a neural network's forward pass (PLAN.md §M14.22 E3b):
//! an f32 matrix product, and the element-wise and row-wise functions
//! between products (GELU, softmax, layer normalization).
//!
//! **The matrix product** follows the structure of GotoBLAS / BLIS
//! (Goto and van de Geijn, "Anatomy of high-performance matrix
//! multiplication", 2008): the right-hand matrix packed once into panels
//! of `NR` columns (a model's weights are packed when it is loaded),
//! the left-hand one packed a block at a time into panels of `MR` rows,
//! and an `MR × NR` register tile accumulated by fused multiply-adds
//! over a block of `KC` of the shared dimension — written as plain loops
//! over fixed-size arrays, which LLVM turns into the SIMD instructions
//! of the level the kernel is compiled for.
//!
//! **Every output is one chain of fused multiply-adds, in order of the
//! shared dimension**, whatever the blocking, the SIMD width or the
//! number of threads the caller splits rows across: a tile is loaded
//! from the output and continued, never summed in parts.  So a model
//! gives bit-identical vectors on one thread or on many and at every
//! CPU level (`f32::mul_add` rounds once on every one of them).

use crate::{Level, level};

/// Rows of a register tile.
#[cfg(target_arch = "aarch64")]
pub const MR: usize = 8;
/// Columns of a register tile (NEON: 8 × 12 in 24 of the 32 vector
/// registers, BLIS's choice for ARMv8).
#[cfg(target_arch = "aarch64")]
pub const NR: usize = 12;
/// Rows of a register tile.
#[cfg(not(target_arch = "aarch64"))]
pub const MR: usize = 6;
/// Columns of a register tile (AVX2: 6 × 16 in 12 of the 16 registers).
#[cfg(not(target_arch = "aarch64"))]
pub const NR: usize = 16;

/// The block of the shared dimension a packed panel of the left-hand
/// matrix holds.
const KC: usize = 512;
/// The rows of the left-hand matrix packed at a time (a multiple of 6
/// and 8): with `KC`, 96 KiB.  Measured on an M2 Pro core (BERT-small's
/// shapes): 512 × 48 gave 82–93 GFLOP/s of the core's 112, 256 × 48
/// 78–86, 128 × 48 68–76; a longer `KC` gained a little on the widest
/// product only.
const MC: usize = 48;

/// The right-hand side of [`matmul`], `k × n`, packed: panel `j` holds
/// columns `j·NR ..` for every row in turn, the last panel padded with
/// zeros.
#[derive(Clone)]
pub struct PackedB {
    k: usize,
    n: usize,
    data: Vec<f32>,
}

impl PackedB {
    /// From `b`, `k × n` with rows `ld` apart.
    pub fn from_rows(b: &[f32], ld: usize, k: usize, n: usize) -> PackedB {
        Self::pack(k, n, |p, j| b[p * ld + j])
    }

    /// From the transpose of `w`, `n × k` with rows `ld` apart: a linear
    /// layer's weight (out × in) makes `x · wᵀ`.
    pub fn from_transposed(w: &[f32], ld: usize, n: usize, k: usize) -> PackedB {
        Self::pack(k, n, |p, j| w[j * ld + p])
    }

    fn pack(k: usize, n: usize, at: impl Fn(usize, usize) -> f32) -> PackedB {
        let panels = n.div_ceil(NR);
        let mut data = vec![0f32; panels * k * NR];
        for jp in 0..panels {
            for p in 0..k {
                let row = &mut data[(jp * k + p) * NR..][..NR];
                for (jj, v) in row.iter_mut().enumerate() {
                    let j = jp * NR + jj;
                    if j < n {
                        *v = at(p, j);
                    }
                }
            }
        }
        PackedB { k, n, data }
    }

    /// Rows (the shared dimension).
    pub fn k(&self) -> usize {
        self.k
    }

    /// Columns.
    pub fn n(&self) -> usize {
        self.n
    }

    /// The bytes it holds.
    pub fn bytes(&self) -> usize {
        self.data.len() * 4
    }
}

/// One register tile: `t += a · b` over `a`'s and `b`'s rows, `a` a packed
/// panel (`MR` values a step), `b` a packed panel (`NR` a step).
#[inline(always)]
fn tile(a: &[[f32; MR]], b: &[[f32; NR]], t: &mut [[f32; NR]; MR]) {
    let mut acc = *t;
    for (ap, bp) in a.iter().zip(b) {
        for i in 0..MR {
            for j in 0..NR {
                acc[i][j] = ap[i].mul_add(bp[j], acc[i][j]);
            }
        }
    }
    *t = acc;
}

kernel! {
    /// `c = a · b (+ bias)`: `a` is `m × b.k()` with rows `lda` apart,
    /// `c` is `m × b.n()` with rows `ldc` apart, `bias` (if given) is
    /// added to each row.  `pack` is scratch space, grown as needed.
    fn matmul / matmul_at(
        a: &[f32],
        lda: usize,
        m: usize,
        b: &PackedB,
        bias: Option<&[f32]>,
        c: &mut [f32],
        ldc: usize,
        pack: &mut Vec<[f32; MR]>
    ) {
        let (k, n) = (b.k, b.n);
        if m == 0 || n == 0 {
            return;
        }
        assert!(a.len() >= (m - 1) * lda + k && c.len() >= (m - 1) * ldc + n);
        assert!(bias.is_none_or(|b| b.len() >= n));
        let panels_b = n.div_ceil(NR);
        let bdata = b.data.as_chunks::<NR>().0;
        if k == 0 {
            for i in 0..m {
                for j in 0..n {
                    c[i * ldc + j] = bias.map_or(0.0, |b| b[j]);
                }
            }
            return;
        }
        for p0 in (0..k).step_by(KC) {
            let kc = KC.min(k - p0);
            let last = p0 + kc == k;
            for r0 in (0..m).step_by(MC) {
                let mc = MC.min(m - r0);
                let panels_a = mc.div_ceil(MR);
                // Pack the rows r0.. into panels of MR, zero past the end.
                pack.clear();
                pack.resize(panels_a * kc, [0.0; MR]);
                for ip in 0..panels_a {
                    for i in 0..MR.min(mc - ip * MR) {
                        let row = &a[(r0 + ip * MR + i) * lda + p0..][..kc];
                        for (p, &v) in row.iter().enumerate() {
                            pack[ip * kc + p][i] = v;
                        }
                    }
                }
                for jp in 0..panels_b {
                    let bp = &bdata[jp * k + p0..][..kc];
                    let cols = NR.min(n - jp * NR);
                    for ip in 0..panels_a {
                        let rows = MR.min(mc - ip * MR);
                        let mut t = [[0f32; NR]; MR];
                        if p0 > 0 {
                            for (i, ti) in t.iter_mut().enumerate().take(rows) {
                                let at = (r0 + ip * MR + i) * ldc + jp * NR;
                                ti[..cols].copy_from_slice(&c[at..at + cols]);
                            }
                        }
                        tile(&pack[ip * kc..][..kc], bp, &mut t);
                        for (i, ti) in t.iter().enumerate().take(rows) {
                            let at = (r0 + ip * MR + i) * ldc + jp * NR;
                            let out = &mut c[at..at + cols];
                            match bias {
                                Some(bias) if last => {
                                    for (j, o) in out.iter_mut().enumerate() {
                                        *o = ti[j] + bias[jp * NR + j];
                                    }
                                }
                                _ => out.copy_from_slice(&ti[..cols]),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `erf(x)` in single precision: Eigen's rational approximation (also
/// XLA's), odd numerator of degree 13 over even denominator of degree 8
/// on `x` clamped to ±4, beyond which `erf` is ±1 in `f32` — within
/// 5·10⁻⁷ of the true value, and branch free so it vectorizes.
#[inline(always)]
pub fn erf(x: f32) -> f32 {
    let x = x.clamp(-4.0, 4.0);
    let x2 = x * x;
    let mut p = (-2.726_142e-10_f32).mul_add(x2, 2.770_681_4e-8);
    p = p.mul_add(x2, -2.101_024e-6);
    p = p.mul_add(x2, -5.692_506_4e-5);
    p = p.mul_add(x2, -7.349_906e-4);
    p = p.mul_add(x2, -2.954_6e-3);
    p = p.mul_add(x2, -1.609_603_3e-2);
    let mut q = (-1.456_607_2e-5f32).mul_add(x2, -2.133_740_6e-4);
    q = q.mul_add(x2, -1.682_827e-3);
    q = q.mul_add(x2, -7.373_329e-3);
    q = q.mul_add(x2, -1.426_474e-2);
    x * p / q
}

/// `eˣ` in single precision: Cephes' `expf` — `x = n·ln 2 + r`, a degree-
/// six polynomial for `eʳ`, `2ⁿ` put in the exponent bits; within 10⁻⁷
/// relative, branch free, on `x` clamped to −87.3 … 88.3 (below, about
/// 10⁻³⁸, not zero).
#[inline(always)]
pub fn exp(x: f32) -> f32 {
    let x = x.clamp(-87.3, 88.3);
    let n = x.mul_add(std::f32::consts::LOG2_E, 0.5).floor();
    let r = n.mul_add(-0.693_359_4, x);
    let r = n.mul_add(2.121_944_4e-4, r);
    let mut p = 1.987_569_1e-4f32.mul_add(r, 1.398_199_9e-3);
    p = p.mul_add(r, 8.333_452e-3);
    p = p.mul_add(r, 4.166_579_6e-2);
    p = p.mul_add(r, 1.666_666_5e-1);
    p = p.mul_add(r, 0.5);
    let y = (p * r).mul_add(r, r) + 1.0;
    y * f32::from_bits(((n as i32 + 127) as u32) << 23)
}

kernel! {
    /// GELU in place, `x·Φ(x)` by `erf` — BERT's activation (exact, not
    /// the tanh approximation).
    fn gelu / gelu_at(x: &mut [f32]) {
        for v in x.iter_mut() {
            let t = *v;
            *v = 0.5 * t * (1.0 + erf(t * std::f32::consts::FRAC_1_SQRT_2));
        }
    }
}

/// Sums by `LANES` running sums, added in order: a fixed order the
/// compiler can vectorize.
#[inline(always)]
fn lane_sum(x: &[f32]) -> f32 {
    const L: usize = 16;
    let mut acc = [0f32; L];
    let (chunks, rest) = x.as_chunks::<L>();
    for ch in chunks {
        for l in 0..L {
            acc[l] += ch[l];
        }
    }
    for (l, &v) in rest.iter().enumerate() {
        acc[l] += v;
    }
    acc.iter().sum()
}

kernel! {
    /// Softmax of each row of `x` (`cols` wide, rows `ld` apart, `rows`
    /// of them) in place, the row scaled by `scale` first.
    fn softmax_rows / softmax_rows_at(x: &mut [f32], rows: usize, cols: usize, ld: usize, scale: f32) {
        for r in 0..rows {
            let row = &mut x[r * ld..r * ld + cols];
            let max = row.iter().fold(f32::NEG_INFINITY, |m, &v| m.max(v)) * scale;
            for v in row.iter_mut() {
                *v = exp(v.mul_add(scale, -max));
            }
            let inv = 1.0 / lane_sum(row);
            for v in row.iter_mut() {
                *v *= inv;
            }
        }
    }
}

kernel! {
    /// Layer normalization of each `cols`-wide row of `x` in place, rows
    /// `ld` apart: `(x − mean) / √(var + eps) · gamma + beta`.
    fn layer_norm / layer_norm_at(x: &mut [f32], rows: usize, cols: usize, ld: usize, gamma: &[f32], beta: &[f32], eps: f32) {
        let n = cols as f32;
        for r in 0..rows {
            let row = &mut x[r * ld..r * ld + cols];
            let mean = lane_sum(row) / n;
            let mut sq = [0f32; 16];
            let (chunks, rest) = row.as_chunks::<16>();
            for ch in chunks {
                for l in 0..16 {
                    let d = ch[l] - mean;
                    sq[l] = d.mul_add(d, sq[l]);
                }
            }
            for (l, &v) in rest.iter().enumerate() {
                let d = v - mean;
                sq[l] = d.mul_add(d, sq[l]);
            }
            let var = sq.iter().sum::<f32>() / n;
            let inv = 1.0 / (var + eps).sqrt();
            for ((v, &g), &b) in row.iter_mut().zip(gamma).zip(beta) {
                *v = ((*v - mean) * inv).mul_add(g, b);
            }
        }
    }
}

kernel! {
    /// `x += y`, element by element.
    fn add_assign / add_assign_at(x: &mut [f32], y: &[f32]) {
        for (a, &b) in x.iter_mut().zip(y) {
            *a += b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levels;

    fn naive(a: &[f32], m: usize, k: usize, b: &[f32], n: usize) -> Vec<f32> {
        let mut c = vec![0f32; m * n];
        for i in 0..m {
            for j in 0..n {
                let mut s = 0f32;
                for p in 0..k {
                    s = a[i * k + p].mul_add(b[p * n + j], s);
                }
                c[i * n + j] = s;
            }
        }
        c
    }

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = crate::mix(x.wrapping_add(0x9E37_79B9_7F4A_7C15));
                (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// Every shape's edge — rows and columns not a tile's multiple, a
    /// shared dimension past one block — exactly the naive product's
    /// chain of fused multiply-adds, at every level this CPU has.
    #[test]
    fn matmul_is_the_naive_chain_exactly() {
        for &(m, k, n) in &[
            (1, 1, 1),
            (7, 3, 5),
            (13, 300, 29),
            (MC + 5, KC * 2 + 7, NR * 3 + 1),
            (50, 384, 384),
        ] {
            let a = noise(m * k, 1);
            let b = noise(k * n, 2);
            let want = naive(&a, m, k, &b, n);
            let packed = PackedB::from_rows(&b, n, k, n);
            for l in levels() {
                let mut c = vec![f32::NAN; m * n];
                matmul_at(l, &a, k, m, &packed, None, &mut c, n, &mut Vec::new());
                assert_eq!(c, want, "{m}×{k}×{n} at {l:?}");
            }
            // Transposed packing, a bias, strides.
            let mut bt = vec![0f32; n * (k + 3)];
            for p in 0..k {
                for j in 0..n {
                    bt[j * (k + 3) + p] = b[p * n + j];
                }
            }
            let packed = PackedB::from_transposed(&bt, k + 3, n, k);
            let bias = noise(n, 3);
            let mut c = vec![0f32; m * (n + 2)];
            matmul(
                &a,
                k,
                m,
                &packed,
                Some(&bias),
                &mut c,
                n + 2,
                &mut Vec::new(),
            );
            for i in 0..m {
                for j in 0..n {
                    assert_eq!(c[i * (n + 2) + j], want[i * n + j] + bias[j]);
                }
            }
        }
    }

    #[test]
    fn functions_near_their_definitions() {
        for i in -2000..=2000 {
            let x = i as f32 / 250.0;
            let e = exp(x);
            assert!(
                ((e as f64) / (x as f64).exp() - 1.0).abs() < 2e-7,
                "exp({x})"
            );
        }
        assert!(exp(-1000.0) < 1e-37 && exp(1000.0).is_finite());
        // erf against a few known values (Abramowitz and Stegun, table 7.1).
        for (x, want) in [
            (0.0f32, 0.0f64),
            (0.5, 0.520_499_877_8),
            (1.0, 0.842_700_792_9),
            (2.0, 0.995_322_265_0),
            (-1.5, -0.966_105_146_5),
            (5.0, 1.0),
        ] {
            assert!((erf(x) as f64 - want).abs() < 6e-7, "erf({x})");
        }
        let mut x = vec![-3.0, -1.0, 0.0, 1.0, 3.0];
        gelu(&mut x);
        let want = [-0.004_049_7, -0.158_655_3, 0.0, 0.841_344_7, 2.995_950_3];
        for (g, w) in x.iter().zip(want) {
            assert!((g - w).abs() < 1e-6, "{g} {w}");
        }
        let mut s = vec![1.0, 2.0, 3.0, 9.0, 1.0, 1.0];
        softmax_rows(&mut s, 2, 2, 3, 1.0);
        assert!((s[0] - 0.268_941_4).abs() < 1e-6 && (s[1] - 0.731_058_6).abs() < 1e-6);
        assert_eq!(s[2], 3.0, "outside the row");
        assert!((s[3] - 0.999_664_6).abs() < 1e-6 && (s[4] - 3.353_501_3e-4).abs() < 1e-8);
        let mut x = vec![1.0, 2.0, 3.0, 4.0];
        layer_norm(&mut x, 1, 4, 4, &[1.0; 4], &[0.5; 4], 1e-12);
        assert!((x[0] - (0.5 - 1.341_640_8)).abs() < 1e-5, "{x:?}");
        for l in levels() {
            let mut y = vec![-3.0f32, 0.3, 2.0];
            let mut z = y.clone();
            gelu_at(l, &mut y);
            gelu_at(Level::Base, &mut z);
            assert_eq!(y, z, "the same at every level");
            let _ = level();
        }
    }
}
