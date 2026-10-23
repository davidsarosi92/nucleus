//! The nucleus server (PLAN.md §M12, §M14.7): front ends over
//! [`nucleus_sql::Session`], one thread per connection.
//!
//! - A front end is a [`FrontEnd`], run by [`serve_front_end`]; the built-in
//!   one is **the nucleus protocol** ([`Ncp`], `ncp`, DOCS §10, port
//!   20035): typed values exactly as stored, SQLSTATE errors, the
//!   transaction status.  nucleus's drivers and `ncsql` speak it.  Others
//!   are modules — PostgreSQL's protocol is `nucleus-pg-compat`
//!   (`modules/pg-compat`).
//! - Who may log in is an [`Authenticator`] ([`Password`], [`Trust`]).

mod native;

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_kv::Kv;

pub use native::handle as handle_native;

/// The nucleus protocol's default port (`NC` in ASCII).
pub const DEFAULT_PORT: u16 = 20_035;

/// The user name a server accepts unless told otherwise.
pub const DEFAULT_USER: &str = "nucleus";
/// The password a server accepts unless told otherwise.
pub const DEFAULT_PASSWORD: &str = "nucleus";

/// Who may log in: an authentication method (PLAN.md principle 12; the
/// full set — roles, SCRAM, tokens, certificates — is §M14.25).
pub trait Authenticator: Send + Sync + std::fmt::Debug {
    /// How the server describes it at startup (`user nucleus`, `no
    /// authentication`).
    fn describe(&self) -> String;

    /// Whether a front end must ask the client for a password.
    fn wants_password(&self) -> bool;

    /// Whether `user` with `password` may log in to the database `kv`.
    fn accepts(&self, kv: &dyn Kv, user: &str, password: &str) -> bool;
}

/// Passwords: the roles of the database's catalog (`CREATE USER …
/// PASSWORD …`, checked against their SCRAM-SHA-256 verifiers) — or, while
/// the database is unclaimed (no administrator with a password yet,
/// [`nucleus_sql::roles`]), this one user.  Passwords travel in clear
/// text: use them on trusted networks only.
#[derive(Debug)]
pub struct Password {
    /// The user.
    pub user: String,
    /// Its password.
    pub password: String,
}

impl Authenticator for Password {
    fn describe(&self) -> String {
        format!("user {}", self.user)
    }
    fn wants_password(&self) -> bool {
        true
    }
    fn accepts(&self, kv: &dyn Kv, user: &str, password: &str) -> bool {
        let check = || -> nucleus_sql::Result<bool> {
            let txn = kv.begin(nucleus_kv::TxnOptions::read_only())?;
            if nucleus_sql::roles::unclaimed(&*txn)? {
                return Ok(user == self.user && password == self.password);
            }
            let now = nucleus_sql::datetime::from_system(kv.wall_clock());
            Ok(nucleus_sql::roles::authenticate(&*txn, user, password, now)?.is_ok())
        };
        check().unwrap_or(false)
    }
}

/// Anyone, without a password.
#[derive(Debug)]
pub struct Trust;

impl Authenticator for Trust {
    fn describe(&self) -> String {
        "no authentication".into()
    }
    fn wants_password(&self) -> bool {
        false
    }
    fn accepts(&self, _: &dyn Kv, _: &str, _: &str) -> bool {
        true
    }
}

/// Server settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Who may log in.
    pub auth: Arc<dyn Authenticator>,
    /// The functions, types and views sessions have (a custom build adds
    /// its modules here; front ends may add their own on top).
    pub registry: Arc<nucleus_sql::ext::Registry>,
    /// Print every statement a client sends, and every error, to stderr
    /// (for seeing what a tool asks).
    pub log_queries: bool,
}

impl Default for Config {
    /// User `nucleus`, password `nucleus` — what every nucleus client
    /// tries by default.
    fn default() -> Self {
        Self::password(DEFAULT_USER, DEFAULT_PASSWORD)
    }
}

