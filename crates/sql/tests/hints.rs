//! Hints beside errors (PLAN.md §M14.10): the common error codes come
//! with what to do about them.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;

fn session() -> Session {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut s = Session::new(kv);
    s.execute(
        "CREATE TABLE customers (id INT PRIMARY KEY, email TEXT NOT NULL, region TEXT); \
         INSERT INTO customers VALUES (1, 'a', 'r1'), (2, 'b', 'r2')",
    )
    .unwrap();
    s
}

/// The code and hint of `sql`'s error.
fn hint(s: &mut Session, sql: &str) -> (&'static str, Option<String>) {
    let e = s.execute(sql).unwrap_err();
    (e.code, e.hint)
}

#[test]
fn the_common_codes_have_hints() {
    let mut s = session();
    for (sql, code, hint_has) in [
        (
            "SELECT emial FROM customers",
            "42703",
            "Perhaps you meant column \"email\"",
        ),
        (
            "SELECT nothing FROM customers",
            "42703",
            "The columns here: id, email, region",
        ),
        (
            "SELECT c.regoin FROM customers c",
            "42703",
            "column \"region\"",
        ),
        (
            "SELECT x.id FROM customers c",
            "42P01",
            "table or alias \"c\"",
        ),
        (
            "SELECT * FROM custmers",
            "42P01",
            "Perhaps you meant table \"customers\"",
        ),
        ("SELECT * FROM elsewhere", "42P01", "The tables: customers"),
        (
            "SELECT * FROM nc_catalog.tabels",
            "42P01",
            "\"nc_catalog.tables\"",
        ),
        (
            "SELECT lowr(email) FROM customers",
            "42883",
            "Perhaps you meant lower()",
        ),
        (
            "SELECT cuont(*) FROM customers",
            "42883",
            "Perhaps you meant count()",
        ),
        (
            "SELECT substr(email) FROM customers",
            "42883",
            "takes 2 to 3 arguments",
        ),
        (
            "SELECT * FROM customers WHERE id = 'x'::text",
            "42883",
            "cast one side",
        ),
        ("SELECT 1/0", "22012", "NULLIF"),
        ("SELECT 2147483647::integer + 1", "22003", "a wider one"),
        ("SELECT (SELECT id FROM customers)", "21000", "LIMIT 1"),
        (
            "INSERT INTO customers VALUES (1, 'c', 'r3')",
            "23505",
            "another key",
        ),
        (
            "INSERT INTO customers (id) VALUES (3)",
            "23502",
            "Give the column a value",
        ),
        ("CREATE TABLE customers (id INT)", "42P07", "IF NOT EXISTS"),
    ] {
        let (c, h) = hint(&mut s, sql);
        assert_eq!(c, code, "{sql}");
        let h = h.unwrap_or_else(|| panic!("no hint for {sql}"));
        assert!(h.contains(hint_has), "{sql}: {h}");
    }
    // A failed transaction, and a privilege missing.
    s.execute("BEGIN").unwrap();
    let _ = s.execute("SELECT nope");
    let (c, h) = hint(&mut s, "SELECT 1");
    assert_eq!((c, h.unwrap().contains("ROLLBACK")), ("25P02", true));
    s.execute("ROLLBACK; CREATE USER other; SET ROLE other")
        .unwrap();
    let (c, h) = hint(&mut s, "SELECT * FROM customers");
    assert_eq!((c, h.unwrap().contains("GRANT")), ("42501", true));
}
