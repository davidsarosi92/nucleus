//! Index access methods as modules (PLAN.md §M14.12 V4b): a module adds a
//! method through the extension API, `CREATE INDEX … USING method (col
//! opclass) WITH (…)` names it, and the index is kept on every write —
//! inserts, updates, deletes, rollbacks, table rewrites — built over the
//! rows a table has, dropped with its prefix, and checked
//! by the integrity check through the method.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, KvTxn, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::catalog::INDEX_PREFIX_LEN;
use nucleus_sql::ext::catalog::{Index, Table};
use nucleus_sql::ext::*;
use nucleus_sql::{Output, Session};

/// A method that mirrors its column: an entry a row, under the index's
/// prefix and the row's primary key, holding the value's text.
struct Mirror;

fn entry(t: &Table, i: usize, pk: &[u8]) -> Vec<u8> {
    let mut k = t.index_prefix(t.indexes[i].id);
    k.extend_from_slice(pk);
    k
}

fn text(t: &Table, i: usize, row: &Row) -> Vec<u8> {
    row[t.indexes[i].columns[0]].render().into_bytes()
}

impl IndexMethod for Mirror {
    fn name(&self) -> &str {
        "mirror"
    }
    fn define(&self, t: &Table, ix: &mut Index) -> Result<()> {
        if ix.columns.len() != 1 || t.columns[ix.columns[0]].ty != DataType::Text {
            return Err(Error::feature("mirror indexes one text column"));
        }
        match ix.opclasses[0].as_deref() {
            None => ix.opclasses[0] = Some("mirror_ops".into()),
            Some("mirror_ops") => {}
            Some(o) => {
                return Err(Error::undefined_object(format!(
                    "operator class \"{o}\" does not exist for access method \"mirror\""
                )));
            }
        }
        for (k, _) in &ix.options {
            if k != "tag" {
                return Err(Error::invalid_parameter(format!(
                    "unrecognized parameter \"{k}\""
                )));
            }
        }
        Ok(())
    }
    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        i: usize,
        old: Option<RowRef>,
        new: Option<RowRef>,
    ) -> Result<()> {
        if let Some((pk, _)) = old {
            txn.delete(&entry(t, i, pk))?;
        }
        if let Some((pk, row)) = new {
            txn.put(&entry(t, i, pk), &text(t, i, row))?;
        }
        Ok(())
    }
    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let rows: Vec<(Vec<u8>, Row)> =
            nucleus_sql::index_method::rows(&*txn, t, None)?.collect::<Result<_>>()?;
        for (pk, row) in rows {
            txn.put(&entry(t, i, &pk), &text(t, i, &row))?;
        }
        Ok(())
    }
    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let prefix = t.index_prefix(t.indexes[i].id);
        let end = nucleus_kv::prefix_end(&prefix).unwrap();
        let mut n = 0;
        for e in txn.scan(
            std::ops::Bound::Included(&prefix),
            std::ops::Bound::Excluded(&end),
        )? {
            let (k, v) = e?;
            n += 1;
            let pk = &k[INDEX_PREFIX_LEN..];
            match rows.iter().find(|(p, _)| p == pk) {
                Some((_, row)) if text(t, i, row) == v => {}
                _ => problems.push(format!("{}: a wrong mirror entry", t.indexes[i].name)),
            }
        }
        if n != rows.len() {
            problems.push(format!(
                "{}: {n} mirror entries for {} rows",
                t.indexes[i].name,
                rows.len()
            ));
        }
        Ok(())
    }
}

struct MirrorModule;

impl Module for MirrorModule {
    fn name(&self) -> &str {
        "mirror-index"
    }
    fn register(&self, registry: &mut Registry) -> Result<()> {
        registry.add_index_method(Arc::new(Mirror))
    }
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).expect_err(sql).code.to_string()
}

fn problems(db: &Arc<dyn Kv>, s: &Session) -> Vec<String> {
    let txn = db.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    nucleus_sql::check::check(&*txn, s.registry()).unwrap()
}

/// The mirror's entries: (primary key bytes, text), in key order.
fn mirrored(db: &Arc<dyn Kv>, index: &str) -> Vec<String> {
    let txn = db.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    let (t, i) = nucleus_sql::catalog::find_index(&*txn, index)
        .unwrap()
        .unwrap();
    let prefix = t.index_prefix(t.indexes[i].id);
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    txn.scan(
        std::ops::Bound::Included(&prefix),
        std::ops::Bound::Excluded(&end),
    )
    .unwrap()
    .map(|e| String::from_utf8(e.unwrap().1).unwrap())
    .collect()
}

