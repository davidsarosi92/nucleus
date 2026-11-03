//! `COPY … FROM STDIN` in the engine (PLAN.md §M14.11 L5): rows checked
//! as an INSERT's — defaults, types, enums, constraints, foreign keys,
//! privileges, row-level security — and none stored when one fails.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value, parse};

fn session() -> Session {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    Session::new(kv)
}

fn copy(s: &mut Session, sql: &str, data: &str) -> nucleus_sql::Result<Output> {
    let stmt = parse(sql)?.pop().unwrap();
    s.run_copy(&stmt, &[], &mut data.as_bytes())
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

#[test]
fn copy_checks_rows_as_insert_does() {
    let mut s = session();
    s.execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok'); \
         CREATE TABLE parent (id INT PRIMARY KEY); \
         INSERT INTO parent VALUES (1), (2); \
         CREATE TABLE child (id INT PRIMARY KEY, parent INT REFERENCES parent (id), \
           m mood, n NUMERIC(5, 2) CHECK (n >= 0), at TIMESTAMPTZ DEFAULT '2024-01-01 00:00Z', \
           tags TEXT[], code VARCHAR(3))",
    )
    .unwrap();
    let out = copy(
        &mut s,
        "COPY child (id, parent, m, n, tags, code) FROM STDIN",
        "1\t1\tok\t1.234\t{a,b}\tabc\n2\t\\N\tsad\t0\t{}\t\\N\n",
    )
    .unwrap();
    assert_eq!(out, Output::Command("COPY 2".into()));
    assert_eq!(
        rows(
            &mut s,
            "SELECT id, m::text, n::text, at::text, tags[2], code FROM child ORDER BY id"
        ),
        vec![
            vec![
                Value::Int(1),
                Value::Text("ok".into()),
                Value::Text("1.23".into()),
                Value::Text("2024-01-01 00:00:00+00".into()),
                Value::Text("b".into()),
                Value::Text("abc".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("sad".into()),
                Value::Text("0.00".into()),
                Value::Text("2024-01-01 00:00:00+00".into()),
                Value::Null,
                Value::Null,
            ],
        ]
    );
    for (data, code) in [
        ("3\t9\tok\t1\t{}\tx\n", "23503"),    // no parent 9
        ("3\t1\tglad\t1\t{}\tx\n", "22P02"),  // not a label
        ("3\t1\tok\t-1\t{}\tx\n", "23514"),   // the check
        ("3\t1\tok\t1\t{}\tabcd\n", "22001"), // too long
        ("3\t1\tok\tone\t{}\tx\n", "22P02"),  // not a number
        ("3\t1\tok\n", "22P04"),              // too few fields
    ] {
        let e = copy(
            &mut s,
            "COPY child (id, parent, m, n, tags, code) FROM STDIN",
            &format!("10\t1\tok\t1\t{{}}\tx\n{data}"),
        )
        .unwrap_err();
        assert_eq!(e.code, code, "{data:?}: {}", e.message);
    }
    // None of the failed loads' rows stayed.
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM child"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn copy_needs_insert_and_passes_row_security() {
    let mut s = session();
    s.execute(
        "CREATE TABLE notes (id INT PRIMARY KEY, owner TEXT, body TEXT); \
         CREATE USER writer; \
         GRANT INSERT (id, owner) ON notes TO writer; \
         GRANT SELECT ON notes TO writer; \
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY own ON notes USING (owner = current_user) WITH CHECK (owner = current_user)",
    )
    .unwrap();
    s.execute("SET ROLE writer").unwrap();
    // A column without INSERT; a row the policy refuses; then a good one.
    let e = copy(&mut s, "COPY notes FROM STDIN", "1\twriter\thi\n").unwrap_err();
    assert_eq!(e.code, "42501");
    let e = copy(&mut s, "COPY notes (id, owner) FROM STDIN", "1\tsomeone\n").unwrap_err();
    assert_eq!(e.code, "42501");
    copy(
        &mut s,
        "COPY notes (id, owner) FROM STDIN WITH (FORMAT csv)",
        "1,writer\n",
    )
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id FROM notes"),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn copy_without_data_is_refused_and_options_are_checked() {
    let mut s = session();
    s.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
    // `Session::run` has no client to take data from.
    let e = s.execute("COPY t FROM STDIN").unwrap_err();
    assert_eq!(e.code, "0A000");
    for (sql, code) in [
        ("COPY t TO STDOUT", "0A000"),
        ("COPY t FROM '/etc/passwd'", "0A000"),
        ("COPY t FROM STDIN WITH (FORMAT binary)", "0A000"),
        ("COPY t FROM STDIN WITH (DELIMITER 'ab')", "0A000"),
        ("COPY t (a, a) FROM STDIN", "42701"),
        ("COPY t (c) FROM STDIN", "42703"),
    ] {
        let e = copy(&mut s, sql, "").unwrap_err();
        assert_eq!(e.code, code, "{sql}: {}", e.message);
    }
    // The older syntax, a delimiter and a NULL of one's own.
    copy(
        &mut s,
        "COPY t FROM STDIN WITH CSV HEADER DELIMITER ';' NULL 'NA'",
        "a;b\n1;NA\n2;\"x;y\"\n",
    )
    .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT a, b FROM t ORDER BY a"),
        vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Text("x;y".into())],
        ]
    );
}

