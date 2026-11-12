//! The tokenizers on the models' own `tokenizer.json`, against ids
//! Hugging Face's `tokenizers` gives (`bench/embed/tokens.py` compares
//! whole corpora).  The files are not in the repository: they are looked
//! for in `$NUCLEUS_MODELS` or the repository's `target/models` (`bench/embed/models.py`
//! fetches them), and a test without them says so and passes.

use std::path::PathBuf;

use nucleus_local_models::tokenizer::Tokenizer;

/// `$NUCLEUS_MODELS`, else the nearest `target/models` above the crate.
fn models() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("NUCLEUS_MODELS") {
        return Some(PathBuf::from(d));
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|a| a.join("target").join("models"))
        .find(|d| d.is_dir())
}

fn tokenizer(model: &str) -> Option<Tokenizer> {
    let Some(dir) = models() else {
        eprintln!("skipped: no target/models (bench/embed/models.py)");
        return None;
    };
    let path = dir.join(model).join("tokenizer.json");
    match std::fs::read(&path) {
        Ok(bytes) => Some(Tokenizer::from_json(&bytes).unwrap()),
        Err(_) => {
            eprintln!("skipped: no {}", path.display());
            None
        }
    }
}

const TEXTS: [&str; 2] = [
    "Árvíztűrő tükörfúrógép: a [SEP] test, naïve café!",
    "query: Mennyi az idő?   中文 👩\u{200D}🔬 ﬁne",
];

#[test]
fn bge_small_en() {
    let Some(t) = tokenizer("bge-small-en-v1.5") else {
        return;
    };
    assert_eq!(
        t.encode(TEXTS[0], 512),
        [
            101, 12098, 5737, 2480, 20689, 2080, 10722, 21815, 27942, 23884, 2361, 1024, 1037, 102,
            3231, 1010, 15743, 7668, 999, 102
        ],
        "accents stripped, a special token in the text is that token"
    );
    assert_eq!(
        t.encode(TEXTS[1], 512),
        [
            101, 23032, 1024, 2273, 4890, 2072, 17207, 8909, 2080, 1029, 1746, 1861, 100, 1984,
            2638, 102
        ]
    );
    assert_eq!(
        t.encode(TEXTS[0], 8),
        [101, 12098, 5737, 2480, 20689, 2080, 10722, 102],
        "cut to eight, [SEP] kept"
    );
}

#[test]
fn multilingual_e5_small() {
    let Some(t) = tokenizer("multilingual-e5-small") else {
        return;
    };
    assert_eq!(
        t.encode(TEXTS[0], 512),
        [
            0, 24133, 62579, 18, 70017, 2983, 221997, 420, 12595, 209360, 12, 10, 378, 294, 21290,
            268, 3034, 4, 24, 9392, 272, 26216, 38, 2
        ]
    );
    assert_eq!(
        t.encode(TEXTS[1], 512),
        [
            0, 41, 1294, 12, 1111, 14916, 295, 14960, 32, 6, 32095, 6, 244785, 6, 245928, 5885, 2
        ]
    );
    assert_eq!(
        t.encode(TEXTS[0], 8),
        [0, 24133, 62579, 18, 70017, 2983, 221997, 2]
    );
}

#[test]
fn what_is_not_supported_is_refused() {
    for (json, what) in [
        (
            r#"{"pre_tokenizer": {"type": "BertPreTokenizer"}, "model": {"type": "BPE"}}"#,
            "BPE",
        ),
        (
            r#"{"normalizer": {"type": "NFKC"}, "model": {"type": "WordPiece", "vocab": {"[UNK]": 0}}}"#,
            "NFKC",
        ),
        (
            r#"{"added_tokens": [{"id": 0, "content": "<x>", "lstrip": true}]}"#,
            "<x>",
        ),
        ("not json", "tokenizer.json"),
    ] {
        let e = Tokenizer::from_json(json.as_bytes()).err().unwrap();
        assert!(e.contains(what), "{e}");
    }
}
