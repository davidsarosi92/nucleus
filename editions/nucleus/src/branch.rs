//! `nucleus branch <dir> create|list|drop|reset` (PLAN.md §M14.13 B3):
//! a local database's branches — a copy of it in constant time and space,
//! writable on its own.  A server's are made by SQL (`CREATE BRANCH`).

use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};

const USAGE: &str = "usage: nucleus branch <dir> create <name> [--from <branch>] | list | \
                     drop <name> | reset <name>";

/// Runs the command.
pub fn run(args: &[String]) -> Result<(), String> {
    let [dir, command, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db.clone());
    let sql = |e: nucleus_sql::Error| e.message;
    match (command.as_str(), rest) {
        ("list", []) => {
            for (name, parent, _) in nucleus_sql::branches::rows(&*kv).map_err(sql)? {
                match parent {
                    Some(p) => println!("{name}\t(from {p})"),
                    None => println!("{name}"),
                }
            }
        }
        ("create", [name]) => nucleus_sql::branches::create(&kv, name, None).map_err(sql)?,
        ("create", [name, flag, from]) if flag == "--from" => {
            nucleus_sql::branches::create(&kv, name, Some(from)).map_err(sql)?
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
