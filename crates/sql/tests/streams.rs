//! Streams (PLAN.md §M16.5 S2) beyond `slt/97_streams.slt`: a table
//! write and its event commit together (the outbox), appenders never
//! conflict and leave no gap, consumers of one group take every entry
//! exactly once with their work, the retention trims from the head
//! without reusing a number, a waiter wakes on a commit.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::Value;
use nucleus_sql::{Output, Session};

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

/// `sql`'s rows, each column an integer.
fn ints(s: &mut Session, sql: &str) -> Vec<Vec<i64>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Int(i) => *i,
                        v => panic!("{sql}: {v:?}"),
                    })
                    .collect()
            })
            .collect(),
        o => panic!("{sql}: {o:?}"),
    }
}

fn check(kv: &Arc<dyn Kv>, s: &Session) {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let problems = nucleus_sql::check::check(&*txn, s.registry()).unwrap();
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Runs `sql` until it is not refused by a conflict; how many were.
fn retried(s: &mut Session, sql: &str) -> usize {
    let mut conflicts = 0;
    loop {
        match s.execute(sql) {
            Ok(_) => return conflicts,
            Err(e) if e.code == "40001" => {
                conflicts += 1;
                let _ = s.execute("ROLLBACK");
            }
            Err(e) => panic!("{sql}: {} {}", e.code, e.message),
        }
    }
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn the_outbox_appenders_and_consumers_exactly_once() {
    const PRODUCERS: usize = 4;
    const EACH: usize = 60;
    const CONSUMERS: usize = 3;
    let kv = open(1);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute(
        "CREATE TABLE orders (id INT PRIMARY KEY, amount INT); \
         CREATE STREAM order_events (order_id INT NOT NULL, amount INT); \
         CREATE TABLE billed (order_id INT PRIMARY KEY, seq BIGINT NOT NULL, by_consumer INT)",
    )
    .unwrap();
    let total = PRODUCERS * EACH;
    let conflicts = std::thread::scope(|sc| {
        // Producers: each order and its event in one transaction; now and
        // then one rolled back (neither kept).
        for p in 0..PRODUCERS {
            let kv = Arc::clone(&kv);
            sc.spawn(move || {
                let mut s = Session::new(kv);
                for i in 0..EACH {
                    let id = p * EACH + i;
                    let write = format!(
                        "INSERT INTO orders VALUES ({id}, {i}); \
                         INSERT INTO order_events (order_id, amount) VALUES ({id}, {i})"
                    );
                    if i % 7 == 3 {
                        s.execute(&format!("BEGIN; {write}; ROLLBACK")).unwrap();
                    }
                    let n = retried(&mut s, &format!("BEGIN; {write}; COMMIT"));
                    assert_eq!(n, 0, "appends conflict with nothing");
                }
            });
        }
        // Consumers of one group: each batch's work (a row per entry)
        // and the group's position commit together.
        let handles: Vec<_> = (0..CONSUMERS)
            .map(|c| {
                let kv = Arc::clone(&kv);
                sc.spawn(move || {
                    let mut s = Session::new(kv);
                    let mut conflicts = 0;
                    let mut idle = 0;
                    while idle < 200 {
                        s.execute("BEGIN").unwrap();
                        let r =
                            s.execute("CONSUME order_id FROM order_events GROUP billing LIMIT 7");
                        let rows = match r {
                            Ok(mut out) => match out.pop().unwrap() {
                                Output::Rows { rows, .. } => rows,
                                o => panic!("{o:?}"),
                            },
                            Err(e) if e.code == "40001" => {
                                conflicts += 1;
                                let _ = s.execute("ROLLBACK");
                                continue;
                            }
                            Err(e) => panic!("{} {}", e.code, e.message),
                        };
                        if rows.is_empty() {
                            s.execute("ROLLBACK").unwrap();
                            idle += 1;
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        idle = 0;
                        let values: Vec<String> = rows
                            .iter()
                            .map(|r| {
                                let (Value::Int(seq), Value::Int(id)) = (&r[0], &r[1]) else {
                                    panic!("{r:?}")
                                };
                                format!("({id}, {seq}, {c})")
                            })
                            .collect();
                        // A duplicate here would be a 23505: never.
                        let done = s
                            .execute(&format!("INSERT INTO billed VALUES {}", values.join(", ")))
                            .and_then(|_| s.execute("COMMIT"));
                        match done {
                            Ok(_) => {}
                            Err(e) if e.code == "40001" => {
                                conflicts += 1;
                                let _ = s.execute("ROLLBACK");
                            }
                            Err(e) => panic!("{} {}", e.code, e.message),
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
    // Every order has its event, numbered 1.. without a gap, in no
    // other order than their commits'.
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*), min(seq), max(seq) FROM order_events"
        ),
        [[total as i64, 1, total as i64]]
    );
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*) FROM orders o JOIN order_events e ON e.order_id = o.id AND e.amount = o.amount"
        ),
        [[total as i64]]
    );
    // Each producer's events in its own order.
    assert_eq!(
        ints(
            &mut s,
            &format!(
                "SELECT count(*) FROM order_events a JOIN order_events b \
                 ON a.order_id / {EACH} = b.order_id / {EACH} AND a.order_id < b.order_id AND a.seq > b.seq"
            )
        ),
        [[0]]
    );
    // Billed exactly once each, by the consumers between them.
    assert_eq!(
        ints(&mut s, "SELECT count(*), count(DISTINCT seq) FROM billed"),
        [[total as i64, total as i64]]
    );
    assert_eq!(
        ints(&mut s, "SELECT stream_position('order_events', 'billing')"),
        [[total as i64]]
    );
    assert_eq!(
        ints(
            &mut s,
            "SELECT count(*) FROM billed b JOIN order_events e ON e.seq = b.seq AND e.order_id = b.order_id"
        ),
        [[total as i64]]
    );
    eprintln!("consumers' conflicts: {conflicts}");
    check(&kv, &s);
}

#[test]
fn retention_trims_from_the_head() {
    let kv = open(2);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE STREAM log (n INT) WITH (max_entries = 5)")
        .unwrap();
    for i in 1..=12 {
        s.execute(&format!("INSERT INTO log (n) VALUES ({i})"))
            .unwrap();
    }
    let now = nucleus_sql::datetime::from_system(kv.wall_clock());
    assert_eq!(nucleus_sql::stream::trim_all(&kv, now).unwrap(), 7);
    assert_eq!(nucleus_sql::stream::trim_all(&kv, now).unwrap(), 0);
    assert_eq!(
        ints(&mut s, "SELECT seq, n FROM log ORDER BY seq"),
        (8..=12).map(|i| vec![i, i]).collect::<Vec<_>>()
    );
    // A group behind the head reads on from what is left.
    assert_eq!(
        ints(&mut s, "CONSUME n FROM log GROUP late LIMIT 2"),
        [[8, 8], [9, 9]]
    );
    // By time: entries older than the retention (now seen an hour on).
    s.execute(
        "ALTER STREAM log SET (retention = '30 minutes'); ALTER STREAM log RESET (max_entries)",
    )
    .unwrap();
    assert_eq!(nucleus_sql::stream::trim_all(&kv, now).unwrap(), 0);
    let later = now + 3_600_000_000;
    assert_eq!(nucleus_sql::stream::trim_all(&kv, later).unwrap(), 5);
    assert_eq!(ints(&mut s, "SELECT count(*) FROM log"), [[0]]);
    // Numbers go on: never reused.
    s.execute("INSERT INTO log (n) VALUES (13)").unwrap();
    assert_eq!(ints(&mut s, "SELECT seq FROM log"), [[13]]);
    assert_eq!(
        ints(&mut s, "SELECT last_seq FROM nc_catalog.streams"),
        [[13]]
    );
    check(&kv, &s);
}

#[test]
#[cfg_attr(target_os = "wasi", ignore = "WASI has no threads")]
fn a_waiter_wakes_on_a_commit() {
    let kv = open(3);
    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE STREAM q (n INT)").unwrap();
    std::thread::scope(|sc| {
        let kv2 = Arc::clone(&kv);
        let waiter = sc.spawn(move || {
            let mut s = Session::new(kv2);
            let began = std::time::Instant::now();
            let got = ints(&mut s, "SELECT stream_wait('q', 0, 20000)");
            (got, began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        s.execute("INSERT INTO q (n) VALUES (1), (2)").unwrap();
        let (got, took) = waiter.join().unwrap();
        assert_eq!(got, [[2]]);
        assert!(
            took < Duration::from_secs(10),
            "woken by the commit: {took:?}"
        );
    });
}
