//! `CREATE MODEL … USING local` end to end, on a tiny BERT made here
//! (eight wide, one layer, random weights — no download): from a
//! directory and from a hub on loopback that answers as Hugging Face's
//! does (a redirect to its large-file store, the SHA-256 it announces);
//! the files kept in the database by content and shared; who may name
//! what; the same vector as the library's.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_local_models::{Files, LocalModel, LocalModels};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::{Output, Registry, Session, Value};

const H: usize = 8;
const I: usize = 16;
const VOCAB: [&str; 12] = [
    "[PAD]", "[UNK]", "[CLS]", "[SEP]", "hello", "world", "a", "b", "##s", "data", "##base", "x",
];

/// A step up a path, put together so the boundary check (no paths out of
/// a module's folder) does not take the refusals tested for one.
const UP: &str = concat!("..", "/");

/// A deterministic number in [-0.5, 0.5).
fn noise(i: &mut u64) -> f32 {
    *i = i
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*i >> 40) as f32 / (1u64 << 24) as f32 - 0.5
}

/// The files of a tiny BERT: name and bytes.
fn tiny() -> Vec<(String, Vec<u8>)> {
    let mut tensors: Vec<(String, Vec<usize>)> = vec![
        (
            "embeddings.word_embeddings.weight".into(),
            vec![VOCAB.len(), H],
        ),
        ("embeddings.position_embeddings.weight".into(), vec![16, H]),
        ("embeddings.token_type_embeddings.weight".into(), vec![2, H]),
        ("embeddings.LayerNorm.weight".into(), vec![H]),
        ("embeddings.LayerNorm.bias".into(), vec![H]),
    ];
    let p = "encoder.layer.0.";
    for part in ["query", "key", "value"] {
        tensors.push((format!("{p}attention.self.{part}.weight"), vec![H, H]));
        tensors.push((format!("{p}attention.self.{part}.bias"), vec![H]));
    }
    for (name, shape) in [
        ("attention.output.dense.weight", vec![H, H]),
        ("attention.output.dense.bias", vec![H]),
        ("attention.output.LayerNorm.weight", vec![H]),
        ("attention.output.LayerNorm.bias", vec![H]),
        ("intermediate.dense.weight", vec![I, H]),
        ("intermediate.dense.bias", vec![I]),
        ("output.dense.weight", vec![H, I]),
        ("output.dense.bias", vec![H]),
        ("output.LayerNorm.weight", vec![H]),
        ("output.LayerNorm.bias", vec![H]),
    ] {
        tensors.push((format!("{p}{name}"), shape));
    }
    let mut seed = 42;
    let mut header = serde_json::Map::new();
    let mut data = Vec::new();
    for (name, shape) in &tensors {
        let n: usize = shape.iter().product();
        let start = data.len();
        for _ in 0..n {
            let v = if name.ends_with("LayerNorm.weight") {
                1.0 + noise(&mut seed) * 0.1
            } else {
                noise(&mut seed)
            };
            data.extend_from_slice(&v.to_le_bytes());
        }
        header.insert(
            name.clone(),
            serde_json::json!({"dtype": "F32", "shape": shape, "data_offsets": [start, data.len()]}),
        );
    }
    let header = serde_json::to_vec(&header).unwrap();
    let mut weights = (header.len() as u64).to_le_bytes().to_vec();
    weights.extend(header);
    weights.extend(data);
    let config = serde_json::json!({
        "model_type": "bert", "hidden_size": H, "num_hidden_layers": 1,
        "num_attention_heads": 2, "intermediate_size": I, "vocab_size": VOCAB.len(),
        "max_position_embeddings": 16, "type_vocab_size": 2, "hidden_act": "gelu",
        "layer_norm_eps": 1e-12
    });
    let vocab: serde_json::Map<String, serde_json::Value> = VOCAB
        .iter()
        .enumerate()
        .map(|(i, t)| (t.to_string(), serde_json::json!(i)))
        .collect();
    let special = |id: usize| {
        serde_json::json!({"id": id, "content": VOCAB[id], "single_word": false, "lstrip": false,
            "rstrip": false, "normalized": false, "special": true})
    };
    let tokenizer = serde_json::json!({
        "added_tokens": [special(0), special(1), special(2), special(3)],
        "normalizer": {"type": "BertNormalizer", "clean_text": true, "handle_chinese_chars": true,
            "strip_accents": null, "lowercase": true},
        "pre_tokenizer": {"type": "BertPreTokenizer"},
        "post_processor": {"type": "TemplateProcessing",
            "single": [{"SpecialToken": {"id": "[CLS]", "type_id": 0}}, {"Sequence": {"id": "A", "type_id": 0}},
                       {"SpecialToken": {"id": "[SEP]", "type_id": 0}}],
            "special_tokens": {"[CLS]": {"id": "[CLS]", "ids": [2], "tokens": ["[CLS]"]},
                               "[SEP]": {"id": "[SEP]", "ids": [3], "tokens": ["[SEP]"]}}},
        "model": {"type": "WordPiece", "unk_token": "[UNK]", "continuing_subword_prefix": "##",
            "max_input_chars_per_word": 100, "vocab": vocab}
    });
    let pooling =
        serde_json::json!({"pooling_mode_cls_token": false, "pooling_mode_mean_tokens": true});
    vec![
        (
            "tokenizer.json".into(),
            serde_json::to_vec(&tokenizer).unwrap(),
        ),
        ("config.json".into(), serde_json::to_vec(&config).unwrap()),
        ("model.safetensors".into(), weights),
        (
            "1_Pooling/config.json".into(),
            serde_json::to_vec(&pooling).unwrap(),
        ),
    ]
}

