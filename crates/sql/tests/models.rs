//! Models and `embed()` (PLAN.md §M14.22 E2b), with the built-in
//! `hashing` provider and a counting one registered here.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::error::Result;
use nucleus_sql::models::{self, Input, ModelProvider, Request, Spec};
use nucleus_sql::secrets::{self, StaticKey};
use nucleus_sql::session::Output;
use nucleus_sql::types::Value;

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(5)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
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

/// Counts its calls and texts; a text's vector is its length and its
/// prefix's, so a test sees which prefix a call got.
struct Counting {
    name: &'static str,
    calls: AtomicUsize,
    texts: AtomicUsize,
    seen: Mutex<Vec<(Input, String, Option<String>)>>,
}

impl Counting {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Counting {
            name,
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl ModelProvider for Counting {
    fn name(&self) -> &str {
        self.name
    }

    fn define(&self, o: &models::Options, _files: &models::Files<'_>) -> Result<Spec> {
        o.only(self.name, &["batch"])?;
        Ok(Spec {
            dimensions: 2,
            max_batch: o.number("batch", 1..=1000)?.unwrap_or(1000) as usize,
        })
    }

    fn embed(&self, r: &Request<'_>) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.fetch_add(r.texts.len(), Ordering::SeqCst);
        let mut seen = self.seen.lock().unwrap();
        for t in r.texts {
            seen.push((r.input, t.clone(), r.secret.map(|s| s.expose().to_string())));
        }
        Ok(r.texts
            .iter()
            .map(|t| vec![t.chars().count() as f32, 1.0])
            .collect())
    }

    fn remote(&self) -> bool {
        false
    }
}

fn setup(provider: &Arc<Counting>) -> (Arc<dyn Kv>, Session) {
    secrets::set_keyring(Some(Arc::new(StaticKey([9; 32]))));
    models::register(provider.clone()).unwrap();
    let kv = db();
    let mut admin = Session::new(kv.clone());
    admin.login("root").unwrap();
    (kv, admin)
}

#[test]
fn hashing_embeds_without_a_network() {
    let kv = db();
    let mut s = Session::new(kv);
    s.execute("CREATE MODEL h USING hashing WITH (dimensions = 64)")
        .unwrap();
    s.execute("CREATE TABLE d (id int PRIMARY KEY, body text, v vector(64))")
        .unwrap();
    s.execute(
        "INSERT INTO d VALUES (1, 'red apples and pears', NULL), (2, 'a blue sky', NULL), \
         (3, 'pears, apples: red', NULL)",
    )
    .unwrap();
    s.execute("UPDATE d SET v = embed(body, 'h')").unwrap();
    let near = rows(
        &mut s,
        "SELECT id FROM d ORDER BY v <=> embed('red apples', 'h', 'query'), id LIMIT 2",
    );
    assert_eq!(
        near,
        vec![vec![Value::Int(3)], vec![Value::Int(1)]],
        "the rows sharing the query's words first, the one with no other word nearest"
    );
    // A vector of another size does not go in.
    s.execute("CREATE MODEL h8 USING hashing WITH (dimensions = 8)")
        .unwrap();
    assert_eq!(code(&mut s, "UPDATE d SET v = embed(body, 'h8')"), "22000");
    assert_eq!(
        rows(
            &mut s,
            "SELECT provider, dimensions, max_batch FROM nc_catalog.models WHERE name = 'h'"
        ),
        vec![vec![
            Value::Text("hashing".into()),
            Value::Int(64),
            Value::Int(1024)
        ]]
    );
    assert!(
        rows(
            &mut s,
            "SELECT name, remote FROM nc_catalog.model_providers"
        )
        .contains(&vec![Value::Text("hashing".into()), Value::Bool(false)])
    );
    // Refusals when made.
    assert_eq!(code(&mut s, "CREATE MODEL h USING hashing"), "42710");
    s.execute("CREATE MODEL IF NOT EXISTS h USING hashing")
        .unwrap();
    assert_eq!(code(&mut s, "CREATE MODEL x USING nowhere"), "42704");
    assert_eq!(
        code(&mut s, "CREATE MODEL x USING hashing WITH (dimensions = 0)"),
        "22023"
    );
    assert_eq!(
        code(
            &mut s,
            "CREATE MODEL x USING hashing WITH (analyzer = 'klingon')"
        ),
        "42704"
    );
    assert_eq!(
        code(
            &mut s,
            "CREATE MODEL x USING hashing WITH (secret = nothing)"
        ),
        "42704"
    );
    assert_eq!(code(&mut s, "SELECT embed('a', 'nothing')"), "42704");
    assert_eq!(code(&mut s, "SELECT embed('a', 'h', 'sideways')"), "22023");
    assert_eq!(
        rows(&mut s, "SELECT embed(NULL, 'h')"),
        vec![vec![Value::Null]]
    );
    s.execute("DROP MODEL h8").unwrap();
    assert_eq!(code(&mut s, "DROP MODEL h8"), "42704");
    s.execute("DROP MODEL IF EXISTS h8").unwrap();
}

#[test]
fn prefixes_secrets_and_the_cache() {
    let p = Counting::new("counting_a");
    let (_kv, mut s) = setup(&p);
    s.execute("CREATE SECRET key VALUE 'the-key'").unwrap();
    s.execute(
        "CREATE MODEL m USING counting_a WITH (secret = key, query_prefix = 'query: ', \
         document_prefix = 'passage: ', batch = 3)",
    )
    .unwrap();
    s.execute("CREATE TABLE d (id int PRIMARY KEY, body text, v vector(2))")
        .unwrap();
    let values: Vec<String> = (0..40)
        .map(|i| format!("({i}, 'text {}', NULL)", i % 10))
        .collect();
    s.execute(&format!("INSERT INTO d VALUES {}", values.join(", ")))
        .unwrap();
    s.execute("UPDATE d SET v = embed(body, 'm')").unwrap();
    // Ten distinct texts, each sent once.
    assert_eq!(p.texts.load(Ordering::SeqCst), 10);
    {
        let seen = p.seen.lock().unwrap();
        assert!(seen.iter().all(|(i, t, k)| *i == Input::Document
            && t.starts_with("passage: text ")
            && k.as_deref() == Some("the-key")));
    }
    // A query's vector, once for the statement however many rows it is
    // compared with.
    let before = p.calls.load(Ordering::SeqCst);
    let r = rows(
        &mut s,
        "SELECT id FROM d ORDER BY v <-> embed('xy', 'm', 'query'), id LIMIT 1",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(p.calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(
        p.seen.lock().unwrap().last().unwrap().1,
        "query: xy",
        "a query gets the query prefix"
    );
    // Again: from the cache.
    rows(
        &mut s,
        "SELECT id FROM d ORDER BY v <-> embed('xy', 'm', 'query'), id LIMIT 1",
    );
    assert_eq!(p.calls.load(Ordering::SeqCst), before + 1);
    let stats = rows(
        &mut s,
        "SELECT calls >= 4, texts, cached > 0, errors FROM nc_catalog.models WHERE name = 'm'",
    );
    assert_eq!(
        stats,
        vec![vec![
            Value::Bool(true),
            Value::Int(11),
            Value::Bool(true),
            Value::Int(0)
        ]]
    );
    // A secret a model uses is not dropped under it.
    assert_eq!(code(&mut s, "DROP SECRET key"), "2BP01");
    s.execute("DROP MODEL m").unwrap();
    s.execute("DROP SECRET key").unwrap();
}

#[test]
fn models_are_used_by_those_granted() {
    let p = Counting::new("counting_b");
    let (kv, mut admin) = setup(&p);
    admin
        .execute(
            "CREATE USER app PASSWORD 'a'; CREATE USER maker PASSWORD 'm'; \
             CREATE SECRET key VALUE 'k'; CREATE MODEL m USING counting_b",
        )
        .unwrap();
    let mut app = Session::new(kv.clone());
    app.login("app").unwrap();
    assert_eq!(code(&mut app, "SELECT embed('x', 'm')"), "42501");
    assert!(rows(&mut app, "SELECT name FROM nc_catalog.models").is_empty());
    admin.execute("GRANT USAGE ON MODEL m TO app").unwrap();
    app.execute("SELECT embed('x', 'm')").unwrap();
    assert_eq!(
        rows(&mut app, "SELECT name FROM nc_catalog.models").len(),
        1
    );
    assert_eq!(code(&mut app, "DROP MODEL m"), "42501");
    assert_eq!(
        code(&mut app, "CREATE MODEL mine USING counting_b"),
        "42501"
    );
    // A maker may make models, but name only secrets it may use.
    admin
        .execute("GRANT CREATE ON DATABASE nucleus TO maker")
        .unwrap();
    let mut maker = Session::new(kv.clone());
    maker.login("maker").unwrap();
    assert_eq!(
        code(
            &mut maker,
            "CREATE MODEL theirs USING counting_b WITH (secret = key)"
        ),
        "42501"
    );
    admin.execute("GRANT USAGE ON SECRET key TO maker").unwrap();
    maker
        .execute("CREATE MODEL theirs USING counting_b WITH (secret = key)")
        .unwrap();
    maker.execute("SELECT embed('x', 'theirs')").unwrap();
    assert_eq!(code(&mut admin, "DROP ROLE maker"), "2BP01");
    admin.execute("ALTER ROLE maker RENAME TO maker2").unwrap();
    assert_eq!(
        rows(
            &mut admin,
            "SELECT owner FROM nc_catalog.models WHERE name = 'theirs'"
        ),
        vec![vec![Value::Text("maker2".into())]]
    );
    // A table called model is still granted on.
    admin.execute("CREATE TABLE model (x int)").unwrap();
    admin.execute("GRANT SELECT ON model TO app").unwrap();
    // The audit log shows the definitions, the secret's strings masked
    // only where a secret is named by keyword.
    let audit = rows(
        &mut admin,
        "SELECT detail FROM nc_catalog.audit WHERE detail LIKE 'CREATE MODEL theirs%'",
    );
    assert_eq!(audit.len(), 2);
}

#[test]
fn a_batch_of_rows_in_few_calls() {
    let p = Counting::new("counting_c");
    let (_kv, mut s) = setup(&p);
    s.execute("CREATE MODEL m USING counting_c WITH (batch = 100)")
        .unwrap();
    s.execute("CREATE TABLE c (id int PRIMARY KEY, body text) WITH (storage = column)")
        .unwrap();
    let values: Vec<String> = (0..2000).map(|i| format!("({i}, 'doc {i}')")).collect();
    s.execute(&format!("INSERT INTO c VALUES {}", values.join(", ")))
        .unwrap();
    let r = rows(&mut s, "SELECT count(embed(body, 'm')) FROM c");
    assert_eq!(r, vec![vec![Value::Int(2000)]]);
    assert_eq!(p.texts.load(Ordering::SeqCst), 2000);
    let calls = p.calls.load(Ordering::SeqCst);
    assert!(
        calls <= 30,
        "{calls} calls for 2,000 texts (24 when measured)"
    ); // The same text more than once in a batch: sent once, each row its
    // vector (the first run of the embedding columns found it panicking).
    let before = p.texts.load(Ordering::SeqCst);
    let r = rows(
        &mut s,
        "SELECT embed(t, 'm')::text FROM (VALUES ('again'), ('again'), ('new'), ('again')) v(t)",
    );
    assert_eq!(r.len(), 4);
    assert_eq!(r[0], r[1]);
    assert_eq!(r[0], r[3]);
    assert_eq!(p.texts.load(Ordering::SeqCst) - before, 2);
}
