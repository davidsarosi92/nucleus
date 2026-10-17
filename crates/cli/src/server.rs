//! `nucleus server`: the PostgreSQL-compatible server.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_kv::{Database, Options};
use nucleus_server::{Config, serve};

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first() else {
        return Err("server: missing <dir>".into());
    };
    let mut listen = "127.0.0.1:5432".to_string();
    let mut config = Config::default();
    let mut rest = args[1..].iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--listen" => listen = rest.next().ok_or("--listen needs an address")?.clone(),
            "--password" => {
                config.password = Some(rest.next().ok_or("--password needs a value")?.clone())
            }
            "--log-queries" => config.log_queries = true,
            other => return Err(format!("server: unknown option {other}")),
        }
    }
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let listener =
        TcpListener::bind(&listen).map_err(|e| format!("cannot listen on {listen}: {e}"))?;
    eprintln!(
        "nucleus {} listening on {} (database {dir})",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr().map_err(|e| e.to_string())?
    );
    serve(listener, Arc::new(db), config).map_err(|e| e.to_string())
}
