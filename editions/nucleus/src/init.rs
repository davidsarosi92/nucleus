//! `nucleus init <dir> [--user <name>]`: claims a database — creates its
//! first administrator, with a password from `NUCLEUS_PASSWORD` or read
//! from standard input (PLAN.md §M14.25).  From then on the catalog's
//! roles decide who logs in, not the server's `--user` / `--password`.

use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_sql::Session;

pub fn run(args: &[String]) -> Result<(), String> {
    let dir = args
        .first()
        .ok_or("usage: nucleus init <dir> [--user <name>]")?;
    let mut user = "admin".to_string();
    let mut rest = args[1..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--user" => user = rest.next().ok_or("--user needs a name")?.clone(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let password = match std::env::var("NUCLEUS_PASSWORD") {
        Ok(p) => p,
        Err(_) => {
            eprint!("password for {user}: ");
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim_end_matches(['\r', '\n']).to_string()
        }
    };
    if password.is_empty() {
        return Err("the password must not be empty".into());
    }
    let db = Database::open(Path::new(dir), Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    {
        let txn = kv
            .begin(TxnOptions::read_only())
            .map_err(|e| e.to_string())?;
        if !nucleus_sql::roles::unclaimed(&*txn).map_err(|e| e.to_string())? {
            return Err(format!(
                "{dir} already has an administrator; log in as one to add others"
            ));
        }
    }
    let quote = |s: &str| s.replace('"', "\"\"");
    let mut s = Session::new(kv);
    s.execute(&format!(
        "CREATE USER \"{}\" SUPERUSER PASSWORD '{}'",
        quote(&user),
        password.replace('\'', "''")
    ))
    .map_err(|e| e.to_string())?;
    println!("created the administrator {user}");
    Ok(())
}
