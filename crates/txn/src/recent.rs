//! What recent commits wrote: the keys (and key spans) of every commit
//! after a running transaction's snapshot, with the commit's timestamp —
//! what writes check their conflicts against and serializable reads
//! validate against, without a look at the tree.
//!
//! A small commit's keys go into a map, a large one's stay the sorted run
//! they are (its write set's keys, moved): no key inserted one by one.  A
//! large commit that spilled is its key span (lowest, highest) — coarser
//! than its keys, so a key inside it is the tree's to decide.

use std::collections::BTreeMap;
use std::ops::Bound;

/// From how many keys a commit's keys stay a run of their own.
const RUN: usize = 256;
/// How many keys in the map, spans or runs before pruning.
const PRUNE_KEYS: usize = 4096;
const PRUNE_SPANS: usize = 64;
const PRUNE_RUNS: usize = 32;

#[derive(Debug, Default)]
pub(crate) struct Recent {
    /// Keys of small commits, each with the newest commit that wrote it.
    keys: BTreeMap<Vec<u8>, u64>,
    /// The newest timestamp in `keys` (at least).
    keys_newest: u64,
    /// Large commits' keys, sorted, with the commit's timestamp.
    runs: Vec<(Vec<Vec<u8>>, u64)>,
    /// Spilled commits' spans: lowest key, highest, commit timestamp.
    spans: Vec<(Vec<u8>, Vec<u8>, u64)>,
}

impl Recent {
    /// The commit at `ts` wrote `keys` (sorted, no key twice).
    pub(crate) fn add(&mut self, keys: Vec<Vec<u8>>, ts: u64) {
        if keys.len() >= RUN {
            self.runs.push((keys, ts));
        } else if !keys.is_empty() {
            for k in keys {
                self.keys.insert(k, ts);
            }
            self.keys_newest = self.keys_newest.max(ts);
        }
    }

    /// The spilled commit at `ts` wrote keys from `lo` to `hi`.
    pub(crate) fn add_span(&mut self, lo: Vec<u8>, hi: Vec<u8>, ts: u64) {
        self.spans.push((lo, hi, ts));
    }

    /// Whether [`prune`](Self::prune) is due.
    pub(crate) fn wants_prune(&self) -> bool {
        self.keys.len() > PRUNE_KEYS
            || self.spans.len() > PRUNE_SPANS
            || self.runs.len() > PRUNE_RUNS
    }

    /// Forgets the commits at or before `oldest` (no running transaction
    /// began before them).  Runs that stay past their bound go into the
    /// map, so a long snapshot does not leave many to search.
    pub(crate) fn prune(&mut self, oldest: u64) {
        self.keys.retain(|_, t| *t > oldest);
        self.spans.retain(|(_, _, t)| *t > oldest);
        self.runs.retain(|(_, t)| *t > oldest);
        if self.runs.len() > PRUNE_RUNS / 2 {
            for (keys, ts) in std::mem::take(&mut self.runs) {
                for k in keys {
                    let t = self.keys.entry(k).or_insert(ts);
                    *t = (*t).max(ts);
                }
                self.keys_newest = self.keys_newest.max(ts);
            }
        }
        if self.keys.is_empty() {
            self.keys_newest = 0;
        }
    }

    /// The commits after `start`, for questions about them.
    pub(crate) fn since(&self, start: u64) -> Since<'_> {
        Since {
            keys: (self.keys_newest > start).then_some(&self.keys),
            start,
            runs: self
                .runs
                .iter()
                .filter(|(_, t)| *t > start)
                .map(|(k, _)| k.as_slice())
                .collect(),
            spans: self
                .spans
                .iter()
                .filter(|(_, _, t)| *t > start)
                .map(|(lo, hi, _)| (lo.as_slice(), hi.as_slice()))
                .collect(),
        }
    }
}

