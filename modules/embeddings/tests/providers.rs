//! The providers against servers on loopback that answer as OpenAI,
//! Cohere and Voyage do: the requests they get, the vectors in order, the
//! key sent and never shown, a throttled call tried again, a refusal.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::{Output, Registry, Session, Value};
use serde_json::{Value as Json, json};

/// A request as the fake server got it.
#[derive(Clone, Debug)]
struct Got {
    path: String,
    auth: Option<String>,
    body: Json,
}

/// A server answering each request with `answer(request) -> (status,
/// extra headers, body)`; what it got, in order.
fn serve(
    answer: impl Fn(&Got, usize) -> (u16, String, Json) + Send + 'static,
) -> (String, Arc<Mutex<Vec<Got>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let got = Arc::new(Mutex::new(Vec::new()));
    let log = got.clone();
    std::thread::spawn(move || {
        for (n, s) in l.incoming().enumerate() {
            let mut s = s.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let (head, body) = loop {
                let k = s.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..k]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8(raw[..end].to_vec()).unwrap();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    while raw.len() < end + 4 + len {
                        let k = s.read(&mut buf).unwrap();
                        raw.extend_from_slice(&buf[..k]);
                    }
                    break (head, raw[end + 4..end + 4 + len].to_vec());
                }
            };
            let path = head.split(' ').nth(1).unwrap().to_string();
            let auth = head
                .lines()
                .find_map(|l| l.strip_prefix("Authorization: "))
                .map(str::to_string);
            let g = Got {
                path,
                auth,
                body: serde_json::from_slice(&body).unwrap(),
            };
            log.lock().unwrap().push(g.clone());
            let (status, extra, json) = answer(&g, n);
            let body = json.to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}"), got)
}

/// A vector of a text: its length, its first byte, 1.
fn fake_vector(t: &str) -> Json {
    json!([t.len() as f64, f64::from(t.as_bytes()[0]), 1.0])
}

fn session() -> Session {
    nucleus_sql::ext::secrets::set_keyring(Some(Arc::new(nucleus_sql::ext::secrets::StaticKey(
        [5; 32],
    ))));
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(3)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut r = Registry::builtin();
    r.add(&nucleus_embeddings::Embeddings).unwrap();
    Session::with_registry(kv, Arc::new(r))
}

fn one(s: &mut Session, sql: &str) -> Value {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { mut rows, .. } => rows.remove(0).remove(0),
        other => panic!("{other:?}"),
    }
}

