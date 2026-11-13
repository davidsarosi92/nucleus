//! A local model's files (`nucleus-local-models`), which may come from
//! anywhere a `CREATE MODEL … WITH (url = …)` reaches: `tokenizer.json`
//! (and the texts it then cuts), `config.json` with `model.safetensors`
//! (the encoder built from them, and run once), and the pooling's
//! `config.json`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nucleus_local_models::encoder::{Config, Encoder, Pooling};
use nucleus_local_models::safetensors::SafeTensors;
use nucleus_local_models::tokenizer::Tokenizer;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let (a, b) = nucleus_fuzz::split(rest);
    match mode % 3 {
        0 => {
            if let Ok(t) = Tokenizer::from_json(a) {
                let text = String::from_utf8_lossy(b);
                let ids = t.encode(&text, 64);
                assert!(ids.len() <= 64, "{} tokens", ids.len());
                let _ = t.count(&text);
                let _ = t.encode("Hello, world! Naïve café 東京 🙂 ", 512);
            }
        }
        1 => {
            let (Ok(config), Ok(weights)) = (Config::from_json(a), SafeTensors::new(b)) else {
                return;
            };
            for name in weights.names() {
                let _ = weights.f32(name, &[]);
            }
            let pooling = if mode & 4 == 0 {
                Pooling::Cls
            } else {
                Pooling::Mean
            };
            if let Ok(e) = Encoder::new(config, pooling, &weights) {
                let n = e.config().vocab.min(e.config().max_tokens()).min(5) as u32;
                let text: Vec<u32> = (0..n).collect();
                let _ = e.encode(&[text.clone(), text], true, 1);
            }
        }
        _ => {
            let _ = Pooling::from_json(a);
        }
    }
});
