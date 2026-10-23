//! The sessions a front end admitted (PLAN.md §M14.25), for
//! `nc_catalog.sessions`: who each is, where it connected from and how,
//! the access rule that admitted it, its ceiling and its active roles —
//! fixed at login, the current role following `SET ROLE`.
//!
//! The list lives in the process, by database (a process may serve
//! several); an administrator sees every session of the database, anyone
//! else their own.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nucleus_kv::Kv;

/// A session, as `nc_catalog.sessions` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// Its number (unique in the process).
    pub id: u64,
    /// The role it acts as (`current_user`).
    pub user: String,
    /// The role it logged in as.
    pub session_user: String,
    /// Where and how it connected.
    pub context: crate::access::Context,
    /// The access rule that admitted it.
    pub rule: Option<String>,
    /// Its ceiling: read only.
    pub read_only: bool,
    /// Its ceiling: only what this role may do as well.
    pub as_role: Option<String>,
    /// The roles whose privileges it had at login.
    pub roles: Vec<String>,
    /// When it logged in (µs since 2000-01-01 UTC).
    pub started: i64,
}

type Table = BTreeMap<u64, Info>;

static NEXT: AtomicU64 = AtomicU64::new(1);
static SESSIONS: Mutex<BTreeMap<usize, Table>> = Mutex::new(BTreeMap::new());

/// A database's identity in the list: its address.
fn database(kv: &Arc<dyn Kv>) -> usize {
    Arc::as_ptr(kv) as *const () as usize
}

/// A session's place in the list, until it is dropped.
#[derive(Debug)]
pub struct Entry {
    database: usize,
    id: u64,
}

/// Lists a session.
pub fn register(kv: &Arc<dyn Kv>, mut info: Info) -> Entry {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    info.id = id;
    let database = database(kv);
    if let Ok(mut all) = SESSIONS.lock() {
        all.entry(database).or_default().insert(id, info);
    }
    Entry { database, id }
}

impl Entry {
    /// The session acts as `user` now (`SET ROLE`).
    pub fn set_user(&self, user: &str) {
        if let Ok(mut all) = SESSIONS.lock()
            && let Some(info) = all
                .get_mut(&self.database)
                .and_then(|t| t.get_mut(&self.id))
        {
            info.user = user.to_string();
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Ok(mut all) = SESSIONS.lock()
            && let Some(t) = all.get_mut(&self.database)
        {
            t.remove(&self.id);
            if t.is_empty() {
                all.remove(&self.database);
            }
        }
    }
}

/// The sessions of the database the current statement runs in that the
/// current role may see.
pub fn visible() -> Vec<Info> {
    let Some(kv) = crate::datetime::current_kv() else {
        return Vec::new();
    };
    let who = crate::access::current();
    let Ok(all) = SESSIONS.lock() else {
        return Vec::new();
    };
    all.get(&database(&kv))
        .map(|t| {
            t.values()
                .filter(|i| who.superuser || i.session_user == who.session_user)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
