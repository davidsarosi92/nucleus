//! The transaction guarantees checked by Jepsen's history checker, Elle
//! (PLAN.md §M14.29 S5; DOCS §5.2): sessions run transactions of appends
//! to and reads of lists — Elle's list-append workload — at an isolation
//! level, on real files, and the history they make is written in Elle's
//! format (EDN) and checked.
//!
//! The checks here need no Java: each key's reads must be prefixes of one
//! another (one order of appends), every element read must have been
//! appended by a transaction that committed, and none twice.  With
//! `ELLE_JAR` (elle-cli's standalone jar; `ELLE_JAVA` the `java` to run
//! it, Java 21 or later) Elle checks the history for the cycles the level
//! forbids — in real time's order too, which one node keeps (a
//! transaction begins at the visible snapshot, and a commit returns once
//! it is visible): `SERIALIZABLE` as strict serializability, the other
//! levels as strong snapshot isolation:
//!
//! ```sh
//! ELLE_JAR=/abs/elle-cli-0.1.11-standalone.jar ELLE_TXNS=4000 \
//!     cargo test --release -p nucleus-sql --test elle -- --nocapture
//! ```
//!
//! `ELLE_OUT` keeps the histories in that directory (else a temporary
//! one, removed after).  The control — a write skew made on purpose under
//! snapshot isolation — must be found by Elle when it is asked for
//! serializability, so a checker that finds nothing is not taken for a
//! database that does nothing wrong.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nucleus_kv::{Database, Kv, Options};
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session, TxnStatus};

/// Lists appended to at once; a list takes this many appends, then a new
/// one takes its place (Jepsen's `key-count` and `max-writes-per-key`:
/// short lists keep the reads cheap, a few at a time make conflicts).
const ACTIVE: u64 = 8;
const APPENDS: u64 = 32;
/// Sessions running at once.
const SESSIONS: usize = 8;

/// A micro-operation: `Append(k, x)` or `Read(k, what was read)`.
#[derive(Clone, Debug)]
enum Mop {
    Append(u64, u64),
    Read(u64, Option<Vec<u64>>),
}

fn edn(mops: &[Mop]) -> String {
    let mut s = String::from("[");
    for (i, m) in mops.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        match m {
            Mop::Append(k, x) => write!(s, "[:append {k} {x}]").unwrap(),
            Mop::Read(k, None) => write!(s, "[:r {k} nil]").unwrap(),
            Mop::Read(k, Some(l)) => {
                let l: Vec<String> = l.iter().map(u64::to_string).collect();
                write!(s, "[:r {k} [{}]]", l.join(" ")).unwrap()
            }
        }
    }
    s.push(']');
    s
}

/// The history: each operation's invocation and completion, in the order
/// they happened (one lock orders them, so the real-time order is the
/// history's).
struct History {
    start: Instant,
    lines: Mutex<Vec<String>>,
    /// Completed transactions: (committed, mops).
    done: Mutex<Vec<(bool, Vec<Mop>)>>,
}

impl History {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            lines: Mutex::new(Vec::new()),
            done: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, ty: &str, process: usize, mops: &[Mop]) {
        let mut lines = self.lines.lock().unwrap();
        let index = lines.len();
        let time = self.start.elapsed().as_nanos();
        lines.push(format!(
            "{{:type :{ty}, :f :txn, :value {}, :process {process}, :time {time}, :index {index}}}",
            edn(mops)
        ));
    }
}

/// Which lists a transaction may touch: `ACTIVE` at a time, each retired
/// after `APPENDS` appends planned for it.
struct Keys {
    active: Vec<u64>,
    planned: HashMap<u64, u64>,
    next_key: u64,
    next_value: u64,
}

impl Keys {
    fn new() -> Self {
        Self {
            active: (0..ACTIVE).collect(),
            planned: HashMap::new(),
            next_key: ACTIVE,
            next_value: 1,
        }
    }

    /// A transaction of 1–4 mops, from `r` (random bits).
    fn plan(&mut self, mut r: impl FnMut() -> u64) -> Vec<Mop> {
        let n = 1 + r() % 4;
        let mut mops = Vec::new();
        for _ in 0..n {
            let slot = (r() % ACTIVE) as usize;
            let k = self.active[slot];
            if r().is_multiple_of(2) {
                mops.push(Mop::Read(k, None));
            } else {
                let x = self.next_value;
                self.next_value += 1;
                mops.push(Mop::Append(k, x));
                let p = self.planned.entry(k).or_default();
                *p += 1;
                if *p >= APPENDS {
                    self.active[slot] = self.next_key;
                    self.next_key += 1;
                }
            }
        }
        mops
    }
}