/// A load larger than a transaction's memory (PLAN.md §M14.11 L5b): its
/// writes spill, the statement and the rest of its block read them —
/// indexes, foreign keys, updates — and a rollback leaves nothing.
#[test]
fn a_load_past_the_transaction_budget() {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(7)),
            Path::new("/db"),
            Options {
                txn_memory_bytes: 16 << 10,
                ..Options::default()
            },
        )
        .unwrap(),
    );
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE p (id INT PRIMARY KEY); INSERT INTO p SELECT * FROM generate_series(0, 9); \
         CREATE TABLE big (id INT PRIMARY KEY, p INT REFERENCES p (id), name TEXT UNIQUE)",
    )
    .unwrap();
    let data: String = (0..20_000)
        .map(|i| format!("{i}\t{}\tn{i}\n", i % 10))
        .collect();
    s.execute("BEGIN").unwrap();
    assert_eq!(
        copy(&mut s, "COPY big FROM STDIN", &data).unwrap(),
        Output::Command("COPY 20000".into())
    );
    // In the same block: the spilled rows, by key, by index, updated.
    assert_eq!(
        rows(&mut s, "SELECT count(*), sum(p) FROM big"),
        vec![vec![Value::Int(20_000), Value::Int(90_000)]]
    );
    assert_eq!(
        rows(&mut s, "SELECT id FROM big WHERE name = 'n12345'"),
        vec![vec![Value::Int(12_345)]]
    );
    let e = copy(&mut s, "COPY big FROM STDIN", "20000\t1\tn5\n").unwrap_err();
    assert_eq!(e.code, "23505");
    s.execute("ROLLBACK").unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM big"),
        vec![vec![Value::Int(0)]]
    );
    // On its own: committed whole.
    copy(&mut s, "COPY big FROM STDIN", &data).unwrap();
    s.execute("UPDATE big SET name = name || '!' WHERE p < 5")
        .unwrap();
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM big WHERE name LIKE '%!'"),
        vec![vec![Value::Int(10_000)]]
    );
    let e = s.execute("DELETE FROM p WHERE id = 3").unwrap_err();
    assert_eq!(e.code, "23503");
}