#[test]
fn openai_compatible() {
    let (url, got) = serve(|g, n| {
        if n == 1 {
            // Throttled once: tried again.
            return (
                429,
                "Retry-After: 0\r\n".into(),
                json!({"error": "slow down"}),
            );
        }
        let input: Vec<&str> = g.body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        // Answered out of order, by index.
        let data: Vec<Json> = input
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| json!({"index": i, "embedding": fake_vector(t)}))
            .collect();
        (200, String::new(), json!({"data": data}))
    });
    let mut s = session();
    s.execute("CREATE SECRET k VALUE 'sk-test-abc'").unwrap();
    s.execute(&format!(
        "CREATE MODEL m USING openai WITH (url = '{url}/v1', model = 'fake-embed', dimensions = 3, \
         secret = k, query_prefix = 'query: ', batch = 2)"
    ))
    .unwrap();
    assert_eq!(
        one(&mut s, "SELECT embed('hello', 'm')::text"),
        Value::Text("[5,104,1]".into())
    );
    assert_eq!(
        one(&mut s, "SELECT embed('hello', 'm', 'query')::text"),
        Value::Text("[12,113,1]".into()),
        "the model's query prefix is sent"
    );
    s.execute("CREATE TABLE d (id int PRIMARY KEY, body text)")
        .unwrap();
    s.execute("INSERT INTO d VALUES (1, 'a'), (2, 'bb'), (3, 'ccc'), (4, 'a')")
        .unwrap();
    match s
        .execute("SELECT id, embed(body, 'm')::text FROM d ORDER BY id")
        .unwrap()
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => {
            let v: Vec<String> = rows.iter().map(|r| format!("{:?}", r[1])).collect();
            assert_eq!(v[0], v[3], "the same text, the same vector");
            assert!(
                v[1].contains("[2,98,1]") && v[2].contains("[3,99,1]"),
                "{v:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let got = got.lock().unwrap().clone();
    assert_eq!(got[0].path, "/v1/embeddings");
    assert_eq!(got[0].auth.as_deref(), Some("Bearer sk-test-abc"));
    assert_eq!(got[0].body["model"], "fake-embed");
    assert!(
        got[0].body.get("dimensions").is_none(),
        "not an OpenAI v3 model"
    );
    assert_eq!(got[2].body["input"], json!(["query: hello"]));
    // The cache answers what was asked before.
    assert_eq!(
        one(&mut s, "SELECT embed('hello', 'm')::text"),
        Value::Text("[5,104,1]".into())
    );
    let calls = one(
        &mut s,
        "SELECT cached FROM nc_catalog.models WHERE name = 'm'",
    );
    assert!(matches!(calls, Value::Int(n) if n >= 1), "{calls:?}");
}

#[test]
fn a_refusal_names_no_key() {
    let (url, _) = serve(|_, _| (401, String::new(), json!({"error": {"message": "bad key"}})));
    let mut s = session();
    s.execute("CREATE SECRET k VALUE 'sk-secret-xyz'").unwrap();
    s.execute(&format!(
        "CREATE MODEL m USING openai WITH (url = '{url}/v1', model = 'text-embedding-3-small', secret = k)"
    ))
    .unwrap();
    assert_eq!(
        one(&mut s, "SELECT dimensions FROM nc_catalog.models"),
        Value::Int(1536)
    );
    let e = s.execute("SELECT embed('x', 'm')").unwrap_err();
    assert_eq!(e.code, "55000");
    assert!(
        e.message.contains("401") && !e.message.contains("sk-secret"),
        "{}",
        e.message
    );
    // Throttled twice: worth trying again — after the wait it asked for,
    // which the error carries (the embedding columns' queue obeys it).
    let (busy, _) = serve(|_, _| (429, "Retry-After: 0\r\n".into(), json!({"error": "busy"})));
    s.execute(&format!(
        "CREATE MODEL busy USING openai WITH (url = '{busy}/v1', model = 'x', dimensions = 2)"
    ))
    .unwrap();
    let e = s.execute("SELECT embed('x', 'busy')").unwrap_err();
    assert_eq!(e.code, "58030");
    assert_eq!(
        nucleus_sql::ext::models::retry_after(&e),
        Some(std::time::Duration::ZERO),
        "{e:?}"
    );
    // A server down: worth trying again later.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    s.execute(&format!(
        "CREATE MODEL gone USING openai WITH (url = 'http://127.0.0.1:{port}/v1', model = 'x', dimensions = 2)"
    ))
    .unwrap();
    assert_eq!(
        s.execute("SELECT embed('x', 'gone')").unwrap_err().code,
        "58030"
    );
    // Options checked when made.
    for bad in [
        "CREATE MODEL b USING openai WITH (model = 'unknown-model')",
        "CREATE MODEL b USING openai WITH (dimensions = 3)",
        "CREATE MODEL b USING openai WITH (model = 'x', dimensions = 3, colour = 'red')",
        "CREATE MODEL b USING cohere WITH (model = 'embed-v4.0', batch = 500)",
    ] {
        assert_eq!(s.execute(bad).unwrap_err().code, "22023", "{bad}");
    }
}

