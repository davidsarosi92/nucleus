//! `COPY … FROM STDIN`'s data, in PostgreSQL's text format and in CSV
//! with its options, into a table of many types (with a key, a check, a
//! foreign key and an index to keep).
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use nucleus_sql::Session;

const COPIES: &[&str] = &[
    "COPY t FROM STDIN",
    "COPY t (id, n, s) FROM STDIN",
    "COPY t FROM STDIN WITH (FORMAT csv)",
    "COPY t FROM STDIN WITH CSV HEADER DELIMITER ';' NULL 'NA'",
    "COPY t FROM STDIN WITH (FORMAT csv, QUOTE '''', ESCAPE '\\')",
    "COPY t FROM STDIN WITH (DELIMITER '|', NULL '')",
    "COPY c FROM STDIN",
    "COPY c FROM STDIN WITH (FORMAT csv)",
];

fn session() -> Session {
    let mut s = Session::new(nucleus_fuzz::database());
    s.execute(
        "CREATE TABLE p (id int PRIMARY KEY); INSERT INTO p VALUES (1), (2); \
         CREATE TABLE t (id int PRIMARY KEY, n numeric(10, 2) CHECK (n > -1000), \
           s text, b bytea, d date, ts timestamptz, i interval, f double precision, \
           ok boolean, u uuid, a int[], v vector(2), r int REFERENCES p); \
         CREATE INDEX t_s ON t (s); \
         CREATE TABLE c (id int PRIMARY KEY, x text, y real) WITH (storage = column)",
    )
    .expect("the tables");
    s
}

thread_local! {
    static S: RefCell<Session> = RefCell::new(session());
}

fuzz_target!(|data: &[u8]| {
    let Some((&k, rest)) = data.split_first() else {
        return;
    };
    let sql = COPIES[k as usize % COPIES.len()];
    S.with_borrow_mut(|s| {
        let _ = s.execute("BEGIN");
        if let Ok(parsed) = s.parse(sql) {
            let _ = s.run_copy(&parsed[0].0, &[], &mut &rest[..]);
        }
        let _ = s.execute("ROLLBACK");
    });
});