impl Config {
    /// One user with a password.
    pub fn password(user: &str, password: &str) -> Self {
        Self {
            auth: Arc::new(Password {
                user: user.into(),
                password: password.into(),
            }),
            log_queries: false,
            registry: nucleus_sql::ext::Registry::shared_builtin(),
        }
    }

    /// No authentication: any user, no password.
    pub fn trust() -> Self {
        Self {
            auth: Arc::new(Trust),
            ..Self::password("", "")
        }
    }

    /// The same, with sessions on `registry`.
    pub fn with_registry(mut self, registry: Arc<nucleus_sql::ext::Registry>) -> Self {
        self.registry = registry;
        self
    }

    /// Whether `user` / `password` may log in to `kv`.
    pub fn accepts(&self, kv: &dyn Kv, user: &str, password: &str) -> bool {
        self.auth.accepts(kv, user, password)
    }
}

/// A client protocol: how the connections of one listener are served.
/// The server runs the front ends a configuration names
/// ([`FrontEnds::get`]); each is a module over [`nucleus_sql::Session`].
pub trait FrontEnd: Send + Sync {
    /// Its name (`ncp`, `pg`).
    fn name(&self) -> &str;

    /// Where it listens unless told otherwise.
    fn default_addr(&self) -> String;

    /// Serves one connection until it ends.
    fn handle(
        &self,
        stream: TcpStream,
        kv: Arc<dyn Kv>,
        config: &Config,
        id: u32,
    ) -> io::Result<()>;
}

/// The nucleus protocol (DOCS §10).
pub struct Ncp;

impl FrontEnd for Ncp {
    fn name(&self) -> &str {
        "ncp"
    }
    fn default_addr(&self) -> String {
        format!("127.0.0.1:{DEFAULT_PORT}")
    }
    fn handle(
        &self,
        stream: TcpStream,
        kv: Arc<dyn Kv>,
        config: &Config,
        id: u32,
    ) -> io::Result<()> {
        native::handle(stream, kv, config, id)
    }
}

/// The front ends a server can run, by name.
#[derive(Clone, Default)]
pub struct FrontEnds(Vec<Arc<dyn FrontEnd>>);

impl FrontEnds {
    /// The built-in one: `ncp`.
    pub fn builtin() -> Self {
        let mut f = Self::default();
        f.add(Arc::new(Ncp)).expect("the first");
        f
    }

    /// Adds a front end; fails if one has its name.
    pub fn add(&mut self, f: Arc<dyn FrontEnd>) -> Result<(), String> {
        if self.get(f.name()).is_some() {
            return Err(format!("front end {} is already registered", f.name()));
        }
        self.0.push(f);
        Ok(())
    }

    /// The front end called `name`.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn FrontEnd>> {
        self.0.iter().find(|f| f.name() == name)
    }

    /// Every name.
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|f| f.name()).collect()
    }
}

/// Serves `front` on `listener` forever, one thread per connection.
pub fn serve_front_end(
    front: Arc<dyn FrontEnd>,
    listener: TcpListener,
    kv: Arc<dyn Kv>,
    config: Config,
) -> io::Result<()> {
    let config = Arc::new(config);
    let ids = Arc::new(AtomicU32::new(1));
    for stream in listener.incoming() {
        let stream: TcpStream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let (kv, config, front) = (Arc::clone(&kv), Arc::clone(&config), Arc::clone(&front));
        let id = ids.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = stream.set_nodelay(true);
            if let Err(e) = front.handle(stream, kv, &config, id)
                && e.kind() != io::ErrorKind::UnexpectedEof
            {
                eprintln!("nucleus-server: {} connection {id}: {e}", front.name());
            }
        });
    }
    Ok(())
}

/// Serves the nucleus protocol on `listener` forever, one thread per
/// connection.
pub fn serve(listener: TcpListener, kv: Arc<dyn Kv>, config: Config) -> io::Result<()> {
    serve_front_end(Arc::new(Ncp), listener, kv, config)
}
