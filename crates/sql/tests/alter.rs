//! The `ALTER` a migration needs (PLAN.md §M14.16 M3): renames, dropped
//! columns, defaults and `NOT NULL` — and everything that names a table
//! or a column keeping up, or the change refused.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn run(s: &mut Session, sql: &str) {
    s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn code(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(o) => panic!("{sql}: ran ({o:?})"),
        Err(e) => e.code.to_string(),
    }
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .unwrap()
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The definitions of the database, as `nucleus dump --schema` writes.
fn schema(s: &mut Session) -> Vec<String> {
    rows(
        s,
        "SELECT definition FROM nc_catalog.definitions ORDER BY position",
    )
}

/// The law of M1 still holds: the definitions replay to themselves.
fn assert_replays(s: &mut Session) {
    let defs = schema(s);
    let mut again = Session::new(db());
    for d in &defs {
        run(&mut again, d);
    }
    assert_eq!(schema(&mut again), defs);
}

#[test]
fn a_table_renamed_keeps_what_names_it() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE dept (id int PRIMARY KEY, name text)",
        "CREATE TABLE emp (id int PRIMARY KEY, dept_id int REFERENCES dept (id), boss int, pay int CHECK (emp.pay > 0))",
        "ALTER TABLE emp ADD CONSTRAINT boss_fk FOREIGN KEY (boss) REFERENCES emp (id)",
        "CREATE INDEX emp_pay ON emp (pay)",
        "CREATE ROLE app LOGIN",
        "GRANT SELECT ON emp TO app",
        "ALTER TABLE emp ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY mine ON emp USING (EXISTS (SELECT 1 FROM emp WHERE emp.boss = 1) OR pay < 100)",
        "CREATE POLICY depts ON dept USING (id IN (SELECT dept_id FROM emp))",
        "INSERT INTO dept VALUES (1, 'R&D')",
        "INSERT INTO emp VALUES (1, 1, NULL, 50), (2, 1, 1, 70)",
        "ALTER TABLE emp RENAME TO staff",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(code(&mut s, "SELECT * FROM emp"), "42P01");
    assert_eq!(rows(&mut s, "SELECT count(*) FROM staff"), ["2"]);
    // Its foreign keys, both ways and to itself, still hold.
    assert_eq!(
        code(&mut s, "INSERT INTO staff VALUES (3, 9, NULL, 1)"),
        "23503"
    );
    assert_eq!(
        code(&mut s, "INSERT INTO staff VALUES (3, 1, 9, 1)"),
        "23503"
    );
    assert_eq!(code(&mut s, "DELETE FROM dept"), "23503");
    assert_eq!(
        code(&mut s, "INSERT INTO staff VALUES (3, 1, NULL, 0)"),
        "23514"
    );
    let defs = schema(&mut s).join(";\n");
    assert!(defs.contains("CHECK (staff.pay > 0)"), "{defs}");
    assert!(
        defs.contains("SELECT 1 FROM staff WHERE staff.boss = 1"),
        "{defs}"
    );
    assert!(defs.contains("SELECT dept_id FROM staff"), "{defs}");
    assert!(defs.contains("REFERENCES staff (id)"), "{defs}");
    assert!(!defs.contains("emp "), "{defs}");
    // Its grant follows it, and its policies (without them, no row).
    run(&mut s, "SET ROLE app");
    assert_eq!(rows(&mut s, "SELECT count(*) FROM staff"), ["2"]);
    run(&mut s, "RESET ROLE");
    assert_eq!(code(&mut s, "ALTER TABLE staff RENAME TO dept"), "42P07");
    assert_replays(&mut s);
}

