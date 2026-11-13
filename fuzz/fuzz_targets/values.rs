//! Values written as text: every type's input (`'…'::type`), the forms a
//! query's literals and a client's text parameters arrive in.
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use nucleus_sql::{Session, Value};

const TYPES: &[&str] = &[
    "boolean",
    "smallint",
    "integer",
    "bigint",
    "real",
    "double precision",
    "numeric",
    "numeric(12, 3)",
    "text",
    "varchar(5)",
    "char(3)",
    "bytea",
    "date",
    "time",
    "timestamp",
    "timestamptz",
    "interval",
    "uuid",
    "inet",
    "cidr",
    "json",
    "jsonb",
    "vector",
    "vector(3)",
    "sparsevec",
    "halfvec",
    "bit(8)",
    "hll",
    "ddsketch",
    "integer[]",
    "text[]",
    "numeric[]",
    "double precision[][]",
    "timestamptz[]",
    "tsvector",
    "tsquery",
    "regclass",
];

thread_local! {
    static S: RefCell<Session> = RefCell::new(Session::with_registry(
        nucleus_fuzz::database(),
        nucleus_fuzz::pg_registry(),
    ));
}

fuzz_target!(|data: &[u8]| {
    let Some((&t, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let ty = TYPES[t as usize % TYPES.len()];
    S.with_borrow_mut(|s| {
        let p = [Value::Text(text.into())];
        // In, and out again: what was accepted prints, and reads back
        // as itself.
        nucleus_fuzz::run(
            s,
            &format!("SELECT $1::{ty}, $1::{ty}::text::{ty} = $1::{ty}"),
            &p,
        );
    });
});