/// xorshift64*: the workload's choices, seeded per session.
fn rng(mut s: u64) -> impl FnMut() -> u64 {
    move || {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        s.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn text(out: &mut Vec<Output>) -> Option<String> {
    match out.pop() {
        Some(Output::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Text(t)) => Some(t.clone()),
            other => panic!("a list read as {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn list(t: &str) -> Vec<u64> {
    t.split(',')
        .filter(|x| !x.is_empty())
        .map(|x| x.parse().unwrap())
        .collect()
}

/// Runs one transaction's mops at `level`: `Ok(completed mops)` once
/// committed, `Err(())` once it failed with a serialization conflict (and
/// was rolled back).  Any other error is the test's failure.
fn run(s: &mut Session, level: &str, mops: &[Mop]) -> Result<Vec<Mop>, ()> {
    let conflict = |s: &mut Session, e: nucleus_sql::Error, what: &str| {
        assert_eq!(e.code, "40001", "{what}: {e:?}");
        if s.status() != TxnStatus::Idle {
            s.execute("ROLLBACK").unwrap();
        }
    };
    s.execute(&format!("BEGIN ISOLATION LEVEL {level}"))
        .unwrap();
    let mut done = Vec::with_capacity(mops.len());
    for m in mops {
        match m {
            Mop::Append(k, x) => {
                let sql = format!("UPDATE la SET v = v || '{x},' WHERE k = {k}");
                if let Err(e) = s.execute(&sql) {
                    conflict(s, e, &sql);
                    return Err(());
                }
                done.push(m.clone());
            }
            Mop::Read(k, _) => {
                // An odd list's read a key's lookup, an even one's a scan of
                // a range (what each records for a serializable commit to
                // validate differs).
                let sql = if k % 2 == 1 {
                    format!("SELECT v FROM la WHERE k = {k}")
                } else {
                    format!(
                        "SELECT v FROM la WHERE k >= {k} AND k < {} ORDER BY k",
                        k + 1
                    )
                };
                match s.execute(&sql) {
                    Ok(mut out) => {
                        let l = list(&text(&mut out).unwrap_or_default());
                        done.push(Mop::Read(*k, Some(l)));
                    }
                    Err(e) => {
                        conflict(s, e, &sql);
                        return Err(());
                    }
                }
            }
        }
    }
    match s.execute("COMMIT") {
        Ok(_) => Ok(done),
        Err(e) => {
            conflict(s, e, "COMMIT");
            Err(())
        }
    }
}

fn open(dir: &Path) -> Arc<dyn Kv> {
    Arc::new(Database::open(dir, Options::default()).unwrap())
}

/// The lists' table, with a row for every list the workload may reach.
fn create(kv: &Arc<dyn Kv>, lists: u64) {
    let mut s = Session::new(kv.clone());
    s.execute(&format!(
        "CREATE TABLE la (k BIGINT PRIMARY KEY, v TEXT NOT NULL); \
         INSERT INTO la SELECT i, '' FROM generate_series(0, {lists}) i"
    ))
    .unwrap();
}

/// `txns` transactions at `level` from `SESSIONS` sessions: the history.
fn workload(kv: &Arc<dyn Kv>, level: &'static str, txns: usize, seed: u64) -> Arc<History> {
    let history = Arc::new(History::new());
    let keys = Arc::new(Mutex::new(Keys::new()));
    let threads: Vec<_> = (0..SESSIONS)
        .map(|p| {
            let (kv, history, keys) = (kv.clone(), history.clone(), keys.clone());
            std::thread::spawn(move || {
                let mut s = Session::new(kv);
                let mut r = rng(seed ^ (p as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                for _ in 0..txns / SESSIONS {
                    let mops = keys.lock().unwrap().plan(&mut r);
                    history.record("invoke", p, &mops);
                    let result = run(&mut s, level, &mops);
                    match result {
                        Ok(done) => {
                            history.record("ok", p, &done);
                            history.done.lock().unwrap().push((true, done));
                        }
                        Err(()) => {
                            history.record("fail", p, &mops);
                            history.done.lock().unwrap().push((false, mops));
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    history
}

/// The checks that need no Elle: each key's reads are prefixes of its
/// longest, every element read was appended by a committed transaction,
/// none twice.
fn self_check(h: &History) {
    let done = h.done.lock().unwrap();
    let mut committed: HashMap<u64, u64> = HashMap::new(); // value → key
    let mut aborted: HashSet<u64> = HashSet::new();
    for (ok, mops) in done.iter() {
        for m in mops {
            if let Mop::Append(k, x) = m {
                if *ok {
                    committed.insert(*x, *k);
                } else {
                    aborted.insert(*x);
                }
            }
        }
    }
    let mut longest: HashMap<u64, Vec<u64>> = HashMap::new();
    for (ok, mops) in done.iter() {
        if !ok {
            continue;
        }
        for m in mops {
            let Mop::Read(k, Some(l)) = m else { continue };
            let mut seen = HashSet::new();
            for x in l {
                assert!(seen.insert(*x), "list {k} holds {x} twice: {l:?}");
                assert!(
                    !aborted.contains(x),
                    "list {k}: {x} of an aborted transaction read"
                );
                assert_eq!(
                    committed.get(x),
                    Some(k),
                    "list {k}: {x} never committed to it"
                );
            }
            let best = longest.entry(*k).or_default();
            let (short, long) = if l.len() <= best.len() {
                (l.as_slice(), best.as_slice())
            } else {
                (best.as_slice(), l.as_slice())
            };
            assert_eq!(&long[..short.len()], short, "list {k}: two orders");
            if l.len() > best.len() {
                *best = l.clone();
            }
        }
    }
}

/// Writes the history to `path`.
fn write(h: &History, path: &Path) {
    let mut out = h.lines.lock().unwrap().join("\n");
    out.push('\n');
    std::fs::write(path, out).unwrap();
}

/// Elle's verdict on the history at `path` for the consistency models
/// `models` (`None` without `ELLE_JAR`): whether it is valid, and Elle's
/// report.
fn elle(path: &Path, models: &str) -> Option<(bool, String)> {
    let jar = std::env::var("ELLE_JAR").ok()?;
    let java = std::env::var("ELLE_JAVA").unwrap_or_else(|_| "java".into());
    let out = std::process::Command::new(java)
        .args(["-jar", &jar, "--model", "list-append"])
        .args(["--consistency-models", models, "--output", "json"])
        .arg(path)
        .output()
        .expect("run elle-cli");
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    let valid = if report.contains("\"valid?\":true") {
        true
    } else if report.contains("\"valid?\":false") {
        false
    } else {
        panic!(
            "elle-cli: {}\n{report}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    Some((valid, report))
}

/// Where the histories go: `ELLE_OUT`, else a temporary directory.
fn out_dir(name: &str) -> (PathBuf, bool) {
    match std::env::var("ELLE_OUT") {
        Ok(d) => {
            std::fs::create_dir_all(&d).unwrap();
            (PathBuf::from(d), false)
        }
        Err(_) => {
            let d =
                std::env::temp_dir().join(format!("nucleus-elle-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            (d, true)
        }
    }
}

fn txns() -> usize {
    std::env::var("ELLE_TXNS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(800)
}

/// A workload at `level` checked against `models`: the self-check, then
/// Elle's when there is a jar.
fn check_level(name: &str, level: &'static str, models: &str) {
    let (dir, temp) = out_dir(name);
    let db = dir.join(format!("{name}.db"));
    assert!(!db.exists(), "{} exists", db.display());
    let kv = open(&db);
    let txns = txns();
    // Every list a workload can reach: each takes APPENDS appends.
    create(&kv, ACTIVE + (txns as u64 * 4) / APPENDS + 1);
    let h = workload(&kv, level, txns, 0x5EED ^ name.len() as u64);
    drop(kv);
    let done = h.done.lock().unwrap();
    let ok = done.iter().filter(|(ok, _)| *ok).count();
    println!("{name}: {} transactions, {ok} committed", done.len());
    assert!(ok > done.len() / 4, "too few commits to tell anything");
    drop(done);
    self_check(&h);
    let path = dir.join(format!("{name}.edn"));
    write(&h, &path);
    if let Some((valid, report)) = elle(&path, models) {
        println!("{name}: Elle ({models}): {valid}");
        assert!(
            valid,
            "{name}: Elle finds the history not {models}:\n{report}"
        );
    }
    if temp {
        std::fs::remove_dir_all(&dir).unwrap();
    } else {
        std::fs::remove_dir_all(&db).unwrap();
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn serializable() {
    check_level("serializable", "SERIALIZABLE", "strict-serializable");
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn repeatable_read_is_snapshot_isolation() {
    check_level(
        "repeatable-read",
        "REPEATABLE READ",
        "strong-snapshot-isolation",
    );
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn read_committed_is_snapshot_isolation() {
    check_level(
        "read-committed",
        "READ COMMITTED",
        "strong-snapshot-isolation",
    );
}

/// The control: a write skew — two transactions each reading the list the
/// other appends to — made on purpose under snapshot isolation, which
/// allows it; Elle, asked for serializability, must find it (`G2-item`),
/// and must not when asked for snapshot isolation.  Under `SERIALIZABLE`
/// the second commit fails.
#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_write_skew_is_found() {
    let (dir, temp) = out_dir("control");
    for level in ["REPEATABLE READ", "SERIALIZABLE"] {
        let db = dir.join(format!("control-{}.db", level.len()));
        let kv = open(&db);
        create(&kv, 2);
        let h = History::new();
        let (mut a, mut b) = (Session::new(kv.clone()), Session::new(kv.clone()));
        // Both lists start with an element, so the reads tell.
        let first = [Mop::Append(1, 1), Mop::Append(2, 2)];
        h.record("invoke", 0, &first);
        let done = run(&mut a, level, &first).unwrap();
        h.record("ok", 0, &done);
        h.done.lock().unwrap().push((true, done));
        let ta = [Mop::Read(1, None), Mop::Append(2, 3)];
        let tb = [Mop::Read(2, None), Mop::Append(1, 4)];
        h.record("invoke", 1, &ta);
        h.record("invoke", 2, &tb);
        a.execute(&format!("BEGIN ISOLATION LEVEL {level}"))
            .unwrap();
        b.execute(&format!("BEGIN ISOLATION LEVEL {level}"))
            .unwrap();
        let ra = list(&text(&mut a.execute("SELECT v FROM la WHERE k = 1").unwrap()).unwrap());
        let rb = list(&text(&mut b.execute("SELECT v FROM la WHERE k = 2").unwrap()).unwrap());
        a.execute("UPDATE la SET v = v || '3,' WHERE k = 2")
            .unwrap();
        b.execute("UPDATE la SET v = v || '4,' WHERE k = 1")
            .unwrap();
        let ca = a.execute("COMMIT");
        let cb = b.execute("COMMIT");
        assert!(ca.is_ok(), "{ca:?}");
        let done_a = vec![Mop::Read(1, Some(ra)), Mop::Append(2, 3)];
        h.record("ok", 1, &done_a);
        h.done.lock().unwrap().push((true, done_a));
        let skewed = cb.is_ok();
        if skewed {
            let done_b = vec![Mop::Read(2, Some(rb)), Mop::Append(1, 4)];
            h.record("ok", 2, &done_b);
            h.done.lock().unwrap().push((true, done_b));
        } else {
            assert_eq!(cb.unwrap_err().code, "40001");
            h.record("fail", 2, &tb);
            h.done.lock().unwrap().push((false, tb.to_vec()));
        }
        assert_eq!(skewed, level == "REPEATABLE READ", "{level}");
        // A last read of both, so every append is seen.
        let last = [Mop::Read(1, None), Mop::Read(2, None)];
        h.record("invoke", 0, &last);
        let done = run(&mut a, level, &last).unwrap();
        h.record("ok", 0, &done);
        h.done.lock().unwrap().push((true, done));
        drop((a, b, kv));
        self_check(&h);
        let path = dir.join(format!(
            "control-{}.edn",
            level.replace(' ', "-").to_lowercase()
        ));
        write(&h, &path);
        if let Some((valid, report)) = elle(&path, "serializable") {
            assert_eq!(valid, !skewed, "{level}:\n{report}");
            if skewed {
                assert!(report.contains("G2-item"), "{report}");
                let (si, report) = elle(&path, "snapshot-isolation").unwrap();
                assert!(si, "{report}");
            }
        }
        std::fs::remove_dir_all(&db).unwrap();
    }
    if temp {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
