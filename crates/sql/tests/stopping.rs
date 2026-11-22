//! A server asked to stop (issue #17) ends its statements first, so the
//! checkpoint it takes next holds every commit and none comes after it:
//! once `budget::stop_all` is called, a statement running stops where it
//! looks at its deadline and every new one is refused, both with `57P01`.
//! (In a test binary of its own: the flag is the process's, never undone.)

use std::sync::Arc;
use std::time::{Duration, Instant};

use nucleus_kv::{Database, Kv};
use nucleus_sql::Session;

#[test]
fn statements_stop_when_the_server_stops() {
    let kv: Arc<dyn Kv> =
        Arc::new(Database::open_in_memory(nucleus_kv::Options::in_memory()).unwrap());
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, v TEXT); \
         INSERT INTO t SELECT i, 'v' || i FROM generate_series(1, 40000) i",
    )
    .unwrap();
    // A statement running: minutes of nested loop, without a timeout.
    let running = std::thread::spawn({
        let kv = Arc::clone(&kv);
        move || {
            let mut s = Session::new(kv);
            let started = Instant::now();
            let e = s
                .execute("SELECT count(*) FROM t a LEFT JOIN t b ON a.v < b.v")
                .map(|_| ())
                .unwrap_err();
            (e.code, started.elapsed())
        }
    });
    // And a transaction open, its write not committed.
    s.execute("BEGIN; UPDATE t SET v = 'open' WHERE id = 1")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    nucleus_sql::budget::stop_all();
    let (code, took) = running.join().unwrap();
    assert_eq!(code, "57P01");
    assert!(took < Duration::from_secs(5), "stopped after {took:?}");
    // Every statement refused from now on — a COMMIT too, so the open
    // transaction never commits.
    for sql in ["SELECT 1", "COMMIT", "INSERT INTO t VALUES (0, 'x')"] {
        let e = s.execute(sql).map(|_| ()).unwrap_err();
        assert_eq!(e.code, "57P01", "{sql}");
    }
    let mut fresh = Session::new(Arc::clone(&kv));
    assert_eq!(
        fresh.execute("SELECT 1").map(|_| ()).unwrap_err().code,
        "57P01"
    );
}
