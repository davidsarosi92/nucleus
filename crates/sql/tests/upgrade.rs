//! A registered type's encoding upgraded on a populated database (PLAN.md
//! §M14.19 "done when"): old and new values read side by side, `ALTER
//! TYPE … UPGRADE` rewrites them all — atomically, crash-tested — an
//! upgrade that changes keys is done before the keys are used, and values
//! newer than the build are refused.
//!
//! The type's version is a switch, so one process plays successive
//! builds; everything runs in one test, in order.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering as AtomicOrdering};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_sql::encoding::{self, Reader};
use nucleus_sql::ext::*;
use nucleus_sql::{Output, Session, catalog};

/// The build's version of `counter`: 1 stores a `u32` little-endian, 2
/// and 3 big-endian; 3 also puts a `00` before each key.
static VERSION: AtomicU16 = AtomicU16::new(1);

fn version() -> u16 {
    VERSION.load(AtomicOrdering::SeqCst)
}

fn counter() -> TypeRef {
    TypeRef::named("counter").unwrap()
}

fn number(v: &Value) -> u32 {
    let b: [u8; 4] = v.ext_bytes().unwrap().try_into().unwrap();
    if version() == 1 {
        u32::from_le_bytes(b)
    } else {
        u32::from_be_bytes(b)
    }
}

fn value(n: u32) -> Value {
    let b = if version() == 1 {
        n.to_le_bytes()
    } else {
        n.to_be_bytes()
    };
    Value::ext(counter(), b.to_vec())
}

struct Counter;
struct ByNumber;
struct Keys;

impl TotalOrder for ByNumber {
    fn compare(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        Ok(number(a).cmp(&number(b)))
    }
}

impl KeyEncoding for Keys {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        if version() >= 3 {
            out.push(0);
        }
        out.extend_from_slice(&number(v).to_be_bytes());
    }
    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        let skip = usize::from(version() >= 3);
        let n = u32::from_be_bytes(b[skip..skip + 4].try_into().unwrap());
        Ok((value(n), skip + 4))
    }
}

impl Type for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Ok(value(
            text.trim()
                .parse()
                .map_err(|_| Error::invalid_text("not a counter"))?,
        ))
    }
    fn output(&self, v: &Value) -> String {
        number(v).to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&ByNumber)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Keys)
    }
    fn encoding_version(&self) -> u16 {
        version()
    }
    fn upgrade(&self, from: u16, bytes: &[u8]) -> Result<Vec<u8>> {
        let b: [u8; 4] = bytes.try_into().map_err(|_| Error::internal("short"))?;
        Ok(match from {
            1 => u32::from_le_bytes(b).to_be_bytes().to_vec(),
            _ => b.to_vec(), // 2 → 3: the same bytes
        })
    }
    fn upgrade_changes_keys(&self, from: u16) -> bool {
        version() >= 3 && from < 3
    }
}

struct Module_;

impl Module for Module_ {
    fn name(&self) -> &str {
        "counters"
    }
    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_type(Arc::new(Counter))
    }
}

fn registry() -> Arc<Registry> {
    let mut r = Registry::builtin();
    r.add(&Module_).unwrap();
    Arc::new(r)
}

fn open(p: &Arc<SimPlatform>) -> nucleus_kv::Result<Database> {
    Database::open_on(p.clone(), Path::new("/db"), Options::default())
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The encoding versions the stored rows of `c` hold their counter in.
fn stored_versions(kv: &dyn Kv) -> Vec<u16> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let t = catalog::table(&*txn, "c").unwrap();
    let prefix = t.index_prefix(0);
    let end = nucleus_kv::prefix_end(&prefix).unwrap();
    let mut out = Vec::new();
    for e in txn
        .scan(
            std::ops::Bound::Included(&prefix[..]),
            std::ops::Bound::Excluded(&end[..]),
        )
        .unwrap()
    {
        let (_, row) = e.unwrap();
        // The version, a u16 count, then `id` (skipped), then the counter:
        // tag 11, u32 id, u16 version.
        let mut r = Reader::new(&row[3..]);
        encoding::decode_value(&mut r).unwrap();
        let at = row.len() - r.remaining();
        assert_eq!(row[at], 11);
        out.push(u16::from_le_bytes([row[at + 5], row[at + 6]]));
    }
    out.sort();
    out
}

