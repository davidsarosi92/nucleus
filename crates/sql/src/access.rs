//! Who a statement runs as (PLAN.md §M14.25): the session's user, the
//! role it has set (`SET ROLE`), and whether that is an administrator.
//!
//! As [`crate::datetime::with_now`] does for the statement's time, the
//! session puts its identity in place for the statement it runs
//! ([`with_who`]); the binder, the executor and DDL read it
//! ([`current`]).  Outside a session — an embedded database, the
//! internals — the identity is [`Who::system`]: the bootstrap
//! administrator.

use std::cell::RefCell;
use std::sync::Arc;

use crate::error::{Error, Result};

/// The name of the bootstrap administrator, and of a session that has not
/// logged in.
pub const SYSTEM_USER: &str = "nucleus";

/// An identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who {
    /// The role the session logged in as (`session_user`).
    pub session_user: String,
    /// The role it acts as (`current_user`): the session user, or the one
    /// `SET ROLE` chose.
    pub user: String,
    /// Whether the role it acts as is an administrator.
    pub superuser: bool,
    /// The session's settings (`SET name = value`), for
    /// `current_setting()` — and so for row-level security policies.
    pub settings: std::collections::BTreeMap<String, String>,
    /// The token the session logged in with ([`crate::tokens`]), by name.
    pub token: Option<String>,
    /// What the session may do at most, whatever its roles may.
    pub ceiling: Ceiling,
    /// Roles the login granted for this session only (an identity
    /// provider's claims, [`Identity::roles`]): the session user has their
    /// privileges as if a member, while it acts as itself.
    pub session_roles: std::collections::BTreeSet<String>,
    /// The login's claims (an identity token's), read only:
    /// `current_setting('nucleus.claims.<name>')`.
    pub claims: std::collections::BTreeMap<String, String>,
    /// Where and how the session connected (none for a local one).
    pub context: Context,
    /// The access rule that admitted it ([`crate::rules`]).
    pub rule: Option<String>,
    /// Memberships `(role, member)` whose `WHEN` condition the connection
    /// does not meet: not held in this session.
    pub suspended: std::collections::BTreeSet<(String, String)>,
}

/// Where and how a session connected: what access rules decide by, and
/// what the session can read (`nucleus.client_addr`, `nucleus.front_end`,
/// `nucleus.auth_method`, `nucleus.tls`, `nucleus.access_rule`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// The client's address (behind a trusted proxy, the one it names).
    pub client_addr: Option<std::net::IpAddr>,
    /// The front end (`ncp`, `pg`; empty for a local session).
    pub front_end: String,
    /// How it authenticated: `password`, `token`, `certificate`, `oidc`,
    /// `ldap`, `trust`.
    pub auth_method: String,
    /// Over TLS.
    pub tls: bool,
}

/// The settings a session's connection context gives, which no `SET` can
/// change.
pub const CONTEXT_SETTINGS: &[&str] = &[
    "nucleus.client_addr",
    "nucleus.front_end",
    "nucleus.auth_method",
    "nucleus.tls",
    "nucleus.access_rule",
];

impl Who {
    /// A context setting's value ([`CONTEXT_SETTINGS`]).
    pub fn context_setting(&self, name: &str) -> Option<String> {
        let c = &self.context;
        match name {
            "nucleus.client_addr" => c.client_addr.map(|a| crate::rules::host(a).host_text()),
            "nucleus.front_end" => Some(c.front_end.clone()),
            "nucleus.auth_method" => Some(c.auth_method.clone()),
            "nucleus.tls" => Some(if c.tls { "on" } else { "off" }.into()),
            "nucleus.access_rule" => self.rule.clone(),
            _ => None,
        }
    }
}

/// Who an identity provider says a client is (a bearer token's subject).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The role it logs in as.
    pub user: String,
    /// Roles granted for the session (they must exist and not be
    /// administrators; others are left out).
    pub roles: Vec<String>,
    /// Claims, as text (an array's items joined by commas, an object as
    /// JSON).
    pub claims: std::collections::BTreeMap<String, String>,
}

/// The prefix under which a session's claims are settings.
pub const CLAIMS: &str = "nucleus.claims.";

/// A ceiling on a session's privileges (a token's, §M14.25's access
/// rules'): it narrows what the roles may do, never widens it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ceiling {
    /// No statement may write (`25006`).
    pub read_only: bool,
    /// The only tables it may touch (`None`: any); none may be created.
    pub tables: Option<std::collections::BTreeSet<String>>,
    /// Only what this role may do as well (an access rule's `AS ROLE`).
    pub as_role: Option<String>,
    /// The longest a statement may run (µs).
    pub timeout: Option<i64>,
    /// The most rows a statement may read.
    pub max_scan: Option<u64>,
    /// The most rows a statement may return.
    pub max_rows: Option<u64>,
    /// The most statements a minute.
    pub rate: Option<u32>,
}

impl Ceiling {
    /// Whether it lets `table` be touched.
    pub fn admits_table(&self, table: &str) -> bool {
        self.tables.as_ref().is_none_or(|t| t.contains(table))
    }
}

impl Who {
    /// The bootstrap administrator.
    pub fn system() -> Who {
        Who {
            session_user: SYSTEM_USER.into(),
            user: SYSTEM_USER.into(),
            superuser: true,
            settings: Default::default(),
            token: None,
            ceiling: Ceiling::default(),
            session_roles: Default::default(),
            claims: Default::default(),
            context: Context::default(),
            rule: None,
            suspended: Default::default(),
        }
    }
}

thread_local! {
    static WHO: RefCell<Option<Arc<Who>>> = const { RefCell::new(None) };
}

/// Runs `f` as `who`.
pub fn with_who<T>(who: &Arc<Who>, f: impl FnOnce() -> T) -> T {
    let before = WHO.with(|w| w.replace(Some(who.clone())));
    let out = f();
    WHO.with(|w| *w.borrow_mut() = before);
    out
}

/// Who the current statement runs as.
pub fn current() -> Arc<Who> {
    WHO.with(|w| w.borrow().clone())
        .unwrap_or_else(|| Arc::new(Who::system()))
}

/// `42501`: `what` needs an administrator.
pub fn denied(what: &str) -> Error {
    Error::insufficient_privilege(format!("permission denied: {what}"))
}

/// Unless the current role is an administrator, `42501`.
pub fn require_superuser(what: &str) -> Result<()> {
    if current().superuser {
        Ok(())
    } else {
        Err(denied(&format!("{what} needs a superuser")))
    }
}
