//! What sealing a page costs (PLAN.md §M14.30 K2a): an 8 KiB page through
//! CRC-32C (today's checksum), ChaCha20-Poly1305 (RFC 8439) and the
//! misuse-resistant `nucleus_crypto::siv`, in MB/s on one core.
//!
//! `cargo test --release -p nucleus-storage --test page_crypto_speed -- --ignored --nocapture`

use std::time::Instant;

use nucleus_crypto::{aead, siv};
use nucleus_storage::crc32c::crc32c;

const PAGE: usize = 8192;
const PAGES: usize = 4096; // 32 MiB a pass

fn rate(name: &str, mut f: impl FnMut(&mut [u8], u64)) {
    let mut page: Vec<u8> = (0..PAGE).map(|i| (i * 131 + 17) as u8).collect();
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        for n in 0..PAGES as u64 {
            f(&mut page, n);
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    let mb = (PAGE * PAGES) as f64 / 1e6;
    println!(
        "{name:28} {:8.0} MB/s  {:6.2} µs a page",
        mb / best,
        best * 1e6 / PAGES as f64
    );
}

#[test]
#[ignore = "a measurement: run it in release"]
fn page_sealing_speed() {
    let key = [0x42u8; 32];
    let mut sink = 0u64;
    rate("crc32c", |p, _| sink ^= u64::from(crc32c(p)));
    rate("chacha20-poly1305 seal", |p, n| {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&n.to_le_bytes());
        let s = aead::seal(&key, &nonce, &n.to_le_bytes(), p);
        sink ^= u64::from(s[0]);
    });
    rate("siv seal (in place)", |p, n| {
        let t = siv::seal_in_place(&key, &[0u8; 12], &n.to_le_bytes(), p);
        sink ^= u64::from(t[0]);
    });
    rate("siv seal + open", |p, n| {
        let aad = n.to_le_bytes();
        let t = siv::seal_in_place(&key, &[0u8; 12], &aad, p);
        assert!(siv::open_in_place(&key, &[0u8; 12], &aad, p, &t));
    });
    println!("({sink})");
}