/// Into a column table (its statement's rows gathered and written at the
/// end, as an INSERT's) and into a partitioned table (each row to its
/// partition, of any storage).
#[test]
fn copy_into_column_and_partitioned_tables() {
    let mut s = session();
    s.execute(
        "CREATE TABLE c (k INT PRIMARY KEY, v TEXT) WITH (storage = column); \
         CREATE TABLE p (k INT PRIMARY KEY, v TEXT) PARTITION BY RANGE (k); \
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (5000); \
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (5000) TO (MAXVALUE) WITH (storage = column)",
    )
    .unwrap();
    let data: String = (1..=9000).map(|i| format!("{i}\tv{i}\n")).collect();
    for t in ["c", "p"] {
        let out = copy(&mut s, &format!("COPY {t} FROM STDIN"), &data).unwrap();
        assert_eq!(out, Output::Command("COPY 9000".into()));
        assert_eq!(
            rows(&mut s, &format!("SELECT count(*), sum(k), max(v) FROM {t}")),
            vec![vec![
                Value::Int(9000),
                Value::Int(9000 * 9001 / 2),
                Value::Text("v999".into())
            ]]
        );
    }
    assert_eq!(
        rows(
            &mut s,
            "SELECT (SELECT count(*) FROM p1), (SELECT count(*) FROM p2)"
        ),
        vec![vec![Value::Int(4999), Value::Int(4001)]]
    );
    // A row no partition takes: the COPY fails, nothing stored.
    let err = copy(&mut s, "COPY p FROM STDIN", "-1\tx\n").unwrap_err();
    assert_eq!(err.code, "23514");
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM p WHERE k < 0"),
        vec![vec![Value::Int(0)]]
    );
}

/// A load parsed beside its stores (PLAN.md §M14.20 W3b): with workers,
/// a thread parses the data while the statement's thread stores it — the
/// same rows, the same errors at the same lines, as one thread taking
/// turns; the parse reading the transaction (an enum made in it) between
/// stores.  Past several parse batches (4 MiB each), into a row table, a
/// column table (keys in order, and not) and a partitioned one.
#[test]
fn a_load_parsed_beside_its_stores() {
    let pad = "x".repeat(80);
    let data = |n: usize, desc: bool| -> String {
        let row = |i: usize| format!("{i}\t{}\tok\t{pad}\n", i % 97);
        if desc {
            (1..=n).rev().map(row).collect()
        } else {
            (1..=n).map(row).collect()
        }
    };
    let n = 60_000; // ~6 MiB
    let mut results = Vec::new();
    for workers in [1, 4] {
        let mut s = session();
        s.execute(&format!("SET parallel_workers = {workers}"))
            .unwrap();
        s.execute(
            "BEGIN; CREATE TYPE mood AS ENUM ('sad', 'ok'); \
             CREATE TABLE r (k INT PRIMARY KEY, g INT, m mood, pad TEXT); \
             CREATE TABLE c (k INT PRIMARY KEY, g INT, m mood, pad TEXT) WITH (storage = column); \
             CREATE TABLE d (k INT PRIMARY KEY, g INT, m mood, pad TEXT) WITH (storage = column); \
             CREATE TABLE p (k INT PRIMARY KEY, g INT, m mood, pad TEXT) PARTITION BY RANGE (k); \
             CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (25000); \
             CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (25000) TO (MAXVALUE) WITH (storage = column)",
        )
        .unwrap();
        let mut got = Vec::new();
        for (t, desc) in [("r", false), ("c", false), ("d", true), ("p", false)] {
            let out = copy(&mut s, &format!("COPY {t} FROM STDIN"), &data(n, desc)).unwrap();
            assert_eq!(out, Output::Command(format!("COPY {n}")));
            got.push(rows(
                &mut s,
                &format!(
                    "SELECT count(*), sum(k), sum(g), min(m::text), max(length(pad)) FROM {t}"
                ),
            ));
        }
        s.execute("COMMIT").unwrap();
        assert_eq!(
            got[0],
            vec![vec![
                Value::Int(n as i64),
                Value::Int((n * (n + 1) / 2) as i64),
                Value::Int((1..=n).map(|i| (i % 97) as i64).sum()),
                Value::Text("ok".into()),
                Value::Int(80),
            ]]
        );
        assert!(got.iter().all(|g| *g == got[0]), "{got:?}");
        // Errors: a bad record deep in the second batch, a key loaded
        // twice there, and a key twice before a bad record (the rows before
        // a bad one are stored first: their error is the one).
        let mut errors = Vec::new();
        for (t, bad) in [
            ("r", 0),
            ("c", 0),
            ("p", 0),
            ("r", 1),
            ("c", 1),
            ("p", 1),
            ("r", 2),
            ("c", 2),
            ("p", 3),
        ] {
            let mut d = data(n, false);
            let line = 51_234;
            let mut lines: Vec<String> = d.lines().map(str::to_string).collect();
            match bad {
                0 => lines[line - 1] = "51234\tnot a number\tok\tx".into(),
                1 => lines[line - 1] = lines[100].clone(),
                // Twice, in two partitions: the first row's error.
                3 => {
                    lines[line - 1] = lines[30_000].clone();
                    lines[line + 10] = lines[100].clone();
                }
                _ => {
                    lines[line - 1] = lines[100].clone();
                    lines[line + 5] = "1\t2\t3".into();
                }
            }
            d = lines.join("\n") + "\n";
            s.execute(&format!("DELETE FROM {t}")).unwrap();
            let e = copy(&mut s, &format!("COPY {t} FROM STDIN"), &d).unwrap_err();
            assert!(
                e.message.contains(&format!("line {line})")),
                "{t} {bad}: {}",
                e.message
            );
            errors.push((e.code, e.message));
        }
        results.push((got, errors));
    }
    assert_eq!(results[0], results[1]);
}

