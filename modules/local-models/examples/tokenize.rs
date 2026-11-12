//! Tokenizes texts as `bench/embed/tokens.py` compares them with
//! Hugging Face's `tokenizers`: `tokenize TOKENIZER.json < texts.jsonl`,
//! one JSON string a line in, its ids as a JSON array a line out.

use std::io::{BufRead, Write};

use nucleus_local_models::tokenizer::Tokenizer;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("tokenize TOKENIZER.json < texts.jsonl");
    let t = Tokenizer::from_json(&std::fs::read(&path).expect("reading the tokenizer"))
        .unwrap_or_else(|e| panic!("{e}"));
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    for line in std::io::stdin().lock().lines() {
        let text: String = serde_json::from_str(&line.expect("reading")).expect("a JSON string");
        let ids = t.encode(&text, usize::MAX);
        serde_json::to_writer(&mut out, &ids).expect("writing");
        out.write_all(b"\n").expect("writing");
    }
}
