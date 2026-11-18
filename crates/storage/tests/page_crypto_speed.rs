//! What sealing a page costs (PLAN.md §M14.30 K2a): an 8 KiB page through
//! CRC-32C (today's checksum), ChaCha20-Poly1305 (RFC 8439), the
//! misuse-resistant `nucleus_crypto::siv` and AES-256-GCM-SIV by the CPU's
//! instructions (`nucleus_kernels::aes`), in MB/s on one core; and what
//! it costs a store (K2b): a load checkpointed, then a scan through a
//! cold buffer pool, in the clear and encrypted with each cipher.
//!
//! `cargo test --release -p nucleus-storage --test page_crypto_speed -- --ignored --nocapture`

use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use nucleus_crypto::{aead, siv};
use nucleus_kernels::aes;
use nucleus_platform::Platform;
use nucleus_platform::real::RealPlatform;
use nucleus_storage::cipher::{CIPHER_AES256_GCM_SIV, CIPHER_CHACHA20_SIV};
use nucleus_storage::crc32c::crc32c;
use nucleus_storage::{Batch, PageCipher, Store, StoreOptions};

const PAGE: usize = 8192;
const PAGES: usize = 4096; // 32 MiB a pass

fn rate(name: &str, mut f: impl FnMut(&mut [u8], u64)) {
    let mut page: Vec<u8> = (0..PAGE).map(|i| (i * 131 + 17) as u8).collect();
    let mut times = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        for n in 0..PAGES as u64 {
            f(&mut page, n);
        }
        times.push(t.elapsed().as_secs_f64());
    }
    times.sort_by(f64::total_cmp);
    let best = times[1];
    let mb = (PAGE * PAGES) as f64 / 1e6;
    metric(&format!("{name}: MB/s"), &[mb / best], "MB/s");
    metric(
        &format!("{name}: µs a page"),
        &[best * 1e6 / PAGES as f64],
        "µs",
    );
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
    if aes::hardware() {
        let k = aes::Key::new(&key);
        rate("aes-gcm-siv seal (in place)", |p, n| {
            let t = aes::seal_in_place(&k, &[0u8; 12], &n.to_le_bytes(), p);
            sink ^= u64::from(t[0]);
        });
        rate("aes-gcm-siv seal + open", |p, n| {
            let aad = n.to_le_bytes();
            let t = aes::seal_in_place(&k, &[0u8; 12], &aad, p);
            assert!(aes::open_in_place(&k, &[0u8; 12], &aad, p, &t));
        });
    }
    println!("({sink})");
}

/// A load of `ROWS` rows (100-byte values) in batches, checkpointed, and
/// a scan of them all from a buffer pool too small to hold them, on the
/// real file system: in the clear, then encrypted.
#[test]
#[ignore = "a measurement: run it in release"]
fn store_load_and_scan_encrypted() {
    const ROWS: u64 = 400_000;
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("page_crypto_store");
    let key = PageCipher::new([9; 32]);
    for key in [
        None,
        Some(key.for_code(CIPHER_CHACHA20_SIV).unwrap()),
        Some(key.for_code(CIPHER_AES256_GCM_SIV).unwrap()),
    ] {
        let name = match key.as_ref().map(PageCipher::code) {
            None => "in the clear",
            Some(CIPHER_CHACHA20_SIV) => "chacha20-siv",
            Some(_) => "aes-gcm-siv",
        };
        let dir = root.join(name.replace(' ', "-"));
        let _ = std::fs::remove_dir_all(&dir);
        let platform: Arc<dyn Platform> = Arc::new(RealPlatform::new().unwrap());
        let opts = StoreOptions {
            cache_pages: 1024,
            wal_segment_size: 64 << 20,
            page_key: key,
            previous_page_key: None,
            compaction_bytes_per_sec: 0,
        };
        let t = Instant::now();
        {
            let (store, _) = Store::open(Arc::clone(&platform), &dir, &opts).unwrap();
            for b in 0..ROWS / 1000 {
                let ops = (b * 1000..(b + 1) * 1000)
                    .map(|i| {
                        let v: Vec<u8> = (0..100).map(|j| (i * 31 + j * 7) as u8).collect();
                        (format!("key{i:010}").into_bytes(), Some(v))
                    })
                    .collect();
                store.write(&Batch { meta: b + 1, ops }).unwrap();
            }
            store.checkpoint().unwrap();
        }
        let load = t.elapsed();
        let (store, _) = Store::open(Arc::clone(&platform), &dir, &opts).unwrap();
        let mut times = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            let n = store
                .read()
                .unwrap()
                .range(Bound::Unbounded, Bound::Unbounded)
                .unwrap()
                .count();
            assert_eq!(n as u64, ROWS);
            times.push(t.elapsed().as_secs_f64());
        }
        times.sort_by(f64::total_cmp);
        let best = times[1];
        let size = std::fs::metadata(dir.join("data")).unwrap().len();
        metric(
            &format!("{name}: load + checkpoint"),
            &[load.as_secs_f64() * 1e3],
            "ms",
        );
        metric(&format!("{name}: scan"), &[best * 1e3], "ms");
        println!(
            "{name:14} load + checkpoint {:7.1} ms   scan {:7.1} ms   ({} MB)",
            load.as_secs_f64() * 1e3,
            best * 1e3,
            size >> 20
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Numbers on stdout as `bench/method.py run` gathers them (PLAN.md
/// §M14.31: the test run again, each number with its spread).
fn metric(name: &str, values: &[f64], unit: &str) {
    let values: Vec<String> = values.iter().map(f64::to_string).collect();
    println!(
        "@metric {{\"name\": {name:?}, \"values\": [{}], \"unit\": {unit:?}, \
         \"system\": \"nucleus\", \"phase\": \"warm\"}}",
        values.join(", ")
    );
}
