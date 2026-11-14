//! `nucleus dump --schema [<dir>] [connection options of nucleus sql]`
//! (PLAN.md §M14.16 M1): the database's schema as the SQL that makes it
//! — `nc_catalog.definitions`, a statement a line, in an order that
//! replays on an empty database (`nucleus sql <new> < schema.sql`).

use crate::sql::{Res, Target};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: nucleus dump --schema [<dir> | :memory:] [connection options of nucleus sql]
The schema as SQL: models, types, tables, indexes, foreign keys, comments,
settings and policies, each as the statement that makes it — not roles,
grants, secrets or data (for the data: BACKUP TO, nucleus restore).
";

/// The rows of the one statement `sql`.
pub(crate) fn rows(
    backend: &mut crate::sql::Backend,
    sql: &str,
) -> Result<Vec<Vec<Option<String>>>> {
    match backend.execute(sql)?.pop() {
        Some(Ok(Res::Rows(t))) => Ok(t.rows),
        Some(Ok(Res::Command(tag))) => Err(format!("{sql}: no rows ({tag})")),
        Some(Err(f)) => Err(f.to_string()),
        None => Err(format!("{sql}: no result")),
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let mut target = Target::new("nucleus dump");
    let mut schema = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        if a == "--schema" {
            schema = true;
            continue;
        }
        if a == "--help" {
            print!("{USAGE}");
            return Ok(());
        }
        if !target.option(a, &mut value)? {
            return Err(format!("unknown option {a}\n{USAGE}"));
        }
    }
    if !schema {
        return Err(format!("only the schema is dumped (--schema)\n{USAGE}"));
    }
    let mut backend = target.open()?;
    let defs = rows(
        &mut backend,
        "SELECT definition FROM nc_catalog.definitions ORDER BY position",
    );
    let closed = backend.close();
    let mut out = String::new();
    for d in defs? {
        out.push_str(d[0].as_deref().unwrap_or_default());
        out.push_str(";\n");
    }
    print!("{out}");
    closed
}
