//! Parameters' types: a parameter takes the type its place gives it, one
//! for the statement — `date ± $1` a number of days, `timestamp + $1` an
//! interval, `interval * $1` a float — and a later coercion of it to the
//! other side's type does not change it (found by a fuzzer running each
//! query with its literals as parameters: `date + $1` was refused as
//! `date + date`, and `date - $1` typed an integer while it ran as days).

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::types::DataType;
use nucleus_sql::{Output, Session, Value};

fn session() -> Session {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    Session::new(kv)
}

/// The parameters' types and the result column's that `sql` is described
/// with, and its one value run with `params` (converted to those types, as
/// the server does), against the same query with the values as literals.
fn check(
    s: &mut Session,
    sql: &str,
    params: &[Value],
    want: (&[DataType], DataType),
    literal: &str,
) {
    let stmt = nucleus_sql::parser::parse(sql).unwrap().remove(0);
    let (types, columns) = s.describe(&stmt, None).unwrap();
    let types: Vec<DataType> = types.into_iter().map(Option::unwrap).collect();
    let column = columns.unwrap()[0].ty;
    assert_eq!((&types[..], column), want, "{sql}");
    let params: Vec<Value> = params
        .iter()
        .zip(&types)
        .map(|(v, t)| v.clone().cast(*t).unwrap())
        .collect();
    let one = |out: Output| match out {
        Output::Rows { mut rows, .. } => rows.remove(0).remove(0),
        o => panic!("{o:?}"),
    };
    let got = one(s.run(&stmt, &params).unwrap());
    let lit = one(s.execute(literal).unwrap().pop().unwrap());
    assert_eq!(got, lit, "{sql} with {params:?}, against {literal}");
}

#[test]
fn a_parameter_keeps_the_type_its_place_gave_it() {
    let mut s = session();
    s.execute("CREATE TABLE t (g date, ts timestamp, iv interval); INSERT INTO t VALUES (DATE '2026-01-01', TIMESTAMP '2026-01-01 10:00', INTERVAL '2 hours')")
        .unwrap();
    use DataType::*;
    let int = Value::Int(3);
    check(
        &mut s,
        "SELECT DATE '2026-01-01' + $1",
        std::slice::from_ref(&int),
        (&[Integer], Date),
        "SELECT DATE '2026-01-01' + 3",
    );
    check(
        &mut s,
        "SELECT DATE '2026-01-01' - $1",
        std::slice::from_ref(&int),
        (&[Integer], Date),
        "SELECT DATE '2026-01-01' - 3",
    );
    check(
        &mut s,
        "SELECT g - $1 FROM t",
        std::slice::from_ref(&int),
        (&[Integer], Date),
        "SELECT g - 3 FROM t",
    );
    check(
        &mut s,
        "SELECT g + $1 FROM t",
        &[Value::Text("3".into())],
        (&[Integer], Date),
        "SELECT g + 3 FROM t",
    );
    check(
        &mut s,
        "SELECT ts + $1 FROM t",
        &[Value::Text("1 day".into())],
        (&[Interval], Timestamp),
        "SELECT ts + INTERVAL '1 day' FROM t",
    );
    check(
        &mut s,
        "SELECT $1 + ts FROM t",
        &[Value::Text("1 day".into())],
        (&[Interval], Timestamp),
        "SELECT INTERVAL '1 day' + ts FROM t",
    );
    check(
        &mut s,
        "SELECT iv * $1 FROM t",
        &[Value::Int(2)],
        (&[Float], Interval),
        "SELECT iv * 2 FROM t",
    );
    // A parameter in two places: the first gave its type.
    check(
        &mut s,
        "SELECT g + $1 FROM t WHERE $1 > 0",
        &[int],
        (&[Integer], Date),
        "SELECT g + 3 FROM t WHERE 3 > 0",
    );
}
