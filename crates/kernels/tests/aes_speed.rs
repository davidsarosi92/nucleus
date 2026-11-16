//! What AES-256-GCM-SIV costs (PLAN.md §M14.30, the speed after K2):
//! sealing and opening messages of a log record's, a scratch block's, a
//! page's and a larger length, by the CPU's instructions — and a page by
//! the portable copy.
//!
//! `cargo test --release -p nucleus-kernels --test aes_speed -- --ignored --nocapture`

use std::time::Instant;

use nucleus_kernels::aes;

#[test]
#[ignore = "a measurement: run it in release"]
fn aes_gcm_siv_speed() {
    let key = aes::Key::new(&[7u8; 32]);
    let nonce = [1u8; 12];
    for (hw, len) in [
        (true, 64),
        (true, 4096),
        (true, 8168),
        (true, 65536),
        (false, 8168),
    ] {
        if hw && !aes::hardware() {
            continue;
        }
        let mut data = vec![0x5Au8; len];
        let rounds = if hw { (64 << 20) / (len + 256) } else { 100 };
        let mut best = (f64::MAX, f64::MAX);
        for _ in 0..3 {
            let t = Instant::now();
            let mut tag = [0u8; 16];
            for i in 0..rounds {
                let mut n = nonce;
                n[..8].copy_from_slice(&(i as u64).to_le_bytes());
                tag = aes::seal_with(hw, &key, &n, b"aad", &mut data);
            }
            let seal = t.elapsed().as_secs_f64() / rounds as f64;
            let mut n = nonce;
            n[..8].copy_from_slice(&((rounds - 1) as u64).to_le_bytes());
            let t = Instant::now();
            for _ in 0..rounds {
                assert!(aes::open_with(hw, &key, &n, b"aad", &mut data, &tag));
                aes::seal_with(hw, &key, &n, b"aad", &mut data);
            }
            let both = t.elapsed().as_secs_f64() / rounds as f64;
            best = (best.0.min(seal), best.1.min(both - seal));
        }
        println!(
            "{:12} {len:6} bytes   seal {:8.3} µs ({:5.0} MB/s)   open {:8.3} µs",
            if hw { "instructions" } else { "portable" },
            best.0 * 1e6,
            len as f64 / best.0 / 1e6,
            best.1 * 1e6
        );
    }
}