#[test]
fn a_column_renamed_keeps_what_names_it() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE t (id int PRIMARY KEY, a int CHECK (a >= 0), b int GENERATED ALWAYS AS (a * 2) STORED, email text)",
        "CREATE TABLE u (id int PRIMARY KEY, ta int REFERENCES t (id))",
        "CREATE UNIQUE INDEX t_email ON t (lower(email))",
        "CREATE INDEX t_a ON t (a)",
        "CREATE ROLE app LOGIN",
        "GRANT SELECT (id, a) ON t TO app",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY small ON t USING (a < 100)",
        "INSERT INTO t (id, a, email) VALUES (1, 5, 'X@y'), (2, 7, 'z@y')",
        "ALTER TABLE t RENAME COLUMN a TO amount",
        "ALTER TABLE t RENAME email TO mail",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(
        rows(&mut s, "SELECT id, amount, b FROM t ORDER BY id"),
        ["1 5 10", "2 7 14"]
    );
    assert_eq!(code(&mut s, "SELECT a FROM t"), "42703");
    assert_eq!(
        code(&mut s, "INSERT INTO t (id, amount) VALUES (3, -1)"),
        "23514"
    );
    assert_eq!(
        code(
            &mut s,
            "INSERT INTO t (id, amount, mail) VALUES (3, 1, 'x@Y')"
        ),
        "23505"
    );
    // The column's privilege follows it.
    run(&mut s, "SET ROLE app");
    assert_eq!(rows(&mut s, "SELECT amount FROM t ORDER BY id"), ["5", "7"]);
    assert_eq!(code(&mut s, "SELECT mail FROM t"), "42501");
    run(&mut s, "RESET ROLE");
    let defs = schema(&mut s).join(";\n");
    assert!(defs.contains("CHECK (amount >= 0)"), "{defs}");
    assert!(defs.contains("AS (amount * 2)"), "{defs}");
    assert!(defs.contains("(lower(mail))"), "{defs}");
    assert!(defs.contains("USING (amount < 100)"), "{defs}");
    assert_eq!(
        code(&mut s, "ALTER TABLE t RENAME COLUMN amount TO id"),
        "42701"
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE t RENAME COLUMN nope TO x"),
        "42703"
    );
    // Another table's policy that may name it: refused, with its name.
    run(&mut s, "ALTER TABLE u ENABLE ROW LEVEL SECURITY");
    run(
        &mut s,
        "CREATE POLICY via ON u USING (ta IN (SELECT id FROM t WHERE amount > 0))",
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE t RENAME COLUMN amount TO a"),
        "2BP01"
    );
    assert_replays(&mut s);
}

#[test]
fn constraints_and_indexes_renamed() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE p (id int PRIMARY KEY, code text UNIQUE)",
        "CREATE TABLE c (id int PRIMARY KEY, p_id int, n int, CONSTRAINT n_ok CHECK (n > 0), CONSTRAINT to_p FOREIGN KEY (p_id) REFERENCES p (id))",
        "CREATE INDEX c_n ON c (n)",
        "ALTER TABLE c RENAME CONSTRAINT n_ok TO n_positive",
        "ALTER TABLE c RENAME CONSTRAINT to_p TO c_parent",
        "ALTER INDEX c_n RENAME TO c_by_n",
        "ALTER INDEX IF EXISTS nope RENAME TO x",
    ] {
        run(&mut s, sql);
    }
    let defs = schema(&mut s).join(";\n");
    assert!(defs.contains("CONSTRAINT n_positive CHECK"), "{defs}");
    assert!(defs.contains("CONSTRAINT c_parent FOREIGN KEY"), "{defs}");
    assert!(defs.contains("CREATE INDEX c_by_n ON c"), "{defs}");
    // The parent knows the foreign key by its new name.
    assert_eq!(code(&mut s, "ALTER TABLE c DROP CONSTRAINT to_p"), "42704");
    run(&mut s, "ALTER TABLE c DROP CONSTRAINT c_parent");
    run(&mut s, "DROP TABLE p");
    assert_eq!(code(&mut s, "ALTER INDEX nope RENAME TO x"), "42704");
    assert_eq!(
        code(&mut s, "ALTER TABLE c RENAME CONSTRAINT nope TO x"),
        "42704"
    );
    assert_eq!(
        code(
            &mut s,
            "ALTER TABLE c RENAME CONSTRAINT n_positive TO c_by_n"
        ),
        "42710"
    );
    assert_replays(&mut s);
}

