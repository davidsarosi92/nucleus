//! A session's catalog cache (§M14.20 W8): what a statement derives from
//! the catalog — a table's definition, a database setting, an object's
//! ACL and owner, a role's effective roles — kept for the session's next
//! statements while no commit writes the catalog.
//!
//! The KV store watches the catalog's key ranges ([`Kv::watch`]); a
//! transaction answers whether it reads them as the snapshot the cache was
//! filled at does ([`KvTxn::watched_as_at`]).  When it does not — a commit
//! changed the catalog since, or this transaction did — the cache is
//! filled again at the transaction's snapshot if that is current, and else
//! (an older snapshot, a transaction with its own catalog writes, a
//! serializable one that writes) passed by: the catalog is read as before.
//!
//! The session lends its cache to the thread running its statement
//! ([`with`]); work on other threads reads the catalog itself.  Every
//! transaction a statement uses is the session's database's.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;

use nucleus_kv::{Kv, KvTxn, Timestamp};

use crate::error::Result;

/// The catalog's key ranges, `[start, end)`: every catalog key but the
/// counters (sequences `00 02`, hidden row ids `00 03`) and the audit log
/// (`00 12`, `00 13`), which ordinary writes change.
const WATCHED: [(&[u8], &[u8]); 2] = [
    (&[0x00, 0x00], &[0x00, 0x02]),
    (&[0x00, 0x04], &[0x00, 0x12]),
];

/// Asks `kv` to watch the catalog.
pub(crate) fn watch(kv: &dyn Kv) {
    for (start, end) in WATCHED {
        kv.watch(start, end);
    }
}

/// What a cached entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Part {
    /// A table's definition (`Option<Table>`).
    Table,
    /// A database setting's value.
    Setting,
    /// An object's ACL (of a kind of object).
    Acl(crate::privileges::Kind),
    /// An object's owner.
    Owner(crate::privileges::Kind),
    /// The roles whose privileges an identity has.
    Roles,
}

/// A session's cache.
#[derive(Default)]
pub struct CatalogCache {
    /// The snapshot its entries were read at.
    at: Option<Timestamp>,
    entries: HashMap<(Part, String), Box<dyn Any + Send>>,
}

thread_local! {
    static CURRENT: RefCell<Option<CatalogCache>> = const { RefCell::new(None) };
}

/// Runs `f` with `cache` lent to this thread; the cache back.
pub(crate) fn with<T>(cache: CatalogCache, f: impl FnOnce() -> T) -> (T, CatalogCache) {
    let before = CURRENT.with(|c| c.replace(Some(cache)));
    let out = f();
    let cache = CURRENT.with(|c| std::mem::replace(&mut *c.borrow_mut(), before));
    (out, cache.unwrap_or_default())
}

/// `part` of `name` as `txn` reads it: from the cache when it holds for
/// `txn`, else by `read` (kept, when `txn` reads the current catalog).
pub(crate) fn memo<T: Clone + Send + 'static>(
    txn: &dyn KvTxn,
    part: Part,
    name: &str,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let usable = CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let cache = c.as_mut()?;
        if cache.at.is_some_and(|at| txn.watched_as_at(at)) {
            return Some(
                cache
                    .entries
                    .get(&(part, name.to_string()))
                    .and_then(|v| v.downcast_ref::<T>().cloned()),
            );
        }
        let now = txn.read_ts();
        if txn.watched_as_at(now) {
            cache.entries.clear();
            cache.at = Some(now);
            return Some(None);
        }
        None
    });
    match usable {
        Some(Some(v)) => Ok(v),
        Some(None) => {
            // Read without the cache borrowed (reads may fill other
            // entries).
            let v = read()?;
            CURRENT.with(|c| {
                if let Some(cache) = c.borrow_mut().as_mut() {
                    cache
                        .entries
                        .insert((part, name.to_string()), Box::new(v.clone()));
                }
            });
            Ok(v)
        }
        None => read(),
    }
}