/// [`Recent`]'s commits after a snapshot.
pub(crate) struct Since<'a> {
    /// The map, when a commit in it may be after `start`.
    keys: Option<&'a BTreeMap<Vec<u8>, u64>>,
    start: u64,
    runs: Vec<&'a [Vec<u8>]>,
    spans: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> Since<'a> {
    /// Whether a commit after the snapshot wrote `key` (one that spilled
    /// aside: see [`spanned`](Self::spanned)).
    pub(crate) fn wrote(&self, key: &[u8]) -> bool {
        self.keys
            .is_some_and(|m| m.get(key).is_some_and(|&t| t > self.start))
            || self
                .runs
                .iter()
                .any(|r| r.binary_search_by(|k| k.as_slice().cmp(key)).is_ok())
    }

    /// Whether a spilled commit after the snapshot wrote a span holding
    /// `key` (it may or may not have written the key).
    pub(crate) fn spanned(&self, key: &[u8]) -> bool {
        self.spans.iter().any(|&(lo, hi)| lo <= key && key <= hi)
    }

    /// The spans of the spilled commits after the snapshot.
    pub(crate) fn spans(&self) -> &[(&'a [u8], &'a [u8])] {
        &self.spans
    }

    /// A key in `bounds` a commit after the snapshot wrote, if any (spans
    /// aside).
    pub(crate) fn wrote_in(&self, bounds: (Bound<&[u8]>, Bound<&[u8]>)) -> Option<Vec<u8>> {
        if let Some((k, _)) = self
            .keys
            .and_then(|m| m.range::<[u8], _>(bounds).find(|(_, t)| **t > self.start))
        {
            return Some(k.clone());
        }
        for r in &self.runs {
            let from = match bounds.0 {
                Bound::Unbounded => 0,
                Bound::Included(s) => r.partition_point(|k| k.as_slice() < s),
                Bound::Excluded(s) => r.partition_point(|k| k.as_slice() <= s),
            };
            if let Some(k) = r.get(from) {
                let inside = match bounds.1 {
                    Bound::Unbounded => true,
                    Bound::Included(e) => k.as_slice() <= e,
                    Bound::Excluded(e) => k.as_slice() < e,
                };
                if inside {
                    return Some(k.clone());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> Vec<u8> {
        i.to_be_bytes().to_vec()
    }

    #[test]
    fn small_and_large_commits_answer_alike() {
        let mut r = Recent::default();
        r.add(vec![key(1), key(5)], 10); // small: the map
        r.add((100..1000).map(key).collect(), 11); // large: a run
        r.add_span(key(2000), key(3000), 12);
        let s = r.since(9);
        assert!(s.wrote(&key(1)) && s.wrote(&key(5)) && s.wrote(&key(500)));
        assert!(!s.wrote(&key(2)) && !s.wrote(&key(1000)) && !s.wrote(&key(2500)));
        assert!(s.spanned(&key(2500)) && !s.spanned(&key(3001)));
        let s = r.since(10);
        assert!(!s.wrote(&key(1)) && s.wrote(&key(100)));
        let s = r.since(12);
        assert!(!s.wrote(&key(500)) && !s.spanned(&key(2500)));

        let inc = |k: u32| Bound::Included(key(k));
        let exc = |k: u32| Bound::Excluded(key(k));
        let ask = |s: &Since, a: Bound<Vec<u8>>, b: Bound<Vec<u8>>| {
            s.wrote_in((a.as_ref().map(|k| &k[..]), b.as_ref().map(|k| &k[..])))
        };
        let s = r.since(9);
        assert_eq!(ask(&s, inc(6), exc(100)), None);
        assert_eq!(ask(&s, inc(6), inc(100)), Some(key(100)));
        assert_eq!(ask(&s, exc(1), exc(6)), Some(key(5)));
        assert_eq!(ask(&s, exc(999), Bound::Unbounded), None);
        assert_eq!(ask(&s, Bound::Unbounded, exc(1)), None);
        assert_eq!(ask(&r.since(10), Bound::Unbounded, exc(100)), None);
    }

    #[test]
    fn prune_forgets_old_commits_and_folds_lingering_runs() {
        let mut r = Recent::default();
        for ts in 1..=40 {
            r.add(
                (0..RUN as u32).map(|i| key(i * 100 + ts as u32)).collect(),
                ts,
            );
        }
        assert!(r.wants_prune());
        r.prune(20); // runs 21..=40 stay: too many to search, so folded
        assert!(r.runs.is_empty() && r.keys.len() == 20 * RUN);
        let s = r.since(35);
        assert!(s.wrote(&key(36)) && !s.wrote(&key(35)) && !s.wrote(&key(30)));
        r.prune(40);
        assert!(r.keys.is_empty() && !r.since(0).wrote(&key(40)));
    }
}
