//! Table storage as a module (PLAN.md §M14.20 V6): a module adds a storage
//! through the extension API, tables name it, and everything the SQL
//! layer does with rows — writes, keys and unique checks, secondary
//! indexes, foreign keys, scans a row and a batch at a time, in parallel,
//! the integrity check — goes through it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nucleus_kv::{Database, Kv, KvTxn, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::catalog::Table;
use nucleus_sql::ext::*;
use nucleus_sql::storage::RowStorage;
use nucleus_sql::{Output, Session};

static WRITES: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);

/// The row storage, counting what goes through it.
struct Counted;

impl TableStorage for Counted {
    fn name(&self) -> &str {
        "counted"
    }
    fn index_ids(&self) -> &[u32] {
        RowStorage.index_ids()
    }
    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        WRITES.fetch_add(1, Ordering::Relaxed);
        RowStorage.write(txn, t, key, row, old)
    }
    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        WRITES.fetch_add(1, Ordering::Relaxed);
        RowStorage.remove(txn, t, key)
    }
    fn exists(&self, txn: &dyn KvTxn, t: &Table, key: &[u8], validated: bool) -> Result<bool> {
        READS.fetch_add(1, Ordering::Relaxed);
        RowStorage.exists(txn, t, key, validated)
    }
    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        READS.fetch_add(1, Ordering::Relaxed);
        RowStorage.get(txn, t, key, wanted, labels)
    }
    fn rows<'a>(&self, scan: StorageScan<'a>) -> Result<RowIter<'a>> {
        READS.fetch_add(1, Ordering::Relaxed);
        RowStorage.rows(scan)
    }
    // Batches: the default, from the rows.
}

struct StorageModule;

impl Module for StorageModule {
    fn name(&self) -> &str {
        "counted-storage"
    }
    fn register(&self, registry: &mut Registry) -> Result<()> {
        registry.add_storage(Arc::new(Counted))
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

#[test]
fn a_module_s_storage_holds_tables() {
    let mut reg = Registry::builtin();
    reg.add(&StorageModule).unwrap();
    let reg = Arc::new(reg);
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(3)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::with_registry(Arc::clone(&db), Arc::clone(&reg));
    let mut plain = Session::with_registry(Arc::clone(&db), reg);
    for (sess, storage) in [(&mut s, "counted"), (&mut plain, "row")] {
        sess.execute(&format!(
            "CREATE TABLE p_{storage} (id INT PRIMARY KEY, name TEXT UNIQUE) WITH (storage = {storage}); \
             CREATE TABLE c_{storage} (id INT PRIMARY KEY, p INT REFERENCES p_{storage} (id), x BIGINT) \
               WITH (storage = {storage}); \
             CREATE INDEX ON c_{storage} (x); \
             INSERT INTO p_{storage} SELECT i, 'n' || i FROM generate_series(1, 300) i; \
             INSERT INTO c_{storage} SELECT i, 1 + i % 300, i * 7 % 1000 FROM generate_series(1, 20000) i; \
             UPDATE c_{storage} SET x = x + 1 WHERE id % 10 = 0; \
             DELETE FROM c_{storage} WHERE id % 7 = 0; \
             UPDATE p_{storage} SET name = name || '!' WHERE id < 5"
        ))
        .unwrap();
        sess.execute("SET parallel_workers = 4").unwrap();
    }
    assert!(
        WRITES.load(Ordering::Relaxed) > 20_000,
        "writes went through it"
    );
    for q in [
        "SELECT count(*), sum(x), min(p), max(x) FROM c_T",
        "SELECT p, count(*) FROM c_T WHERE x > 500 GROUP BY p ORDER BY p LIMIT 5",
        "SELECT id, x FROM c_T WHERE x = 77 ORDER BY id",
        "SELECT * FROM c_T WHERE id BETWEEN 100 AND 110",
        "SELECT p.name, count(*) FROM c_T c JOIN p_T p ON p.id = c.p WHERE p.id < 4 GROUP BY p.name ORDER BY 1",
        "SELECT name FROM p_T WHERE name = 'n1!'",
    ] {
        let reads = READS.load(Ordering::Relaxed);
        assert_eq!(
            rows(&mut s, &q.replace("_T", "_counted")),
            rows(&mut plain, &q.replace("_T", "_row")),
            "{q}"
        );
        assert!(READS.load(Ordering::Relaxed) > reads, "{q} read through it");
    }
    // Keys, unique indexes and foreign keys are checked through it too.
    for bad in [
        "INSERT INTO p_counted VALUES (1, 'again')",
        "INSERT INTO p_counted VALUES (999, 'n7')",
        "INSERT INTO c_counted VALUES (999999, 12345, 1)",
        "DELETE FROM p_counted WHERE id = 2",
    ] {
        assert!(s.execute(bad).is_err(), "{bad}");
    }
    let txn = db.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
    assert_eq!(
        nucleus_sql::check::check(&*txn, s.registry()).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT storage FROM nc_catalog.tables WHERE name = 'c_counted'"
        ),
        [["counted"]]
    );
}