#[test]
fn a_column_dropped() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE t (id int PRIMARY KEY, a int, b text, c int CHECK (c > 0), d int, CONSTRAINT ab CHECK (a < 100 OR b IS NULL))",
        "CREATE INDEX t_b ON t (b)",
        "CREATE INDEX t_d ON t (d)",
        "CREATE INDEX t_bd ON t (b, d)",
        "CREATE INDEX t_expr ON t ((d + 1))",
        "CREATE STATISTICS t_bd_s ON b, d FROM t",
        "CREATE STATISTICS t_acd ON a, c, d FROM t",
        "INSERT INTO t VALUES (1, 1, 'x', 5, 10), (2, 2, 'y', 6, 20), (3, NULL, NULL, 7, 30)",
        "ALTER TABLE t DROP COLUMN b",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(
        rows(&mut s, "SELECT * FROM t ORDER BY id"),
        ["1 1 5 10", "2 2 6 20", "3 NULL 7 30"]
    );
    // Indexes after it still find their rows.
    assert_eq!(rows(&mut s, "SELECT id FROM t WHERE d = 20"), ["2"]);
    assert_eq!(rows(&mut s, "SELECT id FROM t WHERE d + 1 = 31"), ["3"]);
    assert_eq!(code(&mut s, "INSERT INTO t VALUES (4, 1, 0, 1)"), "23514");
    let defs = schema(&mut s).join(";\n");
    assert!(!defs.contains("t_b ") && !defs.contains("t_bd"), "{defs}");
    assert!(!defs.contains("CONSTRAINT ab"), "{defs}");
    assert!(
        defs.contains("CREATE STATISTICS t_acd ON a, c, d"),
        "{defs}"
    );
    assert!(defs.contains("CREATE INDEX t_d ON t (d)"), "{defs}");
    // New rows land right, in the indexes too.
    run(&mut s, "INSERT INTO t VALUES (4, 4, 8, 40)");
    assert_eq!(rows(&mut s, "SELECT id, c FROM t WHERE d = 40"), ["4 8"]);
    run(&mut s, "ALTER TABLE t DROP COLUMN IF EXISTS b");
    assert_eq!(code(&mut s, "ALTER TABLE t DROP COLUMN b"), "42703");
    assert_eq!(code(&mut s, "ALTER TABLE t DROP COLUMN id"), "2BP01");
    // In a transaction: rolled back whole.
    run(&mut s, "BEGIN");
    run(&mut s, "ALTER TABLE t DROP COLUMN a");
    run(&mut s, "ROLLBACK");
    assert_eq!(rows(&mut s, "SELECT a FROM t WHERE id = 2"), ["2"]);
    assert_replays(&mut s);
}

#[test]
fn a_dropped_column_others_name() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE p (id int PRIMARY KEY, code text UNIQUE, n int)",
        "CREATE TABLE c (id int PRIMARY KEY, p_code text REFERENCES p (code), p_id int REFERENCES p (id))",
        "CREATE TABLE g (id int PRIMARY KEY, x int, y int GENERATED ALWAYS AS (x + 1) STORED)",
        "INSERT INTO p VALUES (1, 'a', 1)",
        "INSERT INTO c VALUES (1, 'a', 1)",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(code(&mut s, "ALTER TABLE p DROP COLUMN code"), "2BP01");
    assert_eq!(code(&mut s, "ALTER TABLE g DROP COLUMN x"), "2BP01");
    run(&mut s, "ALTER TABLE g DROP COLUMN y");
    run(&mut s, "ALTER TABLE p DROP COLUMN code CASCADE");
    // The foreign key on it went; the one on the key after it holds.
    assert_eq!(code(&mut s, "INSERT INTO c VALUES (2, 'zz', 9)"), "23503");
    run(&mut s, "INSERT INTO c VALUES (2, 'zz', 1)");
    assert_eq!(code(&mut s, "DELETE FROM p"), "23503");
    // A policy naming it.
    run(&mut s, "ALTER TABLE p ENABLE ROW LEVEL SECURITY");
    run(&mut s, "CREATE POLICY small ON p USING (n < 10)");
    assert_eq!(code(&mut s, "ALTER TABLE p DROP COLUMN n"), "2BP01");
    run(&mut s, "ALTER TABLE p DROP COLUMN n CASCADE");
    assert_eq!(
        rows(&mut s, "SELECT count(*) FROM nc_catalog.policies"),
        ["0"]
    );
    assert_replays(&mut s);
}

#[test]
fn defaults_and_not_null() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE t (id int PRIMARY KEY, a int, b text DEFAULT 'x')",
        "INSERT INTO t (id) VALUES (1)",
        "ALTER TABLE t ALTER COLUMN a SET DEFAULT 7",
        "ALTER TABLE t ALTER b DROP DEFAULT",
        "INSERT INTO t (id) VALUES (2)",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(
        rows(&mut s, "SELECT id, a, b FROM t ORDER BY id"),
        ["1 NULL x", "2 7 NULL"]
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE t ALTER COLUMN a SET DEFAULT 'nope'"),
        "22P02"
    );
    assert_eq!(code(&mut s, "ALTER TABLE t ALTER a SET NOT NULL"), "23502");
    run(&mut s, "UPDATE t SET a = 0 WHERE a IS NULL");
    run(&mut s, "ALTER TABLE t ALTER a SET NOT NULL");
    assert_eq!(
        code(&mut s, "INSERT INTO t (id, a) VALUES (3, NULL)"),
        "23502"
    );
    run(&mut s, "ALTER TABLE t ALTER a DROP NOT NULL");
    run(&mut s, "INSERT INTO t (id, a) VALUES (3, NULL)");
    assert_eq!(
        code(&mut s, "ALTER TABLE t ALTER id DROP NOT NULL"),
        "42P16"
    );
    assert_replays(&mut s);
}

