//! Citations and read records (PLAN.md §M14.24 R5): `recall`'s answers
//! carry the commit they read, which `AS OF COMMIT` reads again; the
//! queries serving a request are recorded with what they returned; and
//! `FORGET` reaches both the history kept and those records.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::Platform;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::{Output, Session, Value};

fn db(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::clone(p) as Arc<dyn Platform>,
            Path::new("/db"),
            Options {
                history_time: Duration::from_secs(3600),
                ..Options::default()
            },
        )
        .unwrap(),
    )
}

fn texts(s: &mut Session, sql: &str) -> Vec<String> {
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

fn store(s: &mut Session) {
    s.execute(
        "CREATE MODEL h USING hashing WITH (dimensions = 64); \
         CREATE MEMORY STORE notes USING h WITH (merge = 0.99); \
         SET embedding.pending = wait",
    )
    .unwrap();
    for (who, what) in [
        ("ann", "likes green tea"),
        ("ann", "drinks tea at noon"),
        ("ann", "works in Lisbon"),
        ("bob", "likes black tea"),
    ] {
        s.execute(&format!("REMEMBER '{what}' FOR '{who}' IN notes"))
            .unwrap();
    }
}

#[test]
fn a_citation_reads_again_what_was_cited() {
    let p = Arc::new(SimPlatform::new(1));
    let mut s = Session::new(db(&p));
    store(&mut s);
    let cited = texts(
        &mut s,
        "SELECT id, content, seen, as_of FROM recall('notes', 'ann', 'tea') ORDER BY content",
    );
    assert_eq!(cited.len(), 3);
    let as_of: Vec<&str> = cited
        .iter()
        .map(|r| r.rsplit(' ').next().unwrap())
        .collect();
    assert!(as_of.iter().all(|a| *a == as_of[0]), "{as_of:?}");
    let commit = as_of[0];
    // The memories move on: one replaced, one merged into, one forgotten.
    p.clock().sleep(Duration::from_secs(5));
    let id_of = |row: &str| row.split(' ').next().unwrap().to_string();
    let green = cited.iter().find(|r| r.contains("green")).unwrap();
    s.execute(&format!(
        "REMEMBER 'likes white tea' FOR 'ann' IN notes REPLACING '{}'",
        id_of(green)
    ))
    .unwrap();
    s.execute("REMEMBER 'drinks tea at noon' FOR 'ann' IN notes")
        .unwrap();
    s.execute("DELETE FROM notes WHERE content = 'works in Lisbon'")
        .unwrap();
    assert_ne!(
        texts(
            &mut s,
            "SELECT id, content, seen FROM recall('notes', 'ann', 'tea') ORDER BY content"
        ),
        cited
            .iter()
            .map(|r| r.rsplit_once(' ').unwrap().0.to_string())
            .collect::<Vec<_>>()
    );
    // Each citation, by its id and commit: what was cited.
    for row in &cited {
        let id = id_of(row);
        assert_eq!(
            texts(
                &mut s,
                &format!(
                    "SELECT id, content, seen, {commit} FROM notes AS OF COMMIT {commit} \
                     WHERE id = '{id}'"
                )
            ),
            std::slice::from_ref(row)
        );
    }
    // The whole answer again, as the system knew it then.
    s.execute(&format!("BEGIN READ ONLY AS OF COMMIT {commit}"))
        .unwrap();
    assert_eq!(
        texts(
            &mut s,
            "SELECT id, content, seen, as_of FROM recall('notes', 'ann', 'tea') ORDER BY content"
        ),
        cited
    );
    s.execute("COMMIT").unwrap();
}

#[test]
fn the_queries_of_a_request_are_recorded_with_what_they_returned() {
    let p = Arc::new(SimPlatform::new(2));
    let mut s = Session::new(db(&p));
    store(&mut s);
    s.execute("CREATE USER app; GRANT SELECT ON notes TO app; GRANT USAGE ON MODEL h TO app")
        .unwrap();
    s.execute("SET request_id = 'gen-1'").unwrap();
    let got = texts(
        &mut s,
        "SELECT id, as_of FROM recall('notes', 'ann', 'tea') ORDER BY id",
    );
    // With parameters, as drivers send them.
    let stmt = nucleus_sql::parser::parse("SELECT id FROM recall('notes', $1, $2, 1)")
        .unwrap()
        .remove(0);
    let Output::Rows { rows, .. } = s
        .run(
            &stmt,
            &[Value::Text("bob".into()), Value::Text("tea".into())],
        )
        .unwrap()
    else {
        panic!()
    };
    let bob = rows[0][0].render();
    // Not a query: not a read.
    s.execute("REMEMBER 'reads novels' FOR 'ann' IN notes")
        .unwrap();
    s.execute("RESET request_id").unwrap();
    s.execute("SELECT count(*) FROM notes").unwrap();
    let reads = texts(
        &mut s,
        "SELECT request_id, as_of, array_length(ids, 1), parameters, statement, \
         tables, erasure IS NULL FROM nc_catalog.reads ORDER BY at, as_of",
    );
    assert_eq!(reads.len(), 2, "{reads:?}");
    let as_of = got[0].split(' ').nth(1).unwrap();
    assert_eq!(
        reads[0],
        format!(
            "gen-1 {as_of} 3  SELECT id, as_of FROM recall('notes', 'ann', 'tea') ORDER BY id {{notes}} t"
        )
    );
    assert!(
        reads[1].starts_with("gen-1 ") && reads[1].contains("$1 = 'bob', $2 = 'tea'"),
        "{reads:?}"
    );
    // The ids are those returned.
    let ids = texts(
        &mut s,
        "SELECT u.x FROM nc_catalog.reads r, unnest(r.ids) WITH ORDINALITY AS u(x, n) \
         ORDER BY r.at, r.as_of, u.n",
    );
    let mut want: Vec<String> = got
        .iter()
        .map(|r| r.split(' ').next().unwrap().to_string())
        .collect();
    want.push(bob);
    assert_eq!(ids, want);
    // A role sees its own reads (with the tables its checks looked at);
    // an administrator all.
    s.execute("SET ROLE app; SET request_id = 'gen-2'").unwrap();
    texts(&mut s, "SELECT id FROM recall('notes', 'ann', 'noon')");
    s.execute("RESET request_id").unwrap();
    assert_eq!(
        texts(
            &mut s,
            "SELECT request_id, user_name, tables FROM nc_catalog.reads"
        ),
        ["gen-2 app {notes}"]
    );
    s.execute("RESET ROLE").unwrap();
    assert_eq!(
        texts(&mut s, "SELECT count(*) FROM nc_catalog.reads").remove(0),
        "3"
    );
    // A request id on one line.
    assert_eq!(
        s.execute("SET request_id = E'a\\nb'")
            .unwrap_err()
            .code
            .to_string(),
        "22023"
    );
}

#[test]
fn forget_reaches_the_history_and_the_read_records() {
    let p = Arc::new(SimPlatform::new(3));
    let mut s = Session::new(db(&p));
    store(&mut s);
    s.execute("SET request_id = 'gen-9'").unwrap();
    let stmt = nucleus_sql::parser::parse("SELECT id FROM recall('notes', $1, 'tea')")
        .unwrap()
        .remove(0);
    s.run(&stmt, &[Value::Text("ann".into())]).unwrap();
    s.run(&stmt, &[Value::Text("bob".into())]).unwrap();
    s.execute("RESET request_id").unwrap();
    p.clock().sleep(Duration::from_secs(5));
    let then = texts(&mut s, "SELECT now()").remove(0);
    p.clock().sleep(Duration::from_secs(5));
    s.execute("FORGET SUBJECT 'ann'").unwrap();
    // The past within the retention no longer holds her memories: erasure
    // overrides the history kept.  Bob's are there.
    s.execute(&format!("BEGIN READ ONLY AS OF TIMESTAMP '{then}'"))
        .unwrap();
    assert_eq!(
        texts(
            &mut s,
            "SELECT subject, count(*) FROM notes GROUP BY subject ORDER BY subject"
        ),
        ["bob 1"]
    );
    s.execute("COMMIT").unwrap();
    // Her read record is blanked, its erasure named; Bob's stays.
    assert_eq!(
        texts(
            &mut s,
            "SELECT request_id, parameters, erasure IS NOT NULL FROM nc_catalog.reads ORDER BY at"
        ),
        ["NULL NULL t", "gen-9 $1 = 'bob' f"]
    );
}