fn consistent(kv: &dyn Kv, reg: &Registry) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, reg).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

const ALL: &str = "SELECT id, n FROM c ORDER BY n";

#[test]
fn encodings_upgrade() {
    let reg = registry();
    let p = Arc::new(SimPlatform::new(5));

    // Build 1 writes three rows.
    VERSION.store(1, AtomicOrdering::SeqCst);
    {
        let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
        let mut s = Session::with_registry(kv.clone(), reg.clone());
        s.execute("CREATE TABLE c (id INT PRIMARY KEY, n counter)")
            .unwrap();
        s.execute("CREATE INDEX c_n ON c (n)").unwrap();
        s.execute("INSERT INTO c VALUES (1, '10'), (2, '20'), (3, '300')")
            .unwrap();
        assert_eq!(stored_versions(&*kv), [1, 1, 1]);
    }

    // Build 2 reads them (upgraded as read) and writes in version 2: old
    // and new side by side.
    VERSION.store(2, AtomicOrdering::SeqCst);
    let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
    let mut s = Session::with_registry(kv.clone(), reg.clone());
    assert_eq!(rows(&mut s, ALL), ["1 10", "2 20", "3 300"]);
    s.execute("INSERT INTO c VALUES (4, '15')").unwrap();
    assert_eq!(rows(&mut s, ALL), ["1 10", "4 15", "2 20", "3 300"]);
    assert_eq!(stored_versions(&*kv), [1, 1, 1, 2]);
    assert_eq!(rows(&mut s, "SELECT id FROM c WHERE n = '20'"), ["2"]);
    consistent(&*kv, &reg);
    assert_eq!(
        rows(
            &mut s,
            "SELECT encoding_version, stored_version FROM nc_catalog.types WHERE name = 'counter'"
        ),
        ["2 1"]
    );

    // ALTER TYPE … UPGRADE rewrites them all — atomically: a crash at any
    // I/O operation leaves either every row old or every row new.
    drop(s);
    drop(kv);
    let mut ops = 0u64;
    {
        let dry = Arc::new(SimPlatform::new(5));
        // Recreate the same state on a fresh simulation, then count the
        // upgrade's operations.
        replay_to_side_by_side(&dry, &reg);
        let kv: Arc<dyn Kv> = Arc::new(open(&dry).unwrap());
        let mut s = Session::with_registry(kv.clone(), reg.clone());
        s.execute("SELECT 1").unwrap();
        let before = dry.sim_fs().op_count();
        s.execute("ALTER TYPE counter UPGRADE").unwrap();
        ops += dry.sim_fs().op_count() - before;
    }
    assert!(ops > 0);
    for n in 1..=ops {
        let sim = Arc::new(SimPlatform::new(5));
        replay_to_side_by_side(&sim, &reg);
        let kv: Arc<dyn Kv> = Arc::new(open(&sim).unwrap());
        let mut s = Session::with_registry(kv.clone(), reg.clone());
        s.execute("SELECT 1").unwrap();
        sim.sim_fs().set_faults(Faults {
            crash_mode: CrashMode::Torn,
            ..Faults::default()
        });
        sim.sim_fs().crash_after_ops(n);
        let _ = s.execute("ALTER TYPE counter UPGRADE");
        drop(s);
        drop(kv);
        if sim.sim_fs().crash_count() == 0 {
            sim.sim_fs().crash();
        }
        let kv: Arc<dyn Kv> = Arc::new(open(&sim).unwrap());
        let versions = stored_versions(&*kv);
        assert!(
            versions == [1, 1, 1, 2] || versions == [2, 2, 2, 2],
            "crash at op {n}: {versions:?}"
        );
        let mut s = Session::with_registry(kv.clone(), reg.clone());
        assert_eq!(
            rows(&mut s, ALL),
            ["1 10", "4 15", "2 20", "3 300"],
            "op {n}"
        );
        consistent(&*kv, &reg);
    }

    // On the original database: upgrade, then build 3, whose keys differ:
    // a read-only statement is refused until a write upgrades the keys.
    let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
    let mut s = Session::with_registry(kv.clone(), reg.clone());
    s.execute("ALTER TYPE counter UPGRADE").unwrap();
    assert_eq!(stored_versions(&*kv), [2, 2, 2, 2]);
    drop(s);
    VERSION.store(3, AtomicOrdering::SeqCst);
    let mut ro = Session::with_registry(kv.clone(), reg.clone());
    ro.execute("BEGIN READ ONLY").unwrap();
    assert_eq!(ro.execute(ALL).unwrap_err().code, "25006");
    let mut s = Session::with_registry(kv.clone(), reg.clone());
    s.execute("INSERT INTO c VALUES (5, '1')").unwrap();
    assert_eq!(stored_versions(&*kv), [3, 3, 3, 3, 3]);
    consistent(&*kv, &reg);
    assert_eq!(rows(&mut s, ALL), ["5 1", "1 10", "4 15", "2 20", "3 300"]);
    assert_eq!(rows(&mut s, "SELECT id FROM c WHERE n = '15'"), ["4"]);

    // Build 2 again: the database is newer than it.
    VERSION.store(2, AtomicOrdering::SeqCst);
    let mut s = Session::with_registry(kv.clone(), reg.clone());
    assert_eq!(s.execute(ALL).unwrap_err().code, "0A000");
    VERSION.store(1, AtomicOrdering::SeqCst);
}

