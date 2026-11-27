//! `INSERT … ON CONFLICT` (PLAN.md §M15 D2) beyond `slt/87_upsert.slt`:
//! writers racing on the same keys never see a unique violation and lose
//! no update; every storage; parameters in `DO UPDATE`; privileges and
//! row-level security as PostgreSQL 18 checks them (the codes below are
//! its answers to the same statements, in one session).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::{DataType, Value};
use nucleus_sql::{Output, Session};

/// The storages a table's rows may have.
const STORAGES: &[&str] = &["", "WITH (storage = column)", "WITH (engine = lsm)"];

fn open(seed: u64) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(seed)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn code(s: &mut Session, sql: &str) -> String {
    match s.execute(sql) {
        Ok(_) => "ok".into(),
        Err(e) => e.code.to_string(),
    }
}

/// The first column of `sql`'s rows, as integers.
fn ints(s: &mut Session, sql: &str) -> Vec<i64> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Int(i) => *i,
                v => panic!("{sql}: {v:?}"),
            })
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session, what: &str) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{what}: {problems:#?}");
}

/// Threads upserting the same few keys, each statement its own
/// transaction or several in a block (`block`), at an isolation level:
/// no statement fails but by a conflict (`40001`, run again), never a
/// unique violation, and every upsert counted once.
fn race(storage: &str, block: bool, isolation: &str) {
    const THREADS: usize = 4;
    const EACH: usize = 150;
    const KEYS: usize = 12;
    let kv = open(7);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(&format!(
        "CREATE TABLE hits (k INT PRIMARY KEY, n INT, tag TEXT UNIQUE) {storage}; \
         CREATE TABLE seen (id INT PRIMARY KEY, email TEXT UNIQUE) {storage}"
    ))
    .unwrap();
    let conflicts = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let kv = Arc::clone(&kv);
                sc.spawn(move || {
                    let mut s = Session::new(kv);
                    s.execute(&format!(
                        "SET default_transaction_isolation = '{isolation}'"
                    ))
                    .unwrap();
                    let mut conflicts = 0;
                    for i in 0..EACH {
                        let k = (i * 7 + t * 3) % KEYS;
                        let id = t * EACH + i;
                        let upserts = format!(
                            "INSERT INTO hits VALUES ({k}, 1, 'tag{k}') \
                             ON CONFLICT (k) DO UPDATE SET n = hits.n + 1; \
                             INSERT INTO seen VALUES ({id}, 'e{k}') ON CONFLICT (email) DO NOTHING"
                        );
                        let sql = if block {
                            format!("BEGIN; {upserts}; COMMIT")
                        } else {
                            upserts
                        };
                        loop {
                            match s.execute(&sql) {
                                Ok(_) => break,
                                Err(e) if e.code == "40001" => {
                                    conflicts += 1;
                                    if block {
                                        let _ = s.execute("ROLLBACK");
                                    }
                                }
                                Err(e) => panic!("{sql}: {} {}", e.code, e.message),
                            }
                        }
                    }
                    conflicts
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .sum::<usize>()
    });
    let what = format!("{storage} block={block} {isolation} ({conflicts} conflicts)");
    assert_eq!(
        ints(&mut s, "SELECT sum(n) FROM hits"),
        [(THREADS * EACH) as i64],
        "{what}"
    );
    assert_eq!(
        ints(&mut s, "SELECT count(*) FROM hits"),
        [KEYS as i64],
        "{what}"
    );
    assert_eq!(
        ints(&mut s, "SELECT count(*) FROM seen"),
        [KEYS as i64],
        "{what}"
    );
    check(&kv, &s, &what);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn racing_upserts_neither_violate_a_key_nor_lose_an_update() {
    for storage in STORAGES {
        race(storage, false, "read committed");
        race(storage, true, "repeatable read");
        race(storage, true, "serializable");
    }
}

#[test]
fn an_upsert_keeps_every_storage_and_its_indexes_whole() {
    for storage in STORAGES {
        let kv = open(1);
        let mut s = Session::new(Arc::clone(&kv));
        s.execute(&format!(
            "CREATE TABLE u (id INT PRIMARY KEY, email TEXT UNIQUE, n INT) {storage}; \
             CREATE INDEX u_n ON u (n); \
             INSERT INTO u SELECT i, 'e' || i, i FROM generate_series(1, 5000) i"
        ))
        .unwrap();
        // Half the rows there (updated, a key changed for some), half new.
        let n = ints(
            &mut s,
            "WITH up AS (INSERT INTO u SELECT i, 'e' || i, -i FROM generate_series(2501, 7500) i \
             ON CONFLICT (id) DO UPDATE SET n = excluded.n * 2, \
             email = CASE WHEN u.id % 10 = 0 THEN 'x' || u.id ELSE u.email END RETURNING 1) \
             SELECT count(*) FROM up",
        );
        assert_eq!(n, [5000], "{storage}");
        assert_eq!(
            ints(&mut s, "SELECT count(*) FROM u WHERE n < 0"),
            [5000],
            "{storage}"
        );
        assert_eq!(
            ints(&mut s, "SELECT count(*) FROM u WHERE email LIKE 'x%'"),
            [250],
            "{storage}"
        );
        // By the secondary key: the changed emails' old values are free.
        let n = ints(
            &mut s,
            "WITH up AS (INSERT INTO u VALUES (9000, 'e2510', 1), (9001, 'e2511', 1) \
             ON CONFLICT (email) DO NOTHING RETURNING id) SELECT id FROM up",
        );
        assert_eq!(n, [9000], "{storage}");
        // Through the secondary index: the updated row and a new one.
        assert_eq!(
            ints(&mut s, "SELECT id FROM u WHERE n = -5022 ORDER BY id"),
            [2511, 5022],
            "{storage}"
        );
        check(&kv, &s, storage);
    }
}

#[test]
fn a_parameter_in_do_update_takes_its_column_type() {
    let kv = open(2);
    let mut s = Session::new(kv);
    s.execute("CREATE TABLE c (k INT PRIMARY KEY, n BIGINT, note TEXT); INSERT INTO c VALUES (1, 10, 'a')")
        .unwrap();
    let sql = "INSERT INTO c AS t (k, n) VALUES ($1, 0) \
               ON CONFLICT (k) DO UPDATE SET n = t.n + $2, note = $3 RETURNING t.n, t.note";
    let stmt = nucleus_sql::parser::parse(sql).unwrap().remove(0);
    let (types, columns) = s.describe(&stmt, None).unwrap();
    let types: Vec<DataType> = types.into_iter().map(Option::unwrap).collect();
    assert_eq!(types, [DataType::Integer, DataType::BigInt, DataType::Text]);
    let columns = columns.unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty))
            .collect::<Vec<_>>(),
        [("n", DataType::BigInt), ("note", DataType::Text)]
    );
    let params = [Value::Int(1), Value::Int(5), Value::Text("b".into())];
    match s.run(&stmt, &params).unwrap() {
        Output::Rows { rows, .. } => {
            assert_eq!(rows, [vec![Value::Int(15), Value::Text("b".into())]]);
        }
        o => panic!("{o:?}"),
    }
}

