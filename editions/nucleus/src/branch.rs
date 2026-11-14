//! `nucleus branch <dir> create|list|drop|reset` (PLAN.md §M14.13 B3,
//! B4): a local database's branches — a copy of it in constant time and
//! space, writable on its own.  A server's are made by SQL (`CREATE
//! BRANCH`).

use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};

const USAGE: &str = "usage: nucleus branch <dir> create <name> [--from <branch>] \
                     [--at <time> --history <duration>] | list | drop <name> | reset <name>";

/// Runs the command.
pub fn run(args: &[String]) -> Result<(), String> {
    let [dir, command, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let (mut from, mut at, mut history) = (None, None, None);
    let mut positional = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n{USAGE}"))
        };
        match a.as_str() {
            "--from" if command == "create" => from = Some(value()?),
            "--at" if command == "create" => at = Some(value()?),
            "--history" if command == "create" => {
                let v = value()?;
                let us = nucleus_sql::dbsettings::duration(&v).map_err(|e| e.message)?;
                history = Some(std::time::Duration::from_micros(us as u64));
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}\n{USAGE}")),
            _ => positional.push(a.clone()),
        }
    }
    // A past time is readable as far as the history the server kept
    // reaches: its --history, given again (not stored with the database).
    if at.is_some() && history.is_none() {
        return Err("--at needs --history: the retention the server runs with \
                    (nucleus server --history), so only a time it kept is taken"
            .into());
    }
    let db = Database::open(
        dir,
        Options {
            history_time: history.unwrap_or_default(),
            ..Options::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let sql = |e: nucleus_sql::Error| e.message;
    match (command.as_str(), positional.as_slice()) {
        ("list", []) => {
            for (name, parent, _) in nucleus_sql::branches::rows(&*kv).map_err(sql)? {
                match parent {
                    Some(p) => println!("{name}\t(from {p})"),
                    None => println!("{name}"),
                }
            }
        }
        ("create", [name]) => {
            // `--at`: a time, as `AS OF TIMESTAMP` takes it.
            let registry = crate::sql::registry();
            nucleus_sql::branches::create(&kv, name, from.as_deref(), &|base| {
                at.as_ref()
                    .map(|t| {
                        let e = nucleus_sql::parser::parse_expr(&format!(
                            "'{}'::timestamptz",
                            t.replace('\'', "''")
                        ))?;
                        nucleus_sql::time_travel::snapshot(
                            base,
                            &registry,
                            &nucleus_sql::ast::AsOf::Time(e),
                        )
                    })
                    .transpose()
            })
            .map_err(sql)?
        }
        ("drop", [name]) => {
            nucleus_sql::branches::drop(&kv, name, false).map_err(sql)?;
        }
        ("reset", [name]) => nucleus_sql::branches::reset(&kv, name).map_err(sql)?,
        _ => return Err(USAGE.into()),
    }
    drop(kv);
    db.close().map_err(|e| e.to_string())
}