/// Rows 1–3 written by build 1, row 4 by build 2 (as above), on `p`.
fn replay_to_side_by_side(p: &Arc<SimPlatform>, reg: &Arc<Registry>) {
    VERSION.store(1, AtomicOrdering::SeqCst);
    {
        let kv: Arc<dyn Kv> = Arc::new(open(p).unwrap());
        let mut s = Session::with_registry(kv, reg.clone());
        s.execute("CREATE TABLE c (id INT PRIMARY KEY, n counter)")
            .unwrap();
        s.execute("CREATE INDEX c_n ON c (n)").unwrap();
        s.execute("INSERT INTO c VALUES (1, '10'), (2, '20'), (3, '300')")
            .unwrap();
    }
    VERSION.store(2, AtomicOrdering::SeqCst);
    let kv: Arc<dyn Kv> = Arc::new(open(p).unwrap());
    let mut s = Session::with_registry(kv, reg.clone());
    s.execute("INSERT INTO c VALUES (4, '15')").unwrap();
}

/// A column's type changes while a reader reads: the reader's snapshot
/// keeps the old column, a later one sees the new (one transaction under
/// MVCC — nothing blocks the reader).
#[test]
fn a_column_type_changes_while_a_reader_reads() {
    let p = Arc::new(SimPlatform::new(9));
    let kv: Arc<dyn Kv> = Arc::new(open(&p).unwrap());
    let mut w = Session::new(kv.clone());
    w.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
        .unwrap();
    w.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    let mut r = Session::new(kv.clone());
    r.execute("BEGIN").unwrap();
    assert_eq!(rows(&mut r, "SELECT n FROM t ORDER BY id"), ["10", "20"]);
    w.execute("ALTER TABLE t ALTER COLUMN n TYPE TEXT USING 'n' || n")
        .unwrap();
    assert_eq!(rows(&mut r, "SELECT n FROM t ORDER BY id"), ["10", "20"]);
    assert_eq!(rows(&mut r, "SELECT n + 1 FROM t WHERE id = 1"), ["11"]);
    r.execute("COMMIT").unwrap();
    assert_eq!(rows(&mut r, "SELECT n FROM t ORDER BY id"), ["n10", "n20"]);
    consistent(&*kv, &Registry::builtin());
}
