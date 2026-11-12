//! Embeds texts as `bench/embed/encoder.py` compares them with ONNX
//! Runtime: `embed MODEL_DIR [THREADS] < texts.jsonl > vectors.f32`, one
//! JSON string a line in, the vectors out as little-endian `f32`s, one
//! after another; the time taken to stderr.

use std::io::{BufRead, Write};
use std::time::Instant;

use nucleus_local_models::{Files, LocalModel};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("embed MODEL_DIR [THREADS]"));
    let threads: usize = args.next().map_or(1, |t| t.parse().expect("THREADS"));
    let read = |p: &str| std::fs::read(dir.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    let t = Instant::now();
    let (tokenizer, config, weights) = (
        read("tokenizer.json"),
        read("config.json"),
        read("model.safetensors"),
    );
    let pooling = std::fs::read(dir.join("1_Pooling/config.json")).ok();
    let model = LocalModel::new(
        &Files {
            tokenizer: &tokenizer,
            config: &config,
            pooling: pooling.as_deref(),
            weights: &weights,
        },
        None,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    drop(weights);
    eprintln!("loaded in {:.2} s", t.elapsed().as_secs_f64());
    let texts: Vec<String> = std::io::stdin()
        .lock()
        .lines()
        .map(|l| serde_json::from_str(&l.expect("reading")).expect("a JSON string"))
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let t = Instant::now();
    let ids: Vec<Vec<u32>> = refs.iter().map(|t| model.tokens(t)).collect();
    let tokens: usize = ids.iter().map(Vec::len).sum();
    let tok = t.elapsed().as_secs_f64();
    let vectors = model
        .encoder()
        .encode(&ids, true, threads)
        .unwrap_or_else(|e| panic!("{e}"));
    let s = t.elapsed().as_secs_f64();
    eprintln!(
        "{} texts, {tokens} tokens in {s:.2} s ({tok:.2} s tokenizing) on {threads} threads: {:.0} texts/s, {:.0} tokens/s",
        texts.len(),
        texts.len() as f64 / s,
        tokens as f64 / s
    );
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for v in vectors {
        for x in v {
            out.write_all(&x.to_le_bytes()).expect("writing");
        }
    }
}
