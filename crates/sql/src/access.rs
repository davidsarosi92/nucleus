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
}

impl Who {
    /// The bootstrap administrator.
    pub fn system() -> Who {
        Who {
            session_user: SYSTEM_USER.into(),
            user: SYSTEM_USER.into(),
            superuser: true,
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
