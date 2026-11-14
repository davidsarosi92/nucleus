//! JSON measured (PLAN.md §M14.15): parse, output and one field's access
//! by nucleus's binary form and by serde_json's `Value`, on files given as
//! arguments.  Each figure is the median of `ROUNDS` timed runs.
//!
//!     cargo run --release -- canada.json citm_catalog.json twitter.json
//!
//! `--profile FILE` prints ten seconds of output alone, for a sampler
//! (`sample`, `perf`).

use std::time::Instant;

use nucleus_sql::json::{form, parse, to_text};

const ROUNDS: usize = 15;

fn median(mut f: impl FnMut()) -> f64 {
    f();
    let mut t: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_secs_f64()
        })
        .collect();
    t.sort_by(f64::total_cmp);
    t[ROUNDS / 2]
}

fn main() {
    // `--profile FILE`: ten seconds of output alone, for a sampler.
    if std::env::args().nth(1).as_deref() == Some("--profile") {
        let text = std::fs::read_to_string(std::env::args().nth(2).expect("a file")).unwrap();
        let b = parse::parse(&text).unwrap();
        let s = Instant::now();
        while s.elapsed().as_secs() < 10 {
            std::hint::black_box(to_text(std::hint::black_box(&b)).unwrap());
        }
        return;
    }
    println!(
        "{:<18} {:>8} {:>8} | {:>9} {:>9} | {:>9} {:>9} | {:>10} {:>10}",
        "file", "text KB", "form KB", "parse", "serde", "output", "serde", "field µs", "serde µs"
    );
    for path in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&path).expect("readable");
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mb = text.len() as f64 / 1e6;
        let b = parse::parse(&text).expect("parses");
        let sv: serde_json::Value = serde_json::from_str(&text).expect("serde parses");
        let parse = median(|| {
            std::hint::black_box(parse::parse(std::hint::black_box(&text)).unwrap());
        });
        let serde = median(|| {
            std::hint::black_box(serde_json::from_str::<serde_json::Value>(&text).unwrap());
        });
        let out = median(|| {
            std::hint::black_box(to_text(&b).unwrap());
        });
        let serde_out = median(|| {
            std::hint::black_box(serde_json::to_string(&sv).unwrap());
        });
        // One field of the top object (its last key), as `doc ->> 'k'`
        // reads it: from the stored form, or by parsing the text.
        let key = match form::root(&b).unwrap() {
            form::Node::Object(o) => o.key(o.len() - 1).unwrap().to_string(),
            _ => String::new(),
        };
        let field = median(|| {
            for _ in 0..1000 {
                let form::Node::Object(o) = form::root(std::hint::black_box(&b)).unwrap() else {
                    panic!()
                };
                std::hint::black_box(o.get_bytes(&key).unwrap());
            }
        }) / 1000.0;
        let serde_field = median(|| {
            let v: serde_json::Value = serde_json::from_str(std::hint::black_box(&text)).unwrap();
            std::hint::black_box(v.get(&key).is_some());
        });
        println!(
            "{:<18} {:>8.0} {:>8.0} | {:>6.0} MB/s {:>6.0} MB/s | {:>6.0} MB/s {:>6.0} MB/s | {:>10.3} {:>10.0}",
            name,
            text.len() as f64 / 1024.0,
            b.len() as f64 / 1024.0,
            mb / parse,
            mb / serde,
            mb / out,
            mb / serde_out,
            field * 1e6,
            serde_field * 1e6,
        );
    }
}