#[test]
fn cohere_and_voyage_say_query_or_document() {
    let (url, got) = serve(|g, _| {
        if g.path == "/v2/embed" {
            let texts = g.body["texts"].as_array().unwrap();
            let rows: Vec<Json> = texts
                .iter()
                .map(|t| fake_vector(t.as_str().unwrap()))
                .collect();
            // The tokens billed: 10 a text.
            let billed = json!({"billed_units": {"input_tokens": 10 * rows.len()}});
            (
                200,
                String::new(),
                json!({"embeddings": {"float": rows}, "meta": billed}),
            )
        } else {
            let input = g.body["input"].as_array().unwrap();
            let data: Vec<Json> = input
                .iter()
                .enumerate()
                .map(|(i, t)| json!({"index": i, "embedding": fake_vector(t.as_str().unwrap())}))
                .collect();
            let usage = json!({"total_tokens": 20 * data.len()});
            (200, String::new(), json!({"data": data, "usage": usage}))
        }
    });
    let mut s = session();
    s.execute("CREATE SECRET k VALUE 'key'").unwrap();
    s.execute(&format!(
        "CREATE MODEL c USING cohere WITH (url = '{url}', model = 'embed-english-v3.0', dimensions = 3, secret = k)"
    ))
    .unwrap();
    s.execute(&format!(
        "CREATE MODEL v USING voyage WITH (url = '{url}', model = 'voyage-3', dimensions = 3, secret = k)"
    ))
    .unwrap();
    for sql in [
        "SELECT embed('doc', 'c')",
        "SELECT embed('q', 'c', 'query')",
        "SELECT embed('doc', 'v')",
        "SELECT embed('q', 'v', 'query')",
    ] {
        s.execute(sql).unwrap();
    }
    let got = got.lock().unwrap().clone();
    let kinds: Vec<(&str, &str)> = got
        .iter()
        .map(|g| (g.path.as_str(), g.body["input_type"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("/v2/embed", "search_document"),
            ("/v2/embed", "search_query"),
            ("/v1/embeddings", "document"),
            ("/v1/embeddings", "query"),
        ]
    );
    assert_eq!(got[0].body["output_dimension"], json!(3));
    // The tokens each answer reported.
    assert_eq!(
        one(
            &mut s,
            "SELECT string_agg(name || '=' || tokens, ' ' ORDER BY name) FROM nc_catalog.models"
        ),
        Value::Text("c=20 v=40".into())
    );
}

#[test]
fn calls_at_once_and_their_usage() {
    let (url, got) = serve(|g, _| {
        let input = g.body["input"].as_array().unwrap();
        let data: Vec<Json> = input
            .iter()
            .enumerate()
            .map(|(i, t)| json!({"index": i, "embedding": fake_vector(t.as_str().unwrap())}))
            .collect();
        let usage = json!({"prompt_tokens": 3, "total_tokens": 3 * data.len()});
        (200, String::new(), json!({"data": data, "usage": usage}))
    });
    let mut s = session();
    // Named apart: a model's statistics are the process's, by name.
    s.execute(&format!(
        "CREATE MODEL par USING openai WITH (url = '{url}/v1', model = 'x', dimensions = 3, \
         batch = 2, parallel = 3, tokens_per_minute = 100000)"
    ))
    .unwrap();
    s.execute("CREATE TABLE d (id int PRIMARY KEY, body text) WITH (storage = column)")
        .unwrap();
    s.execute("INSERT INTO d SELECT i, 'text ' || i FROM generate_series(1, 9) i")
        .unwrap();
    s.execute("SET parallel_workers = 1").unwrap();
    assert_eq!(
        one(&mut s, "SELECT count(embed(body, 'par')) FROM d"),
        Value::Int(9)
    );
    // Five calls (two texts each, one left over), made three at a time,
    // each answer in its place.
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 5);
    let mut sent: Vec<String> = got
        .iter()
        .flat_map(|g| g.body["input"].as_array().unwrap().clone())
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    sent.sort();
    assert_eq!(sent.len(), 9);
    assert_eq!(
        one(&mut s, "SELECT embed('text 7', 'par')::text"),
        Value::Text("[6,116,1]".into())
    );
    assert_eq!(
        one(
            &mut s,
            "SELECT calls || ' ' || tokens || ' ' || tokens_per_minute || ' ' || parallel \
             FROM nc_catalog.models WHERE name = 'par'"
        ),
        Value::Text("5 27 100000 3".into())
    );
}