/// Of a batch's errors, the first row's, as a row at a time (PLAN.md
/// §M14.20 W3b): a key twice before a unique value twice, and after; a
/// row no partition takes after a key twice, and before.
#[test]
fn a_batch_fails_at_its_first_bad_row() {
    let mut s = session();
    s.execute(
        "CREATE TABLE u (id INT PRIMARY KEY, name TEXT UNIQUE); \
         CREATE TABLE p (k INT PRIMARY KEY) PARTITION BY RANGE (k); \
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10); \
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20) WITH (storage = column)",
    )
    .unwrap();
    for (sql, names) in [
        (
            "INSERT INTO u VALUES (1, 'a'), (1, 'b'), (2, 'a')",
            "u_pkey",
        ),
        (
            "INSERT INTO u VALUES (1, 'a'), (2, 'a'), (1, 'b')",
            "u_name",
        ),
        ("INSERT INTO p VALUES (15), (3), (15), (3)", "p2_pkey"),
        ("INSERT INTO p VALUES (3), (15), (3), (15)", "p1_pkey"),
        ("INSERT INTO p VALUES (3), (3), (99)", "p1_pkey"),
        ("INSERT INTO p VALUES (3), (99), (3)", "partition"),
    ] {
        let e = s.execute(sql).unwrap_err();
        assert!(e.message.contains(names), "{sql}: {}", e.message);
    }
}

/// A load's foreign keys are checked as a statement's end would check
/// them: a bad record later in the same batch fails the load with its own
/// error, not the missing parent's — one thread or parsing beside the
/// stores alike.
#[test]
fn a_bad_record_fails_before_a_missing_parent() {
    for workers in [1, 4] {
        let mut s = session();
        s.execute(&format!(
            "SET parallel_workers = {workers}; \
             CREATE TABLE parent (id INT PRIMARY KEY); INSERT INTO parent VALUES (1); \
             CREATE TABLE child (id INT PRIMARY KEY, parent INT REFERENCES parent (id))"
        ))
        .unwrap();
        let e = copy(&mut s, "COPY child FROM STDIN", "1\t1\n2\t9\n3\tthree\n").unwrap_err();
        assert_eq!(e.code, "22P02", "{workers}: {}", e.message);
        let e = copy(&mut s, "COPY child FROM STDIN", "1\t1\n2\t9\n3\t1\n").unwrap_err();
        assert_eq!(e.code, "23503", "{workers}: {}", e.message);
    }
}