#[test]
fn privileges_and_row_security_as_postgresql() {
    let kv = open(3);
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE d (id INT PRIMARY KEY, tenant TEXT, n INT, note TEXT); \
         INSERT INTO d VALUES (1, 'a', 0, 'x'), (2, 'b', 0, 'y'); \
         CREATE USER u1; CREATE USER u2; \
         GRANT SELECT, INSERT ON d TO u1; \
         GRANT SELECT, INSERT, UPDATE (n) ON d TO u2; \
         ALTER TABLE d ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY p_sel ON d FOR SELECT USING (true); \
         CREATE POLICY p_ins ON d FOR INSERT WITH CHECK (true); \
         CREATE POLICY p_upd ON d FOR UPDATE USING (tenant = 'a') WITH CHECK (n < 100)",
    )
    .unwrap();
    let up = |set: &str, id: i32| {
        format!("INSERT INTO d VALUES ({id}, 'a', 5, 'z') ON CONFLICT (id) DO UPDATE SET {set}")
    };
    // INSERT alone: DO NOTHING, not DO UPDATE.
    s.execute("SET ROLE u1").unwrap();
    assert_eq!(
        code(
            &mut s,
            "INSERT INTO d VALUES (1, 'a', 5, 'z') ON CONFLICT (id) DO NOTHING"
        ),
        "ok"
    );
    assert_eq!(code(&mut s, &up("n = d.n + 1", 1)), "42501");
    s.execute("RESET ROLE; SET ROLE u2").unwrap();
    // UPDATE on the column set; the row there through the policy's USING,
    // the new one through its check.
    assert_eq!(code(&mut s, &up("n = d.n + 1", 1)), "ok");
    assert_eq!(code(&mut s, &up("note = 'q'", 1)), "42501");
    assert_eq!(code(&mut s, &up("n = d.n + 1", 2)), "42501");
    assert_eq!(code(&mut s, &up("n = 500", 1)), "42501");
    assert_eq!(
        ints(
            &mut s,
            "INSERT INTO d VALUES (1, 'a', 5, 'z') ON CONFLICT (id) \
             DO UPDATE SET n = excluded.n WHERE d.note = 'x' RETURNING n"
        ),
        [5]
    );
    s.execute("RESET ROLE").unwrap();
    assert_eq!(ints(&mut s, "SELECT n FROM d ORDER BY id"), [5, 0]);
}
