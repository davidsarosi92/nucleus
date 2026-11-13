//! Text-search queries and documents: nucleus's query syntax (`body @@
//! '…'`, Lucene's), PostgreSQL's `tsquery` and `tsvector` and the
//! functions that make them, highlighting, and the chunkers that cut a
//! document for a model.
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use nucleus_sql::{Session, Value};

const STATEMENTS: &[&str] = &[
    "SELECT id, score(body @@ $1) FROM docs WHERE body @@ $1 ORDER BY 2 DESC LIMIT 5",
    "SELECT id FROM plain WHERE body @@ $1",
    "SELECT highlight(body @@ $1), snippet(body @@ $1, '[', ']', '…', 5) FROM docs",
    "SELECT $1 @@ 'fox OR dog'",
    "SELECT to_tsquery('english', $1)::text, plainto_tsquery($1)::text, \
     phraseto_tsquery($1)::text, websearch_to_tsquery('english', $1)::text",
    "SELECT $1::tsquery::text, to_tsvector('english', 'the quick brown fox') @@ $1::tsquery",
    "SELECT $1::tsvector::text, to_tsvector('simple', $1)::text, \
     ts_rank($1::tsvector, 'fox'::tsquery), ts_rank_cd($1::tsvector, 'fox'::tsquery)",
    "SELECT ts_headline('english', $1, to_tsquery('english', 'fox & dog'))",
    "SELECT ts_headline('english', 'the quick brown fox jumps over the lazy dog', \
     websearch_to_tsquery('english', $1), 'MaxWords=5, MinWords=2')",
    "SELECT count(*), sum(size) FROM chunks($1, 'sentences', 7, 2)",
    "SELECT count(*), sum(size) FROM chunks($1, 'markdown', 9)",
    "SELECT count(*), sum(size) FROM chunks($1, 'tokens', 4, 1)",
];

fn session() -> Session {
    let mut s = Session::with_registry(nucleus_fuzz::database(), nucleus_fuzz::pg_registry());
    s.execute(
        "CREATE TABLE docs (id int PRIMARY KEY, body text); \
         CREATE TABLE plain (id int PRIMARY KEY, body text); \
         INSERT INTO docs VALUES (1, 'the quick brown fox'), (2, 'fox fox fox'), \
           (3, 'a lazy dog'), (4, 'the fox and the dog and the cat'), (5, NULL); \
         INSERT INTO plain SELECT * FROM docs; \
         CREATE INDEX docs_body ON docs USING text (body)",
    )
    .expect("the documents");
    s
}

thread_local! {
    static S: RefCell<Session> = RefCell::new(session());
}

fuzz_target!(|data: &[u8]| {
    let Some((&k, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let sql = STATEMENTS[k as usize % STATEMENTS.len()];
    S.with_borrow_mut(|s| nucleus_fuzz::run(s, sql, &[Value::Text(text.into())]));
});
