//! The Rust nucleus codegen makes (scripts/driver-tests.sh codegen): it
//! compiles (warnings denied) and its structs take the rows.
#![deny(warnings)]

mod queries;
mod schema;

use nucleus_client::{Client, Config, Format, Reply, Value};
use schema::{CgEvents, CgEvery};

fn rows(db: &mut Client, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[], Format::Typed).expect("query").pop() {
        Some(Ok(Reply { rows, .. })) => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn main() {
    let config = Config {
        port: std::env::var("NUCLEUS_PORT").expect("NUCLEUS_PORT").parse().expect("port"),
        ..Config::default()
    };
    let mut db = Client::connect(config).expect("connect");
    let all: Vec<CgEvery> = rows(&mut db, &format!("{} ORDER BY id", CgEvery::SELECT))
        .into_iter()
        .map(|r| CgEvery::from_row(r).expect("a row"))
        .collect();
    let (a, b) = (&all[0], &all[1]);
    assert_eq!((a.id, a.b, a.s, a.i), (1, Some(true), Some(2), 3));
    assert_eq!((a.r, a.d), (Some(1.5), Some(2.5)));
    assert_eq!(a.n, Value::Numeric { mantissa: 1234, scale: 2 });
    assert_eq!((a.m.as_deref(), a.c.as_deref()), (Some("happy"), Some("abc")));
    assert_eq!(a.bin, Some(vec![1, 2]));
    assert!(a.dt.is_some() && a.ts.is_some() && a.tz.is_some() && a.tm.is_some() && a.u.is_some());
    assert_eq!(a.j.as_deref(), Some(r#"{"a": [1, 2]}"#));
    assert_eq!(a.vec, Some(vec![1.0, 2.0, 3.0]));
    assert!(matches!(a.iv, Value::Interval { .. }) && matches!(a.sv, Value::SparseVector { .. }));
    assert!(matches!(a.ip, Value::Inet { .. }) && matches!(a.net, Value::Cidr { .. }));
    assert_eq!(a.ia, Some(vec![Some(1), None, Some(3)]));
    assert_eq!(a.ta, Some(vec![Some("a".to_string()), Some("b".to_string())]));
    assert_eq!((a.select.as_deref(), a.two_words.as_deref()), (Some("sel"), Some("tw")));
    assert_eq!((b.i, &b.n, &b.ia), (7, &Value::Null, &None));
    let events: Vec<CgEvents> = rows(&mut db, CgEvents::SELECT)
        .into_iter()
        .map(|r| CgEvents::from_row(r).expect("an event"))
        .collect();
    assert!(events.iter().any(|e| e.kind.as_deref() == Some("start")));
    // The queries' functions (PLAN.md §M14.16 M6).
    let e = queries::every_by_id(&mut db, 1).expect("every_by_id").expect("a row");
    assert_eq!((e.m.as_deref(), e.two_words.as_deref()), (Some("happy"), Some("tw")));
    assert_eq!(e.n, Value::Numeric { mantissa: 1234, scale: 2 });
    assert!(queries::every_by_id(&mut db, 99).expect("every_by_id").is_none());
    let ids: Vec<i32> = queries::every_ids(&mut db, 0, 10).expect("every_ids").iter().map(|r| r.i).collect();
    assert_eq!(ids, [3, 7]);
    assert_eq!(queries::every_by_mood(&mut db, "happy").expect("every_by_mood").len(), 1);
    // 2025-06-01: days since 2000-01-01.
    assert_eq!(queries::add_event(&mut db, 9283, 13, "rust").expect("add_event"), 1);
    let renamed = queries::rename_event(&mut db, 13, None).expect("rename_event").expect("a row");
    assert_eq!((renamed.id, renamed.kind), (13, None));
    let kinds = queries::kinds_after(&mut db, 9132).expect("kinds_after");
    assert!(
        kinds.iter().any(|k| k.kind.as_deref() == Some("start") && k.n == 1 && k.t.as_deref() == Some("x")),
        "{kinds:?}"
    );
    println!("codegen rust: ok");
}
