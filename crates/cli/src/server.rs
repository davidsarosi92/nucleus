//! `nucleus server`: the nucleus protocol, and optionally PostgreSQL
//! compatibility.

use std::net::TcpListener;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_server::{Config, DEFAULT_PORT, serve, serve_pg};

pub fn run(args: &[String]) -> Result<(), String> {
    let Some(dir) = args.first() else {
        return Err("server: missing <dir>".into());
    };
    let mut listen = format!("127.0.0.1:{DEFAULT_PORT}");
    let mut pg: Option<String> = None;
    let mut config = Config::default();
    let mut rest = args[1..].iter().peekable();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--listen" => listen = rest.next().ok_or("--listen needs an address")?.clone(),
            "--pg-compat" => {
                // An optional address follows.
                let addr = match rest.peek() {
                    Some(next) if !next.starts_with("--") => rest.next().cloned(),
                    _ => None,
                };
                pg = Some(addr.unwrap_or_else(|| "127.0.0.1:5432".into()));
            }
            "--user" => config.user = rest.next().ok_or("--user needs a name")?.clone(),
            "--password" => {
                config.password = Some(rest.next().ok_or("--password needs a value")?.clone())
            }
            "--no-auth" => config.password = None,
            "--log-queries" => config.log_queries = true,
            other => return Err(format!("server: unknown option {other}")),
        }
    }
    let db = Database::open(dir, Options::default()).map_err(|e| e.to_string())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    let bind =
        |addr: &str| TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"));
    let listener = bind(&listen)?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    let pg_listener = pg.as_deref().map(bind).transpose()?;
    eprintln!(
        "nucleus {} listening on {local} (nucleus protocol){} (database {dir}; {})",
        env!("CARGO_PKG_VERSION"),
        match &pg_listener {
            Some(l) => format!(
                " and {} (PostgreSQL compatibility)",
                l.local_addr().map_err(|e| e.to_string())?
            ),
            None => String::new(),
        },
        match &config.password {
            Some(_) => format!("user {}", config.user),
            None => "no authentication".into(),
        }
    );
    if let Some(l) = pg_listener {
        let (kv, config) = (Arc::clone(&kv), config.clone());
        std::thread::spawn(move || {
            if let Err(e) = serve_pg(l, kv, config) {
                eprintln!("nucleus: PostgreSQL listener stopped: {e}");
            }
        });
    }
    serve(listener, kv, config).map_err(|e| e.to_string())
}
