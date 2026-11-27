//! Sequences (issue #23): values handed out by many sessions at once are
//! each handed out once, with no gap; after a crash anywhere the sequence
//! goes on past every value a committed row holds.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};
use nucleus_sql::{Output, Session, Value};

fn open(p: &Arc<SimPlatform>) -> Arc<dyn Kv> {
    Arc::new(Database::open_on(p.clone(), Path::new("/db"), Options::default()).unwrap())
}

fn ints(s: &mut Session, sql: &str) -> Vec<i64> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
        .expect("an output")
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref v => panic!("{v:?}"),
            })
            .collect(),
        o => panic!("{o:?}"),
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "threads")]
fn sessions_at_once_take_each_value_once() {
    let p = Arc::new(SimPlatform::new(1));
    let kv = open(&p);
    Session::new(kv.clone())
        .execute("CREATE SEQUENCE s")
        .unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let kv = kv.clone();
            std::thread::spawn(move || {
                let mut s = Session::new(kv);
                let mut got = Vec::new();
                for _ in 0..500 {
                    got.extend(ints(&mut s, "SELECT nextval('s')"));
                }
                // Its own last value.
                assert_eq!(ints(&mut s, "SELECT currval('s')"), [*got.last().unwrap()]);
                got
            })
        })
        .collect();
    let mut all: Vec<i64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    all.sort_unstable();
    assert_eq!(all, (1..=4000).collect::<Vec<_>>());
}

#[test]
fn after_a_reopen_values_go_on_past_the_last() {
    let p = Arc::new(SimPlatform::new(2));
    let kv = open(&p);
    let mut s = Session::new(kv.clone());
    s.execute("CREATE SEQUENCE s").unwrap();
    for _ in 0..10 {
        ints(&mut s, "SELECT nextval('s')");
    }
    drop(s);
    drop(kv);
    let kv = open(&p);
    let mut s = Session::new(kv.clone());
    let next = ints(&mut s, "SELECT nextval('s')")[0];
    // Past the reservation: a gap, never a value again.
    assert!((11..=10 + 32 + 1).contains(&next), "{next}");
    assert_eq!(ints(&mut s, "SELECT nextval('s')"), [next + 1]);
    // A value set: the next ones after it, a reopen or not.
    ints(&mut s, "SELECT setval('s', 100)");
    assert_eq!(ints(&mut s, "SELECT nextval('s')"), [101]);
    drop(s);
    drop(kv);
    let kv = open(&p);
    let mut s = Session::new(kv.clone());
    let next = ints(&mut s, "SELECT nextval('s')")[0];
    // (A reservation doubles while they come fast: 64 here.)
    assert!((102..=101 + 64 + 1).contains(&next), "{next}");
    // Altered: what was reserved by the old increment is not the new
    // one's — reopened, past the values the new one handed out.
    s.execute("CREATE SEQUENCE a").unwrap();
    assert_eq!(ints(&mut s, "SELECT nextval('a')"), [1]);
    s.execute("ALTER SEQUENCE a INCREMENT BY 1000").unwrap();
    let mut last = 0;
    for _ in 0..5 {
        last = ints(&mut s, "SELECT nextval('a')")[0];
    }
    assert_eq!(last, 5001);
    drop(s);
    drop(kv);
    let kv = open(&p);
    let mut s = Session::new(kv);
    let next = ints(&mut s, "SELECT nextval('a')")[0];
    assert!(next > last, "{next} after {last}");
}

/// Rows keyed by a sequence's values, the database crashed after every
/// count of file operations in turn: reopened, new rows never take a key
/// a committed row holds.
#[test]
fn a_crash_anywhere_never_hands_a_value_out_again() {
    let rows = |s: &mut Session| ints(s, "SELECT id FROM t ORDER BY id");
    let dry = Arc::new(SimPlatform::new(3));
    let ops = {
        let kv = open(&dry);
        let mut s = Session::new(kv);
        s.execute("CREATE SEQUENCE s").unwrap();
        s.execute("CREATE TABLE t (id bigint DEFAULT nextval('s') PRIMARY KEY, n int)")
            .unwrap();
        let before = dry.sim_fs().op_count();
        for i in 0..40 {
            s.execute(&format!("INSERT INTO t (n) VALUES ({i})"))
                .unwrap();
        }
        dry.sim_fs().op_count() - before
    };
    assert!(ops > 0);
    for n in (1..=ops).step_by(((ops / 60) as usize).max(1)) {
        let p = Arc::new(SimPlatform::new(3));
        let kv = open(&p);
        let mut s = Session::new(kv.clone());
        s.execute("CREATE SEQUENCE s").unwrap();
        s.execute("CREATE TABLE t (id bigint DEFAULT nextval('s') PRIMARY KEY, n int)")
            .unwrap();
        p.sim_fs().set_faults(Faults {
            crash_mode: CrashMode::Torn,
            ..Faults::default()
        });
        p.sim_fs().crash_after_ops(n);
        for i in 0..40 {
            if s.execute(&format!("INSERT INTO t (n) VALUES ({i})"))
                .is_err()
            {
                break;
            }
        }
        drop(s);
        drop(kv);
        if p.sim_fs().crash_count() == 0 {
            p.sim_fs().crash();
        }
        p.sim_fs().set_faults(Faults::default());
        let kv = open(&p);
        let mut s = Session::new(kv);
        let before = rows(&mut s);
        for i in 0..40 {
            s.execute(&format!("INSERT INTO t (n) VALUES ({i})"))
                .unwrap_or_else(|e| panic!("crash at op {n}: {e:?}"));
        }
        let after = rows(&mut s);
        assert_eq!(after.len(), before.len() + 40, "op {n}");
        let high = before.last().copied().unwrap_or(0);
        assert!(
            after[before.len()..].iter().all(|&id| id > high),
            "crash at op {n}: {before:?} then {after:?}"
        );
    }
}
