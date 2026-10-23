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
//! - TLS is a module's [`Tls`] (`nucleus-tls`), in [`Config::tls`]; the
//!   front ends negotiate it ([`transport`]).

mod native;
pub mod transport;

pub use nucleus_sql::access::Identity;
pub use transport::{Secured, Stream, Tls, TlsInfo};

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_kv::Kv;

pub use native::handle as handle_native;
/// SCRAM-SHA-256 (RFC 5802 / 7677), for front ends and their clients.
pub use nucleus_crypto::{ScramClient, ScramError, ScramServer};

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

    /// Whether `user` with `password` may log in to the database `kv` (a
    /// password sent in clear: `ncp` versions before 4).
    fn accepts(&self, kv: &dyn Kv, user: &str, password: &str) -> bool;

    /// The SCRAM-SHA-256 verifier `user` logs in to `kv` with (`None`: it
    /// may not — an unknown user, no password, no `LOGIN`, expired).
    fn verifier(&self, _kv: &dyn Kv, _user: &str) -> Option<nucleus_crypto::ScramVerifier> {
        None
    }

    /// Whether `user` — the common name of a client certificate the TLS
    /// handshake verified, the same name the client logs in as — may log in
    /// without a password (mutual TLS).
    fn accepts_certificate(&self, _kv: &dyn Kv, _user: &str) -> bool {
        false
    }
}

/// Bearer tokens — OAuth 2.0 / OpenID Connect identity tokens (JWTs): a
/// module provides it (`nucleus-oidc`).  A client sends one through the
/// OAUTHBEARER mechanism instead of proving a password; the token names
/// the user, the roles granted for the session and the claims the session
/// sees (`current_setting('nucleus.claims.<name>')`).
pub trait Bearer: Send + Sync + std::fmt::Debug {
    /// How the server describes it at startup.
    fn describe(&self) -> String;

    /// Who `token` says its holder is, for the database `kv` — or why it
    /// is not accepted (expired, another issuer or audience, a bad
    /// signature, …).
    fn verify(&self, kv: &dyn Kv, token: &str) -> Result<Identity, String>;
}

/// Whether a client logs in by its certificate: TLS verified one naming
/// `user`, and the authenticator admits that.
pub fn certificate_login(config: &Config, kv: &dyn Kv, tls: Option<&TlsInfo>, user: &str) -> bool {
    tls.and_then(|t| t.client_name.as_deref()) == Some(user)
        && config.auth.accepts_certificate(kv, user)
}

/// A stand-in salt for `user`, stable while the process runs and not
/// derivable without its secret, so an unknown name's exchange looks like a
/// known one's.
pub fn fake_salt(kv: &dyn Kv, user: &str) -> Vec<u8> {
    static SECRET: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let secret = SECRET.get_or_init(|| {
        let mut b = [0u8; 32];
        kv.random_bytes(&mut b);
        b
    });
    nucleus_crypto::hmac_sha256(secret, user.as_bytes())[..16].to_vec()
}

/// The server side of a SCRAM-SHA-256 login of `user` to `kv`, against
/// the verifier `config`'s authenticator gives — or, for a user that may
/// not log in, a stand-in exchange that ends in a refusal and looks like a
/// real one until then.  Front ends carry its messages in their own
/// framing (`ncp` version 4, PostgreSQL's SASL).
pub fn scram_server(config: &Config, kv: &dyn Kv, user: &str) -> ScramServer {
    ScramServer::new(
        config.auth.verifier(kv, user),
        &fake_salt(kv, user),
        &scram_nonce(kv),
    )
}

/// A random SCRAM nonce: printable, without commas.
pub fn scram_nonce(kv: &dyn Kv) -> String {
    let mut b = [0u8; 18];
    kv.random_bytes(&mut b);
    nucleus_crypto::base64_encode(&b)
}

