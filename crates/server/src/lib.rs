//! The nucleus server (PLAN.md §M12, §M14.7).
//!
//! - **The nucleus protocol** (`ncp`, DOCS §10) — nucleus's own, and the
//!   default ([`serve`], port 20035): typed values exactly as stored,
//!   SQLSTATE errors, the transaction status.  nucleus's drivers and
//!   `ncsql` speak it.
//! - **PostgreSQL compatibility** (optional, [`serve_pg`], `nucleus server
//!   --pg-compat`) — the PostgreSQL frontend/backend protocol version 3, so
//!   `psql` and PostgreSQL drivers connect too: startup (SSL and GSSAPI
//!   requests declined), trust or cleartext password, the simple and
//!   extended query protocols, text and binary formats.  Frozen (D14): kept
//!   working, not extended for tool-specific catalog queries.
//!
//! Both run statements through [`nucleus_sql::Session`], one thread per
//! connection.

mod native;
mod pg;

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_kv::Kv;

pub use native::handle as handle_native;
pub use pg::handle as handle_pg;

/// The nucleus protocol's default port (`NC` in ASCII).
pub const DEFAULT_PORT: u16 = 20_035;

/// The user name a server accepts unless told otherwise.
pub const DEFAULT_USER: &str = "nucleus";
/// The password a server accepts unless told otherwise.
pub const DEFAULT_PASSWORD: &str = "nucleus";

/// Server settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// The user clients log in as (with `password`).
    pub user: String,
    /// The password; `None`: no authentication at all.  Passwords travel
    /// in clear text: use them on trusted networks only.
    pub password: Option<String>,
    /// Print every statement a client sends, and every error, to stderr
    /// (for seeing what a tool asks).
    pub log_queries: bool,
}

impl Default for Config {
    /// User `nucleus`, password `nucleus` — what every nucleus client
    /// tries by default.
    fn default() -> Self {
        Self {
            user: DEFAULT_USER.into(),
            password: Some(DEFAULT_PASSWORD.into()),
            log_queries: false,
        }
    }
}

impl Config {
    /// No authentication: any user, no password.
    pub fn trust() -> Self {
        Self {
            password: None,
            ..Self::default()
        }
    }

    /// Whether `user` / `password` may log in.
    pub fn accepts(&self, user: &str, password: &str) -> bool {
        match &self.password {
            None => true,
            Some(pw) => user == self.user && password == pw,
        }
    }
}

type Handler = fn(TcpStream, Arc<dyn Kv>, &Config, u32) -> io::Result<()>;

fn accept(
    listener: TcpListener,
    kv: Arc<dyn Kv>,
    config: Config,
    handler: Handler,
    name: &'static str,
) -> io::Result<()> {
    let config = Arc::new(config);
    let ids = Arc::new(AtomicU32::new(1));
    for stream in listener.incoming() {
        let stream: TcpStream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let (kv, config) = (Arc::clone(&kv), Arc::clone(&config));
        let id = ids.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = stream.set_nodelay(true);
            if let Err(e) = handler(stream, kv, &config, id)
                && e.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("nucleus-server: {name} connection {id}: {e}");
            }
        });
    }
    Ok(())
}

/// Serves the nucleus protocol on `listener` forever, one thread per
/// connection.
pub fn serve(listener: TcpListener, kv: Arc<dyn Kv>, config: Config) -> io::Result<()> {
    accept(listener, kv, config, native::handle, "ncp")
}

/// Serves the PostgreSQL protocol (compatibility) on `listener` forever.
pub fn serve_pg(listener: TcpListener, kv: Arc<dyn Kv>, config: Config) -> io::Result<()> {
    accept(listener, kv, config, pg::handle, "pg")
}
