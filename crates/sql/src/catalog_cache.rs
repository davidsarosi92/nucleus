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
    /// A query's plan, by its text ([`statement_text`]).
    Plan,
    /// A statement's parameter types and result columns, by its text.
    Describe,
}

impl Part {
    /// Whether its entries are by a statement's text: kept once the text
    /// came again ([`SEEN`]), and no more than [`KEPT`] of them.
    fn by_text(self) -> bool {
        matches!(self, Part::Plan | Part::Describe)
    }
}

/// How many entries by text a session keeps; past it they all go.
const KEPT: usize = 512;

/// How many texts a session remembers having seen once; past it it
/// forgets them all.
const SEEN: usize = 4096;

/// A session's cache.
#[derive(Default)]
pub struct CatalogCache {
    /// The snapshot its entries were read at.
    at: Option<Timestamp>,
    entries: HashMap<(Part, String), Box<dyn Any + Send>>,
    /// How many of `entries` are by text.
    by_text: usize,
    /// Hashes of the texts seen once (TinyLFU's doorkeeper: a text with
    /// literals in it, never again the same, is not kept).
    seen: std::collections::HashSet<u64>,
}

impl CatalogCache {
    /// Whether an entry for `part` of `name` may be kept.
    fn admits(&mut self, part: Part, name: &str) -> bool {
        if !part.by_text() {
            return true;
        }
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            name.hash(&mut h);
            h.finish()
        };
        if self.seen.contains(&hash) {
            return true;
        }
        if self.seen.len() >= SEEN {
            self.seen.clear();
        }
        self.seen.insert(hash);
        false
    }

    fn keep(&mut self, part: Part, name: &str, v: Box<dyn Any + Send>) {
        if part.by_text() && self.by_text >= KEPT {
            self.entries.retain(|(p, _), _| !p.by_text());
            self.by_text = 0;
        }
        if self.entries.insert((part, name.to_string()), v).is_none() && part.by_text() {
            self.by_text += 1;
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<CatalogCache>> = const { RefCell::new(None) };
    /// The text of the statement a session runs, for its plan's entry.
    static TEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `f` (the statement `stmt`'s execution) with its text known to
/// [`statement_text`] — a query's: its plan is kept by it.
pub(crate) fn with_text<T>(
    stmt: &crate::ast::Statement,
    text: Option<&str>,
    f: impl FnOnce() -> T,
) -> T {
    let text = text.filter(|_| matches!(stmt, crate::ast::Statement::Query(_)));
    let before = TEXT.with(|t| t.replace(text.map(str::to_string)));
    let out = f();
    TEXT.with(|t| *t.borrow_mut() = before);
    out
}

/// The text [`with_text`] gave, once (a statement run inside the first —
/// a function's — finds none).
pub(crate) fn statement_text() -> Option<String> {
    TEXT.with(|t| t.borrow_mut().take())
}

/// Whether an entry for `part` of `name` would be kept now (a text seen
/// before), noting it seen: to do what only a kept entry needs only then.
pub(crate) fn admits(part: Part, name: &str) -> bool {
    CURRENT.with(|c| {
        c.borrow_mut()
            .as_mut()
            .is_some_and(|cache| cache.admits(part, name))
    })
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
    memo_fitting(txn, part, name, |_| true, read)
}

/// [`memo`], an entry kept only if it `fits` (else read again and kept
/// in its place): an entry by a statement's text holds the statement, to
/// be the same.
pub(crate) fn memo_fitting<T: Clone + Send + 'static>(
    txn: &dyn KvTxn,
    part: Part,
    name: &str,
    fits: impl Fn(&T) -> bool,
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
                    .and_then(|v| v.downcast_ref::<T>())
                    .filter(|v| fits(v))
                    .cloned(),
            );
        }
        let now = txn.read_ts();
        if txn.watched_as_at(now) {
            cache.entries.clear();
            cache.by_text = 0;
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
                if let Some(cache) = c.borrow_mut().as_mut()
                    && cache.admits(part, name)
                {
                    cache.keep(part, name, Box::new(v.clone()));
                }
            });
            Ok(v)
        }
        None => read(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_kept_when_seen_again_and_no_more_than_kept() {
        let mut c = CatalogCache::default();
        assert!(c.admits(Part::Table, "t"), "not by text: always");
        assert!(!c.admits(Part::Plan, "q"));
        assert!(c.admits(Part::Plan, "q"));
        for i in 0..KEPT {
            c.keep(Part::Plan, &format!("q{i}"), Box::new(i));
        }
        c.keep(Part::Table, "t", Box::new(0usize));
        assert_eq!(c.by_text, KEPT);
        // One more: the entries by text go, the others stay.
        c.keep(Part::Describe, "d", Box::new(0usize));
        assert_eq!(c.by_text, 1);
        assert_eq!(c.entries.len(), 2);
        // The texts seen once are forgotten past SEEN.
        for i in 0..SEEN {
            c.admits(Part::Plan, &format!("s{i}"));
        }
        assert!(!c.admits(Part::Plan, "q"));
    }
}