/// Passwords: the roles of the database's catalog (`CREATE USER …
/// PASSWORD …`, checked against their SCRAM-SHA-256 verifiers) — or, while
/// the database is unclaimed (no administrator with a password yet,
/// [`nucleus_sql::roles`]), this one user.  `ncp` version 4 and the
/// PostgreSQL front end prove them by SCRAM-SHA-256; older `ncp` clients
/// send them in clear text.
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
            if let Some(v) = token_verifier(kv, &*txn, user)? {
                return Ok(v.is_some_and(|v| v.matches(password)));
            }
            if nucleus_sql::roles::unclaimed(&*txn)? {
                return Ok(user == self.user && password == self.password);
            }
            let now = nucleus_sql::datetime::from_system(kv.wall_clock());
            Ok(nucleus_sql::roles::authenticate(&*txn, user, password, now)?.is_ok())
        };
        check().unwrap_or(false)
    }
    fn accepts_certificate(&self, kv: &dyn Kv, user: &str) -> bool {
        if user.starts_with(nucleus_sql::tokens::PREFIX) {
            return false;
        }
        let check = || -> nucleus_sql::Result<bool> {
            let txn = kv.begin(nucleus_kv::TxnOptions::read_only())?;
            if nucleus_sql::roles::unclaimed(&*txn)? {
                return Ok(user == self.user);
            }
            Ok(nucleus_sql::roles::get(&*txn, user)?.is_some_and(|r| r.login))
        };
        check().unwrap_or(false)
    }
    fn verifier(&self, kv: &dyn Kv, user: &str) -> Option<nucleus_crypto::ScramVerifier> {
        let find = || -> nucleus_sql::Result<Option<nucleus_crypto::ScramVerifier>> {
            let txn = kv.begin(nucleus_kv::TxnOptions::read_only())?;
            if let Some(v) = token_verifier(kv, &*txn, user)? {
                return Ok(v);
            }
            if nucleus_sql::roles::unclaimed(&*txn)? {
                // The configured password's verifier, salted per user.
                return Ok((user == self.user).then(|| {
                    let salt =
                        nucleus_crypto::sha256(format!("nucleus bootstrap {user}").as_bytes());
                    nucleus_crypto::ScramVerifier::new(
                        &self.password,
                        &salt[..16],
                        nucleus_crypto::SCRAM_ITERATIONS,
                    )
                }));
            }
            let Some(role) = nucleus_sql::roles::get(&*txn, user)? else {
                return Ok(None);
            };
            let now = nucleus_sql::datetime::from_system(kv.wall_clock());
            if !role.login || role.valid_until.is_some_and(|t| t < now) {
                return Ok(None);
            }
            Ok(role
                .password
                .as_deref()
                .and_then(nucleus_crypto::ScramVerifier::parse))
        };
        find().ok().flatten()
    }
}

/// For a token's login name (`nct_…`), `Some`: the verifier of its secret
/// if it may log in now (`None` inside: unknown, expired, its role gone);
/// for any other name, `None`.
fn token_verifier(
    kv: &dyn Kv,
    txn: &dyn nucleus_kv::KvTxn,
    user: &str,
) -> nucleus_sql::Result<Option<Option<nucleus_crypto::ScramVerifier>>> {
    if !user.starts_with(nucleus_sql::tokens::PREFIX) {
        return Ok(None);
    }
    let now = nucleus_sql::datetime::from_system(kv.wall_clock());
    Ok(Some(nucleus_sql::tokens::usable(txn, user, now)?.and_then(
        |t| nucleus_crypto::ScramVerifier::parse(&t.verifier),
    )))
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
    /// TLS, for clients that ask for it (`None`: they are refused).
    pub tls: Option<Arc<dyn Tls>>,
    /// Bearer tokens (OIDC), as well as passwords (`None`: none).
    pub bearer: Option<Arc<dyn Bearer>>,
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
            tls: None,
            bearer: None,
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

    /// The same, also taking bearer tokens (OIDC) that `bearer` accepts.
    pub fn with_bearer(mut self, bearer: Arc<dyn Bearer>) -> Self {
        self.bearer = Some(bearer);
        self
    }

    /// The same, offering `tls` to clients that ask for it.
    pub fn with_tls(mut self, tls: Arc<dyn Tls>) -> Self {
        self.tls = Some(tls);
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
