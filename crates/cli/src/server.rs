//! `nucleus server`: the front ends a command line names — the nucleus
//! protocol by default, PostgreSQL compatibility on request.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_server::{
    Config, DEFAULT_PASSWORD, DEFAULT_USER, FrontEnds, Password, Trust, serve_front_end,
};

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first() else {
        return Err("server: missing <dir>".into());
    };
    let fronts = FrontEnds::builtin();
    // (front end, address): ncp always, on `--listen`'s address.
    let mut listen: Option<String> = None;
    let mut extra: Vec<(String, Option<String>)> = Vec::new();
    let (mut user, mut password) = (DEFAULT_USER.to_string(), Some(DEFAULT_PASSWORD.to_string()));
    let mut log_queries = false;
    let mut rest = args[1..].iter().peekable();
    // An optional address after a flag.
    let addr = |rest: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| match rest.peek() {
        Some(next) if !next.starts_with("--") => rest.next().cloned(),
        _ => None,
    };
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--listen" => listen = Some(rest.next().ok_or("--listen needs an address")?.clone()),
            "--pg-compat" => extra.push(("pg".into(), addr(&mut rest))),
            "--front-end" => {
                let name = rest.next().ok_or("--front-end needs a name")?.clone();
                extra.push((name, addr(&mut rest)));
            }
            "--user" => user = rest.next().ok_or("--user needs a name")?.clone(),
            "--password" => password = Some(rest.next().ok_or("--password needs a value")?.clone()),
            "--no-auth" => password = None,
            "--log-queries" => log_queries = true,
            other => return Err(format!("server: unknown option {other}")),
        }
    }
    let config = Config {
        auth: match password {
            Some(password) => Arc::new(Password { user, password }),
            None => Arc::new(Trust),
        },
        log_queries,
    };
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    let mut plan = vec![("ncp".to_string(), listen)];
    plan.extend(extra);
    let mut listeners = Vec::new();
    for (name, addr) in plan {
        let front = fronts.get(&name).ok_or_else(|| {
            format!(
                "server: no front end {name} (there are: {})",
                fronts.names().join(", ")
            )
        })?;
        let addr = addr.unwrap_or_else(|| front.default_addr());
        let l = TcpListener::bind(&addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        listeners.push((Arc::clone(front), l));
    }
    let shown: Vec<String> = listeners
        .iter()
        .map(|(f, l)| {
            let what = match f.name() {
                "ncp" => "nucleus protocol",
                "pg" => "PostgreSQL compatibility",
                other => other,
            };
            l.local_addr()
                .map(|a| format!("{a} ({what})"))
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    eprintln!(
        "nucleus {} listening on {} (database {dir}; {})",
        env!("CARGO_PKG_VERSION"),
        shown.join(" and "),
        config.auth.describe()
    );
    let mut listeners = listeners.into_iter();
    let (first, first_listener) = listeners.next().expect("ncp");
    for (front, l) in listeners {
        let (kv, config) = (Arc::clone(&kv), config.clone());
        std::thread::spawn(move || {
            let name = front.name().to_string();
            if let Err(e) = serve_front_end(front, l, kv, config) {
                eprintln!("nucleus: {name} listener stopped: {e}");
            }
        });
    }
    serve_front_end(first, first_listener, kv, config).map_err(|e| e.to_string())
}