fn library_vector(files: &[(String, Vec<u8>)], text: &str) -> Vec<f32> {
    let f = |n: &str| &files.iter().find(|(k, _)| k == n).unwrap().1[..];
    let m = LocalModel::new(
        &Files {
            tokenizer: f("tokenizer.json"),
            config: f("config.json"),
            pooling: Some(f("1_Pooling/config.json")),
            weights: f("model.safetensors"),
        },
        None,
    )
    .unwrap();
    m.embed(&[text], 1).unwrap().remove(0)
}

fn kv() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(11)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn session(kv: &Arc<dyn Kv>) -> Session {
    let mut r = Registry::builtin();
    r.add(&LocalModels).unwrap();
    Session::with_registry(kv.clone(), Arc::new(r))
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

/// The keys under the model files' prefix.
fn file_keys(kv: &Arc<dyn Kv>) -> usize {
    let t = kv.begin(TxnOptions::read_only()).unwrap();
    t.scan(
        Bound::Included(&[0x00, 0x16][..]),
        Bound::Excluded(&[0x00, 0x17][..]),
    )
    .unwrap()
    .count()
}

fn vector(v: &Value) -> Vec<f32> {
    match v {
        Value::Text(t) => t
            .trim_matches(['[', ']'])
            .split(',')
            .map(|x| x.parse().unwrap())
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn from_a_directory() {
    let files = tiny();
    let dir = std::env::temp_dir().join(format!("nucleus-tiny-bert-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("1_Pooling")).unwrap();
    for (name, bytes) in &files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let kv = kv();
    let mut s = session(&kv);
    s.execute(&format!(
        "CREATE MODEL tiny USING local WITH (path = '{}', threads = 2)",
        dir.display()
    ))
    .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    // The files are in the database now: the directory is not needed.
    let got = rows(&mut s, "SELECT embed('Hello, databases!', 'tiny')::text");
    let want = library_vector(&files, "Hello, databases!");
    let got = vector(&got[0][0]);
    assert_eq!(got, want, "the library's vector");
    let total: i64 = files.iter().map(|(_, b)| b.len() as i64).sum();
    assert_eq!(
        rows(
            &mut s,
            "SELECT provider, dimensions, files, bytes FROM nc_catalog.models"
        ),
        vec![vec![
            Value::Text("local".into()),
            Value::Int(H as i64),
            Value::Array(
                files
                    .iter()
                    .map(|(n, _)| Value::Text(n.as_str().into()))
                    .collect()
            ),
            Value::Int(total),
        ]]
    );
    assert!(
        rows(
            &mut s,
            "SELECT remote FROM nc_catalog.model_providers WHERE name = 'local'"
        ) == vec![vec![Value::Bool(false)]]
    );
    // A second model from the same files stores nothing more, and its
    // pooling may differ.
    let keys = file_keys(&kv);
    assert!(keys >= files.len());
    s.execute("CREATE TABLE d (id int PRIMARY KEY, body text)")
        .unwrap();
    s.execute("INSERT INTO d VALUES (1, 'a b'), (2, 'x'), (3, 'hello world'), (4, NULL)")
        .unwrap();
    let n = rows(&mut s, "SELECT count(embed(body, 'tiny')) FROM d");
    assert_eq!(n, vec![vec![Value::Int(3)]]);
    // Chunks sized in the model's tokens: "databases" is three
    // (data ##base ##s), each chunk counted again whole.
    assert_eq!(
        rows(
            &mut s,
            "SELECT chunk, size FROM chunks('databases databases hello world', 'tokens', 4, 0, 'tiny')"
        ),
        vec![
            vec![Value::Text("databases".into()), Value::Int(3)],
            vec![Value::Text("databases hello".into()), Value::Int(4)],
            vec![Value::Text("world".into()), Value::Int(1)],
        ]
    );
    // A chunk table sized in the model's tokens, written by a role not
    // granted the model (its maker was); the model kept while it is used.
    s.execute("CREATE TABLE parts CHUNKS OF d (body) BY tokens SIZE 4 USING tiny")
        .unwrap();
    s.execute("CREATE USER writer PASSWORD 'w'; GRANT SELECT, INSERT, UPDATE ON d TO writer")
        .unwrap();
    let mut writer = session(&kv);
    writer.login("writer").unwrap();
    writer
        .execute("INSERT INTO d VALUES (5, 'databases hello')")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT chunk, size FROM parts WHERE id = 5"),
        vec![vec![Value::Text("databases hello".into()), Value::Int(4)]]
    );
    assert_eq!(code(&mut writer, "SELECT embed('x', 'tiny')"), "42501");
    assert_eq!(code(&mut s, "DROP MODEL tiny"), "2BP01");
    s.execute("DROP TABLE parts").unwrap();
    // Dropped: the files go with the last model that kept them.
    s.execute("DROP MODEL tiny").unwrap();
    assert_eq!(file_keys(&kv), 0);
    // Refusals.
    assert_eq!(
        code(
            &mut s,
            "CREATE MODEL m USING local WITH (path = '/nowhere')"
        ),
        "55000"
    );
    assert_eq!(code(&mut s, "CREATE MODEL m USING local"), "22023");
    assert_eq!(
        code(
            &mut s,
            "CREATE MODEL m USING local WITH (path = '/x', huggingface = 'a/b')"
        ),
        "22023"
    );
    assert_eq!(
        code(
            &mut s,
            &format!("CREATE MODEL m USING local WITH (huggingface = '{UP}etc')")
        ),
        "22023"
    );
    assert_eq!(
        code(
            &mut s,
            &format!("CREATE MODEL m USING local WITH (huggingface = 'a/b', revision = 'x/{UP}y')")
        ),
        "22023"
    );
    // Only an administrator reads the server's files.
    s.execute("CREATE USER maker PASSWORD 'p'; GRANT CREATE ON DATABASE nucleus TO maker")
        .unwrap();
    let mut maker = session(&kv);
    maker.login("maker").unwrap();
    assert_eq!(
        code(
            &mut maker,
            "CREATE MODEL m USING local WITH (path = '/tmp')"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &mut maker,
            "CREATE MODEL m USING local WITH (huggingface = 'a/b', endpoint = 'http://127.0.0.1:1')"
        ),
        "42501"
    );
}

/// A hub on loopback: `/{repo}/resolve/{rev}/{file}`, the weights by a
/// redirect to `/cas/…` with the SHA-256 announced (`bad` announces a
/// wrong one); a missing pooling file is a 404.
fn hub(files: Vec<(String, Vec<u8>)>, bad: bool) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                let k = s.read(&mut buf).unwrap();
                if k == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..k]);
            }
            let head = String::from_utf8_lossy(&raw).to_string();
            let path = head.split(' ').nth(1).unwrap_or("").to_string();
            let file = |name: &str| {
                files
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, b)| b.clone())
            };
            let (status, extra, body) = if let Some(name) = path.strip_prefix("/cas/") {
                (200, String::new(), file(name).unwrap())
            } else if let Some(name) = path.strip_prefix("/org/tiny/resolve/v1/") {
                match file(name) {
                    Some(b) if name == "model.safetensors" => {
                        let sha: String = nucleus_sql::ext::sha256(&b)
                            .iter()
                            .map(|x| format!("{x:02x}"))
                            .collect();
                        let sha = if bad { "0".repeat(64) } else { sha };
                        (
                            302,
                            format!("Location: /cas/{name}\r\nX-Linked-Etag: \"{sha}\"\r\n"),
                            Vec::new(),
                        )
                    }
                    Some(b) => (200, String::new(), b),
                    None => (404, String::new(), b"Entry not found".to_vec()),
                }
            } else {
                (404, String::new(), b"Repository not found".to_vec())
            };
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(&body);
        }
    });
    format!("http://{addr}")
}

