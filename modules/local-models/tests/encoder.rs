//! The encoder on the models' own files (`bench/embed/models.py` fetches
//! them; without them a test says so and passes): vectors near ONNX
//! Runtime's on the publishers' f32 exports (`bench/embed/encoder.py`
//! compares a corpus), and each text's vector the same bits alone, in a
//! batch and on any number of threads.

use std::path::{Path, PathBuf};

use nucleus_local_models::{Files, LocalModel};

fn models() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("NUCLEUS_MODELS") {
        return Some(PathBuf::from(d));
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join("target").join("models"))
        .find(|d| d.is_dir())
}

fn model(name: &str) -> Option<LocalModel> {
    let dir = models()?.join(name);
    let read = |p: &str| std::fs::read(dir.join(p)).ok();
    let (Some(tokenizer), Some(config), Some(weights)) = (
        read("tokenizer.json"),
        read("config.json"),
        read("model.safetensors"),
    ) else {
        eprintln!("skipped: no {}", dir.display());
        return None;
    };
    let pooling = read("1_Pooling/config.json");
    Some(
        LocalModel::new(
            &Files {
                tokenizer: &tokenizer,
                config: &config,
                pooling: pooling.as_deref(),
                weights: &weights,
            },
            None,
        )
        .unwrap(),
    )
}

const TEXTS: [&str; 2] = [
    "A database that embeds its own text.",
    "query: Hány éves a Duna?",
];

/// The first six components, as ONNX Runtime gives them.
fn check(m: &LocalModel, want: [[f32; 6]; 2]) {
    let v = m.embed(&TEXTS, 1).unwrap();
    for (got, want) in v.iter().zip(want) {
        assert_eq!(got.len(), 384);
        let norm: f32 = got.iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-5);
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 2e-6, "{g} {w}: {:?}", &got[..6]);
        }
    }
}

/// A text's vector alone, among others, on one thread and on several.
fn independent(m: &LocalModel) {
    let mut texts: Vec<String> = (0..40)
        .map(|i| format!("text number {i} {}", "word ".repeat(i * 3)))
        .collect();
    texts.push(TEXTS[1].into());
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let one = m.embed(&refs, 1).unwrap();
    let many = m.embed(&refs, 5).unwrap();
    let alone = m.embed(&[TEXTS[1]], 1).unwrap();
    assert_eq!(one, many, "the threads change nothing");
    assert_eq!(one[40], alone[0], "nor the batch");
}

#[test]
fn bge_small_en() {
    let Some(m) = model("bge-small-en-v1.5") else {
        return;
    };
    check(
        &m,
        [
            [
                -0.026365, -0.016722, -0.011307, -0.007774, -0.024608, -0.018717,
            ],
            [
                -0.043115, 0.016197, -0.041132, 0.019801, -0.03606, -0.011844,
            ],
        ],
    );
    independent(&m);
}

#[test]
fn multilingual_e5_small() {
    let Some(m) = model("multilingual-e5-small") else {
        return;
    };
    check(
        &m,
        [
            [
                0.035065, -0.03935, -0.028944, -0.111893, 0.077296, -0.002183,
            ],
            [
                0.027035, -0.047199, -0.050779, -0.03137, 0.066949, -0.048816,
            ],
        ],
    );
    independent(&m);
    // The longest text is cut to the model's 512 tokens, not refused.
    let long = "szó ".repeat(2000);
    assert_eq!(m.tokens(&long).len(), 512);
    m.embed(&[&long], 1).unwrap();
}