#[test]
fn a_module_s_index_method_is_kept_on_every_write() {
    let mut reg = Registry::builtin();
    reg.add(&MirrorModule).unwrap();
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(5)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::with_registry(Arc::clone(&db), Arc::new(reg));
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, s TEXT, n INT); \
         INSERT INTO t SELECT i, 'v' || i, i FROM generate_series(1, 5) i; \
         CREATE INDEX m ON t USING mirror (s) WITH (tag = 'x')",
    )
    .unwrap();
    // Built over the rows there.
    assert_eq!(mirrored(&db, "m"), ["v1", "v2", "v3", "v4", "v5"]);
    assert_eq!(
        rows(
            &mut s,
            "SELECT method, opclasses, options FROM nc_catalog.indexes WHERE name = 'm'"
        ),
        [["mirror", "{mirror_ops}", "{tag=x}"]]
    );
    // Kept on writes, rolled back with them.
    s.execute(
        "INSERT INTO t VALUES (6, 'v6', 6); \
         UPDATE t SET s = s || '!' WHERE id IN (2, 3); \
         UPDATE t SET n = 0 WHERE id = 4; \
         DELETE FROM t WHERE id = 1; \
         BEGIN; INSERT INTO t VALUES (7, 'v7', 7); DELETE FROM t WHERE id = 5; ROLLBACK",
    )
    .unwrap();
    assert_eq!(mirrored(&db, "m"), ["v2!", "v3!", "v4", "v5", "v6"]);
    assert_eq!(problems(&db, &s), Vec::<String>::new());
    // A table rewrite builds it again.
    s.execute("ALTER TABLE t ALTER COLUMN n TYPE BIGINT")
        .unwrap();
    assert_eq!(mirrored(&db, "m"), ["v2!", "v3!", "v4", "v5", "v6"]);
    // The integrity check asks the method.
    {
        let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
        let (t, i) = nucleus_sql::catalog::find_index(&*txn, "m")
            .unwrap()
            .unwrap();
        let mut k = t.index_prefix(t.indexes[i].id);
        k.extend_from_slice(b"\xff");
        txn.put(&k, b"stray").unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(problems(&db, &s).len(), 2, "{:?}", problems(&db, &s));
    // Emptied, then dropped with its prefix (the stray entry deleted
    // by hand: the method keeps only its rows').
    s.execute("DELETE FROM t; INSERT INTO t VALUES (1, 'a', 1)")
        .unwrap();
    {
        let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
        let (t, i) = nucleus_sql::catalog::find_index(&*txn, "m")
            .unwrap()
            .unwrap();
        let mut k = t.index_prefix(t.indexes[i].id);
        k.extend_from_slice(b"\xff");
        txn.delete(&k).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(mirrored(&db, "m"), ["a"]);
    assert_eq!(problems(&db, &s), Vec::<String>::new());
    // Refused: what the method refuses, what no method does, and the
    // B-tree's own refusals.
    for (sql, state) in [
        ("CREATE INDEX ON t USING mirror (n)", "0A000"),
        ("CREATE INDEX ON t USING mirror (s other_ops)", "42704"),
        ("CREATE INDEX ON t USING mirror (s) WITH (m = 4)", "22023"),
        ("CREATE UNIQUE INDEX ON t USING mirror (s)", "0A000"),
        ("CREATE INDEX ON t USING nothing (s)", "42704"),
        ("CREATE INDEX ON t USING btree (s text_ops)", "42704"),
        ("CREATE INDEX ON t (s) WITH (fillfactor = 70)", "22023"),
        (
            "ALTER TABLE t ALTER COLUMN s TYPE INT USING length(s)",
            "0A000",
        ),
    ] {
        assert_eq!(code(&mut s, sql), state, "{sql}");
    }
    s.execute("CREATE INDEX b ON t USING btree (n)").unwrap();
    s.execute("DROP INDEX m").unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, method FROM nc_catalog.indexes WHERE table_name = 't' ORDER BY 1"
        ),
        [["b", "btree"], ["t_pkey", "btree"]]
    );
    assert_eq!(problems(&db, &s), Vec::<String>::new());
}
