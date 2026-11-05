//! What a session keeps by a statement's text (§M14.20 W8): its parse, its
//! plan, its description, its normalized form — kept the second time the
//! text comes (TinyLFU's *doorkeeper*: a text with its literals inline,
//! never the same twice, costs nothing to keep and pushes nothing out),
//! at most [`KEPT`] texts, all dropped past that.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// How many entries are kept; past it they all go.
pub(crate) const KEPT: usize = 512;

/// How many texts seen once are remembered; past it they are all
/// forgotten.
pub(crate) const SEEN: usize = 4096;

/// Entries by text (or by a key holding one).
pub(crate) struct ByText<K, V> {
    kept: HashMap<K, V>,
    /// Hashes of the keys seen once.
    seen: HashSet<u64>,
}

impl<K, V> Default for ByText<K, V> {
    fn default() -> Self {
        Self {
            kept: HashMap::new(),
            seen: HashSet::new(),
        }
    }
}

impl<K: Hash + Eq, V> ByText<K, V> {
    /// The entry kept for `key`.
    pub(crate) fn get<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.kept.get(key)
    }

    /// Whether an entry for `key` may be kept: it was seen before (else it
    /// is noted seen).
    pub(crate) fn admits<Q: Hash + ?Sized>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        let hash = h.finish();
        if self.seen.contains(&hash) {
            return true;
        }
        if self.seen.len() >= SEEN {
            self.seen.clear();
        }
        self.seen.insert(hash);
        false
    }

    /// Keeps `value` for `key` (all others dropped first, at [`KEPT`]).
    pub(crate) fn keep(&mut self, key: K, value: V) {
        if self.kept.len() >= KEPT && !self.kept.contains_key(&key) {
            self.kept.clear();
        }
        self.kept.insert(key, value);
    }

    /// Drops every entry (what was seen stays seen).
    pub(crate) fn clear(&mut self) {
        self.kept.clear();
    }

    /// How many entries are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.kept.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_when_seen_again_and_no_more_than_kept() {
        let mut c: ByText<String, usize> = ByText::default();
        assert!(!c.admits("q"));
        assert!(c.admits("q"));
        for i in 0..KEPT {
            c.keep(format!("q{i}"), i);
        }
        assert_eq!(c.len(), KEPT);
        assert_eq!(c.get("q7"), Some(&7));
        // Kept again: nothing goes.
        c.keep("q7".into(), 70);
        assert_eq!((c.len(), c.get("q7")), (KEPT, Some(&70)));
        // One more: the others go.
        c.keep("d".into(), 0);
        assert_eq!(c.len(), 1);
        // Texts seen once are forgotten past SEEN.
        for i in 0..SEEN {
            c.admits(format!("s{i}").as_str());
        }
        assert!(!c.admits("q"));
    }
}
