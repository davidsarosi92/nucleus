//! `nucleus init <dir> [--user <name>]`: claims a database — creates its
//! first administrator, with a password from `NUCLEUS_PASSWORD` or read
//! from standard input (PLAN.md §M14.25), or, given none, one made and
//! shown once (§M14.17 E2).  From then on the catalog's roles decide who
//! logs in, not the server's `--user` / `--password`.

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
            eprint!("password for {user} (empty: one made for you): ");
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            line.trim_end_matches(['\r', '\n']).to_string()
        }
    };
    let db = Database::open(Path::new(dir), Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    // None given: 20 letters and digits from the system's randomness
    // (116 bits).
    let made = password.is_empty();
    let password = if made {
        const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut out = String::new();
        while out.len() < 20 {
            let mut b = [0u8; 32];
            kv.random_bytes(&mut b);
            // Rejection sampling: no letter more likely than another.
            let limit = 256 - 256 % ALPHABET.len();
            out.extend(
                b.iter()
                    .filter(|&&x| (x as usize) < limit)
                    .map(|&x| ALPHABET[x as usize % ALPHABET.len()] as char),
            );
        }
        out.truncate(20);
        out
    } else {
        password
    };
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
    if made {
        println!("its password, shown this once: {password}");
    }
    println!("start the server:  nucleus server {dir}");
    println!("then connect:      nucleus sql -U {user} --password <its password>");
    Ok(())
}
