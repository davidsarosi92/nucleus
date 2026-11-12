//! The f32 matrix product's speed on one core: `matmul [M K N ...]`,
//! GFLOP/s for each shape (BERT-small's by default).

use std::time::Instant;

use nucleus_kernels::{PackedB, matmul};

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| a.parse().unwrap())
        .collect();
    let shapes: Vec<(usize, usize, usize)> = if args.is_empty() {
        vec![
            (512, 384, 384),
            (512, 384, 1536),
            (512, 1536, 384),
            (128, 32, 128),
            (2048, 384, 1152),
        ]
    } else {
        args.chunks(3).map(|c| (c[0], c[1], c[2])).collect()
    };
    for (m, k, n) in shapes {
        let a: Vec<f32> = (0..m * k).map(|i| (i % 7) as f32 * 0.1).collect();
        let b: Vec<f32> = (0..k * n).map(|i| (i % 5) as f32 * 0.1).collect();
        let packed = PackedB::from_rows(&b, n, k, n);
        let mut c = vec![0f32; m * n];
        let mut pack = Vec::new();
        let flops = 2.0 * (m * k * n) as f64;
        let reps = ((2e10 / flops) as usize).max(3);
        matmul(&a, k, m, &packed, None, &mut c, n, &mut pack);
        let t = Instant::now();
        for _ in 0..reps {
            matmul(&a, k, m, &packed, None, &mut c, n, &mut pack);
        }
        let s = t.elapsed().as_secs_f64() / reps as f64;
        println!("{m:5} × {k:5} × {n:5}: {:7.1} GFLOP/s", flops / s / 1e9);
    }
}