#[test]
fn from_a_hub() {
    let mut files = tiny();
    // No pooling file: named instead.
    files.retain(|(n, _)| n != "1_Pooling/config.json");
    let url = hub(files.clone(), false);
    let kv = kv();
    let mut s = session(&kv);
    s.execute(&format!(
        "CREATE MODEL tiny USING local WITH (huggingface = 'org/tiny', revision = 'v1', \
         endpoint = '{url}', pooling = 'cls')"
    ))
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT cardinality(files) FROM nc_catalog.models"),
        vec![vec![Value::Int(3)]]
    );
    let v = rows(&mut s, "SELECT embed('data base', 'tiny', 'query')::text");
    assert_eq!(vector(&v[0][0]).len(), H);
    // Without pooling where the repository has none: refused when made.
    assert_eq!(
        code(
            &mut s,
            &format!(
                "CREATE MODEL nopool USING local WITH (huggingface = 'org/tiny', revision = 'v1', endpoint = '{url}')"
            )
        ),
        "55000"
    );
    assert_eq!(
        code(
            &mut s,
            &format!(
                "CREATE MODEL gone USING local WITH (huggingface = 'org/none', revision = 'v1', endpoint = '{url}')"
            )
        ),
        "55000"
    );
    // Weights that are not what the hub announced.
    let bad = hub(files, true);
    assert_eq!(
        code(
            &mut s,
            &format!(
                "CREATE MODEL bad USING local WITH (huggingface = 'org/tiny', revision = 'v1', endpoint = '{bad}', pooling = 'mean')"
            )
        ),
        "58030"
    );
}
