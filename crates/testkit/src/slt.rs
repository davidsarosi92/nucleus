//! A sqllogictest-style test runner, over any [`Connection`] — a local
//! [`Session`] ([`Local`]), the nucleus protocol in the client's tests, the
//! PostgreSQL protocol in pg-compat's (PLAN.md §M12: the same files run
//! through all of them).
//!
//! File format (records separated by blank lines):
//!
//! ```text
//! # a comment
//! statement ok
//! CREATE TABLE t (a INT)
//!
//! statement error 23505
//! INSERT …              -- fails with this SQLSTATE (or message substring)
//!
//! query IT rowsort       -- column types: I int, R real, T text, B bool
//! SELECT a, b FROM t
//! ----
//! 1 one
//! 2 NULL
//!
//! connection other       -- switch to (or open) another session
//! ```
//!
//! Values print as: `NULL`, `(empty)` for an empty string, `true`/`false`,
//! reals with three decimals, the rest as text; a row is its values
//! separated by single spaces.  `rowsort` sorts the result rows before
//! comparing; `nosort` (the default) keeps their order.

use std::collections::HashMap;

use nucleus_sql::ext::{Output, Session, parse_with_text};

/// A result cell as text (`None` = SQL `NULL`).
pub type Cell = Option<String>;

/// A failed statement: SQLSTATE and message.
#[derive(Debug, Clone)]
pub struct SqlError {
    /// SQLSTATE.
    pub code: String,
    /// Message.
    pub message: String,
}

/// Something that runs SQL: one session.
pub trait Connection {
    /// Runs one statement; returns its rows (`None` for a command).
    fn run(&mut self, sql: &str) -> Result<Option<Vec<Vec<Cell>>>, SqlError>;
}

/// The local session as a [`Connection`].
pub struct Local(pub Session);

impl Connection for Local {
    fn run(&mut self, sql: &str) -> Result<Option<Vec<Vec<Cell>>>, SqlError> {
        let mut stmts = parse_with_text(sql).map_err(|e| SqlError {
            code: e.code.into(),
            message: e.message,
        })?;
        if stmts.len() != 1 {
            return Err(SqlError {
                code: "42601".into(),
                message: "expected one statement".into(),
            });
        }
        let (stmt, text) = stmts.remove(0);
        self.0.note_text(&text);
        match self.0.run(&stmt, &[]) {
            Ok(Output::Rows { rows, .. }) => Ok(Some(
                rows.into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|v| if v.is_null() { None } else { Some(v.render()) })
                            .collect()
                    })
                    .collect(),
            )),
            Ok(Output::Command(_)) => Ok(None),
            Err(e) => Err(SqlError {
                code: e.code.into(),
                message: e.message,
            }),
        }
    }
}

fn format_cell(cell: &Cell, ty: char) -> String {
    let Some(s) = cell else { return "NULL".into() };
    match ty {
        'R' => s
            .parse::<f64>()
            .map_or_else(|_| s.clone(), |f| format!("{f:.3}")),
        'B' => match s.as_str() {
            "t" | "true" => "true".into(),
            "f" | "false" => "false".into(),
            other => other.into(),
        },
        _ if s.is_empty() => "(empty)".into(),
        _ => s.clone(),
    }
}

/// Runs a test script; returns the failures (empty = pass).  `connect`
/// opens a new session (for `connection` records).
pub fn run_script(
    script: &str,
    name: &str,
    connect: &mut dyn FnMut() -> Box<dyn Connection>,
) -> Vec<String> {
    let mut conns: HashMap<String, Box<dyn Connection>> = HashMap::new();
    let mut current = "default".to_string();
    let mut failures = Vec::new();
    let lines: Vec<&str> = script.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let at = i + 1;
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words[0] == "connection" {
            current = words.get(1).copied().unwrap_or("default").to_string();
            continue;
        }
        // The SQL: lines up to a blank line or "----".
        let mut sql = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i].trim() != "----" {
            sql.push(lines[i]);
            i += 1;
        }
        let sql = sql.join("\n");
        let mut expected = Vec::new();
        if i < lines.len() && lines[i].trim() == "----" {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i].trim_end().to_string());
                i += 1;
            }
        }
        let conn = conns.entry(current.clone()).or_insert_with(&mut *connect);
        let here = format!("{name}:{at}");
        let result = conn.run(&sql);
        match (words[0], words.get(1).copied()) {
            ("statement", Some("ok")) => {
                if let Err(e) = result {
                    failures.push(format!(
                        "{here}: expected ok, got {} {}\n  {sql}",
                        e.code, e.message
                    ));
                }
            }
            ("statement", Some("error")) | ("query", Some("error")) => {
                let want = words[2..].join(" ");
                match result {
                    Ok(_) => {
                        failures.push(format!("{here}: expected error {want}, got ok\n  {sql}"))
                    }
                    Err(e) if !want.is_empty() && e.code != want && !e.message.contains(&want) => {
                        failures.push(format!(
                            "{here}: expected error {want}, got {} {}\n  {sql}",
                            e.code, e.message
                        ));
                    }
                    Err(_) => {}
                }
            }
            ("query", Some(types)) => {
                let sort = words.get(2).copied().unwrap_or("nosort");
                let types: Vec<char> = types.chars().collect();
                match result {
                    Err(e) => failures.push(format!(
                        "{here}: query failed: {} {}\n  {sql}",
                        e.code, e.message
                    )),
                    Ok(None) => failures.push(format!(
                        "{here}: query returned no rows (a command?)\n  {sql}"
                    )),
                    Ok(Some(rows)) => {
                        let mut got: Vec<String> = Vec::new();
                        let mut width_bad = false;
                        for r in &rows {
                            if r.len() != types.len() {
                                width_bad = true;
                            }
                            got.push(
                                r.iter()
                                    .enumerate()
                                    .map(|(j, c)| {
                                        format_cell(c, types.get(j).copied().unwrap_or('T'))
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            );
                        }
                        let mut want = expected.clone();
                        if sort == "rowsort" {
                            got.sort();
                            want.sort();
                        }
                        if width_bad {
                            failures.push(format!(
                                "{here}: rows do not have {} columns\n  {sql}",
                                types.len()
                            ));
                        } else if got != want {
                            failures.push(format!(
                                "{here}: wrong result\n  {sql}\n  expected:\n    {}\n  got:\n    {}",
                                want.join("\n    "),
                                got.join("\n    ")
                            ));
                        }
                    }
                }
            }
            _ => failures.push(format!("{here}: unknown record {line:?}")),
        }
    }
    failures
}
