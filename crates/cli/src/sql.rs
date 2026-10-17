//! `nucleus sql`: a SQL shell over a local database.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;

use nucleus_kv::{Database, Options};
use nucleus_sql::{Output, Session, TxnStatus};

type Result<T> = std::result::Result<T, String>;

/// Prints a result like psql's aligned format.
pub fn print(out: &mut dyn Write, output: &Output) -> std::io::Result<()> {
    match output {
        Output::Command(tag) => writeln!(out, "{tag}"),
        Output::Rows { columns, rows } => {
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| {
                            if v.is_null() {
                                String::new()
                            } else {
                                v.render()
                            }
                        })
                        .collect()
                })
                .collect();
            let mut widths: Vec<usize> = columns.iter().map(|c| c.name.chars().count()).collect();
            for r in &cells {
                for (w, c) in widths.iter_mut().zip(r) {
                    *w = (*w).max(c.chars().count());
                }
            }
            let line = |vals: Vec<&str>| -> String {
                vals.iter()
                    .zip(&widths)
                    .map(|(v, w)| format!(" {v}{} ", " ".repeat(w - v.chars().count())))
                    .collect::<Vec<_>>()
                    .join("|")
            };
            writeln!(
                out,
                "{}",
                line(columns.iter().map(|c| c.name.as_str()).collect())
            )?;
            writeln!(
                out,
                "{}",
                widths
                    .iter()
                    .map(|w| "-".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join("+")
            )?;
            for r in &cells {
                writeln!(out, "{}", line(r.iter().map(String::as_str).collect()))?;
            }
            writeln!(
                out,
                "({} row{})",
                rows.len(),
                if rows.len() == 1 { "" } else { "s" }
            )
        }
    }
}

fn run_text(session: &mut Session, sql: &str, out: &mut dyn Write) -> Result<bool> {
    let stmts = match nucleus_sql::parser::parse(sql) {
        Ok(s) => s,
        Err(e) => {
            writeln!(out, "ERROR:  {} ({})", e.message, e.code).map_err(|e| e.to_string())?;
            return Ok(false);
        }
    };
    let mut ok = true;
    for stmt in stmts {
        match session.run(&stmt, &[]) {
            Ok(o) => print(out, &o).map_err(|e| e.to_string())?,
            Err(e) => {
                writeln!(out, "ERROR:  {} ({})", e.message, e.code).map_err(|e| e.to_string())?;
                ok = false;
                break;
            }
        }
    }
    Ok(ok)
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(dir) = args.first() else {
        return Err("sql: missing <dir>".into());
    };
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let mut session = Session::new(Arc::new(db.clone()));
    let mut out = std::io::stdout().lock();
    let result = if args.get(1).map(String::as_str) == Some("-c") {
        let sql = args.get(2).ok_or("sql: -c needs a command")?;
        run_text(&mut session, sql, &mut out).and_then(|ok| {
            if ok {
                Ok(())
            } else {
                Err("the command failed".into())
            }
        })
    } else {
        shell(&mut session, &mut out)
    };
    drop(session);
    let closed = db.close().map_err(|e| e.to_string());
    result.and(closed)
}

fn shell(session: &mut Session, out: &mut dyn Write) -> Result<()> {
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        writeln!(
            out,
            "nucleus {} SQL — end statements with ;  (\\q quits)",
            env!("CARGO_PKG_VERSION")
        )
        .map_err(|e| e.to_string())?;
    }
    let mut buf = String::new();
    let mut line = String::new();
    let mut ok = true;
    loop {
        if interactive {
            let mark = match session.status() {
                TxnStatus::Idle => "",
                TxnStatus::InBlock => "*",
                TxnStatus::Failed => "!",
            };
            let prompt = if buf.trim().is_empty() {
                format!("nucleus{mark}=> ")
            } else {
                format!("nucleus{mark}-> ")
            };
            write!(out, "{prompt}")
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())?;
        }
        line.clear();
        if stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            break;
        }
        if buf.trim().is_empty() && matches!(line.trim(), "\\q" | "quit" | "exit") {
            break;
        }
        buf.push_str(&line);
        if buf.trim_end().ends_with(';') {
            ok &= run_text(session, &buf, out)?;
            buf.clear();
        }
    }
    if !buf.trim().is_empty() {
        ok &= run_text(session, &buf, out)?;
    }
    if ok || interactive {
        Ok(())
    } else {
        Err("a statement failed".into())
    }
}
