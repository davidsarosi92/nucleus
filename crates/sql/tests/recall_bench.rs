//! Recall's latency in the process (PLAN.md §M14.24 R4; docs/benchmarks.md
//! "Memory stores") — not run by default:
//!
//! ```sh
//! RECALL_BENCH=1 cargo test --release -p nucleus-sql --test recall_bench \
//!     -- --ignored --nocapture
//! ```
//!
//! A memory store of `SUBJECTS` (1 000) subjects × `EACH` (100)
//! memories, a vocabulary of 2 000 words, the hashing model (so the
//! times are the database's, not a model's); one subject's question at a
//! time, a statement parsed once and run with parameters, then batches of
//! 16 questions in one statement (`LATERAL recall` over an array).
//! Without `RECALL_BENCH` (the CI's ignored tests) it runs small, as a
//! test: each batch's answers are its questions' own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nucleus_kv::{Database, Kv, Options};
use nucleus_sql::{Output, Session, Value};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as usize
    }

    fn words(&mut self, vocabulary: &[String], n: usize) -> String {
        (0..n)
            .map(|_| vocabulary[self.next() % vocabulary.len()].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn rows(out: Output) -> Vec<Vec<Value>> {
    match out {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn quantile(times: &mut [Duration], q: f64) -> Duration {
    times.sort();
    times[((times.len() - 1) as f64 * q) as usize]
}

#[test]
#[ignore]
fn recall_latency() {
    let full = std::env::var("RECALL_BENCH").is_ok();
    let env = |name: &str, small: usize, big: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(if full { big } else { small })
    };
    let (subjects, each, asks) = (
        env("SUBJECTS", 50, 1000),
        env("EACH", 20, 100),
        env("ASKS", 40, 1000),
    );
    // Real files, as the embedded edition's (a temporary directory).
    let dir = std::env::temp_dir().join(format!("nucleus-recall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let kv: Arc<dyn Kv> = Arc::new(Database::open(&dir, Options::default()).unwrap());
    let mut s = Session::new(kv.clone());
    let mut g = Lcg(17);
    // Words of 4–9 letters, made up.
    let vocabulary: Vec<String> = (0..2000)
        .map(|_| {
            let n = 4 + g.next() % 6;
            (0..n)
                .map(|_| (b'a' + (g.next() % 26) as u8) as char)
                .collect()
        })
        .collect();
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 64); \
         CREATE MEMORY STORE notes USING h; \
         SET embedding.pending = wait",
    )
    .unwrap();
    let t = Instant::now();
    for chunk in 0..subjects.div_ceil(10) {
        let values: Vec<String> = (chunk * 10..((chunk + 1) * 10).min(subjects))
            .flat_map(|u| (0..each).map(move |_| u))
            .map(|u| format!("('u{u}', '{}')", g.words(&vocabulary, 8)))
            .collect();
        s.execute(&format!(
            "INSERT INTO notes (subject, content) VALUES {}",
            values.join(", ")
        ))
        .unwrap();
    }
    // The queue's vectors made (a statement waits for them).
    s.execute("SELECT count(*) FROM notes").unwrap();
    if std::env::var("VACUUM").is_ok() {
        kv.vacuum().unwrap();
    }
    s.execute("ANALYZE").unwrap();
    println!(
        "{} memories of {subjects} subjects in {:?}",
        subjects * each,
        t.elapsed()
    );
    let one = nucleus_sql::parser::parse("SELECT id, score FROM recall('notes', $1, $2, 10)")
        .unwrap()
        .remove(0);
    let asked: Vec<(String, String)> = (0..asks)
        .map(|_| (format!("u{}", g.next() % subjects), g.words(&vocabulary, 3)))
        .collect();
    let ask = |s: &mut Session, (u, q): &(String, String)| {
        rows(
            s.run(&one, &[Value::Text(u.clone()), Value::Text(q.clone())])
                .unwrap(),
        )
    };
    if std::env::var("EXPLAIN").is_ok() {
        let (u, q) = &asked[0];
        s.execute(&format!("SELECT id FROM recall('notes', '{u}', '{q}', 10)"))
            .unwrap();
        for o in s
            .execute(&format!(
                "EXPLAIN ANALYZE SELECT id, score FROM recall('notes', '{u}', '{q}', 10)"
            ))
            .unwrap()
        {
            if let Output::Rows { rows, .. } = o {
                for r in rows {
                    println!("{}", r[0].render());
                }
            }
        }
        let t = Instant::now();
        s.execute(&format!(
            "SELECT id, score FROM recall('notes', '{u}', '{q}', 10)"
        ))
        .unwrap();
        println!("literal: {:?}", t.elapsed());
    }
    // Warm.
    for a in asked.iter().take(20) {
        ask(&mut s, a);
    }
    let mut times = Vec::with_capacity(asks);
    let mut answers = Vec::with_capacity(asks);
    for a in &asked {
        let t = Instant::now();
        answers.push(ask(&mut s, a));
        times.push(t.elapsed());
    }
    println!(
        "recall(…, 10), one question a statement: p50 {:?}, p95 {:?}",
        quantile(&mut times, 0.5),
        quantile(&mut times, 0.95)
    );
    // Batches of 16 questions of one subject.
    let batch = nucleus_sql::parser::parse(
        "SELECT n, r.id, r.score FROM unnest($2::text[]) WITH ORDINALITY AS b(q, n), \
         LATERAL recall('notes', $1, q, 10) r ORDER BY n",
    )
    .unwrap()
    .remove(0);
    let mut per = Vec::new();
    for (k, group) in asked.chunks(16).enumerate() {
        let u = &group[0].0;
        let qs = Value::Array(group.iter().map(|(_, q)| Value::Text(q.clone())).collect());
        let t = Instant::now();
        let got = rows(s.run(&batch, &[Value::Text(u.clone()), qs]).unwrap());
        per.push(t.elapsed() / group.len() as u32);
        // Each question's answer is its own recall's.
        for (j, (_, q)) in group.iter().enumerate() {
            let mine: Vec<Vec<Value>> = got
                .iter()
                .filter(|r| r[0] == Value::Int(j as i64 + 1))
                .map(|r| r[1..].to_vec())
                .collect();
            let alone = ask(&mut s, &(u.clone(), q.clone()));
            assert_eq!(mine, alone, "batch {k}, question {j}");
        }
    }
    println!(
        "recall(…, 10), 16 questions a statement: p50 {:?}, p95 {:?} a question",
        quantile(&mut per, 0.5),
        quantile(&mut per, 0.95)
    );
    assert_eq!(answers.len(), asks);
    drop(s);
    drop(kv);
    let _ = std::fs::remove_dir_all(&dir);
}