#[test]
fn a_partitioned_tables_columns_change_with_its_partitions() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE TABLE ev (at date NOT NULL, id int NOT NULL, kind text, note text, PRIMARY KEY (at, id)) PARTITION BY RANGE (at)",
        "CREATE TABLE ev_a PARTITION OF ev FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')",
        "CREATE TABLE ev_b PARTITION OF ev FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
        "INSERT INTO ev VALUES ('2025-05-01', 1, 'x', 'n1'), ('2026-05-01', 2, 'y', 'n2')",
        "ALTER TABLE ev DROP COLUMN note",
        "ALTER TABLE ev RENAME COLUMN kind TO type",
        "ALTER TABLE ev ALTER type SET DEFAULT 'none'",
        "INSERT INTO ev (at, id) VALUES ('2026-06-01', 3)",
        "ALTER TABLE ev RENAME TO events",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(
        rows(&mut s, "SELECT id, type FROM events ORDER BY id"),
        ["1 x", "2 y", "3 none"]
    );
    assert_eq!(code(&mut s, "ALTER TABLE ev_a DROP COLUMN type"), "42809");
    assert_eq!(code(&mut s, "ALTER TABLE events DROP COLUMN at"), "2BP01");
    assert_replays(&mut s);
}

#[test]
fn what_the_engine_keeps_is_kept() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE MODEL h USING hashing WITH (dimensions = 8)",
        "CREATE MEMORY STORE notes USING h",
        "ALTER TABLE notes ADD COLUMN topic text",
        "ALTER TABLE notes RENAME TO memos",
        "ALTER TABLE memos RENAME COLUMN topic TO subject_area",
    ] {
        run(&mut s, sql);
    }
    assert_eq!(
        code(&mut s, "ALTER TABLE memos DROP COLUMN content"),
        "42809"
    );
    assert_eq!(
        code(&mut s, "ALTER TABLE memos RENAME COLUMN content TO c"),
        "42809"
    );
    assert_eq!(
        code(&mut s, "ALTER INDEX memos_embedding RENAME TO x"),
        "42809"
    );
    run(&mut s, "ALTER TABLE memos DROP COLUMN subject_area");
    assert_replays(&mut s);
}

#[test]
fn a_column_dropped_before_embeddings_and_chunks() {
    let mut s = Session::new(db());
    for sql in [
        "CREATE MODEL h USING hashing WITH (dimensions = 8)",
        "CREATE TABLE docs (id int PRIMARY KEY, title text, body text, v vector GENERATED BY EMBEDDING (body) USING h)",
        "CREATE TABLE parts CHUNKS OF docs (body) BY sentences SIZE 50",
        // Its vector's job still queued when the column goes.
        "BEGIN; INSERT INTO docs VALUES (1, 't', 'One sentence. Another one.'); ALTER TABLE docs DROP COLUMN title; COMMIT",
        "INSERT INTO docs VALUES (2, 'A third sentence here.')",
    ] {
        run(&mut s, sql);
    }
    // Rows written before and after: their vectors made, their chunks
    // taken from the text column where it is now.
    assert_eq!(
        rows(&mut s, "SELECT id, v IS NOT NULL FROM docs ORDER BY id"),
        ["1 t", "2 t"]
    );
    let chunks = rows(&mut s, "SELECT chunk FROM parts ORDER BY 1");
    assert!(chunks.iter().any(|c| c.contains("third")), "{chunks:?}");
    assert_eq!(code(&mut s, "ALTER TABLE docs DROP COLUMN body"), "2BP01");
    assert_replays(&mut s);
}

/// A word that is the grammar's, not the column — `extract`'s field —
/// is left as it is.
#[test]
fn a_field_named_as_a_column_is_kept() {
    let mut s = Session::new(db());
    run(
        &mut s,
        "CREATE TABLE t (id int PRIMARY KEY, day int CHECK (day > 0), at date CHECK (extract(day from at) > 1))",
    );
    run(&mut s, "ALTER TABLE t RENAME COLUMN day TO dd");
    let defs = rows(&mut s, "SELECT definition FROM nc_catalog.definitions");
    assert!(
        defs[0].contains("CHECK (dd > 0)") && defs[0].contains("CHECK (extract(day from at) > 1)"),
        "{defs:?}"
    );
    assert_eq!(
        code(&mut s, "INSERT INTO t VALUES (1, 1, '2024-01-01')"),
        "23514"
    );
    assert_replays(&mut s);
}
