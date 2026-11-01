//! A transaction's write set: the newest write of each key, in key order.
//!
//! Most large write sets arrive in key order (a statement writes the rows
//! it found by a scan of the key order), so the set is a sorted vector
//! while they do: a write past the last key is a push, a read a binary
//! search, the commit a walk — no tree to build key by key.  A write a
//! little out of order is moved into place; the first one far out of order
//! turns the set into a B-tree map (built from the sorted run in one pass)
//! for good.

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::ops::Bound;

/// A write: the key, and its value (`None`: a delete).
pub(crate) type Write = (Vec<u8>, Option<Vec<u8>>);

/// How far from the end a write may land and still go into the vector
/// (the entries after it moved by one).
const NEAR_END: usize = 64;

#[derive(Debug)]
pub(crate) enum WriteSet {
    /// Sorted, no key twice.
    Sorted(Vec<Write>),
    Map(BTreeMap<Vec<u8>, Option<Vec<u8>>>),
}

impl Default for WriteSet {
    fn default() -> Self {
        WriteSet::Sorted(Vec::new())
    }
}

impl WriteSet {
    pub(crate) fn len(&self) -> usize {
        match self {
            WriteSet::Sorted(v) => v.len(),
            WriteSet::Map(m) => m.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The write of `key`, if any.
    pub(crate) fn get(&self, key: &[u8]) -> Option<&Option<Vec<u8>>> {
        match self {
            // Past the last key — every write's question while they come
            // in order — is one comparison.
            WriteSet::Sorted(v) => match v.last() {
                Some((last, _)) if last.as_slice() < key => None,
                _ => v
                    .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                    .ok()
                    .map(|i| &v[i].1),
            },
            WriteSet::Map(m) => m.get(key),
        }
    }

    pub(crate) fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Sets `key`'s write; the one it replaces, if any.
    pub(crate) fn insert(&mut self, key: &[u8], value: Option<Vec<u8>>) -> Option<Option<Vec<u8>>> {
        let v = match self {
            WriteSet::Map(m) => return m.insert(key.to_vec(), value),
            WriteSet::Sorted(v) => v,
        };
        // In order: past the last key (or it).
        match v.last_mut() {
            None => {
                v.push((key.to_vec(), value));
                return None;
            }
            Some((last, _)) if last.as_slice() < key => {
                v.push((key.to_vec(), value));
                return None;
            }
            Some((last, old)) if last.as_slice() == key => {
                return Some(std::mem::replace(old, value));
            }
            Some(_) => {}
        }
        match v.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => Some(std::mem::replace(&mut v[i].1, value)),
            Err(i) if v.len() - i <= NEAR_END => {
                v.insert(i, (key.to_vec(), value));
                None
            }
            Err(_) => {
                let mut m: BTreeMap<_, _> = std::mem::take(v).into_iter().collect();
                m.insert(key.to_vec(), value);
                *self = WriteSet::Map(m);
                None
            }
        }
    }

    pub(crate) fn first_key(&self) -> Option<&Vec<u8>> {
        match self {
            WriteSet::Sorted(v) => v.first().map(|(k, _)| k),
            WriteSet::Map(m) => m.keys().next(),
        }
    }

    pub(crate) fn last_key(&self) -> Option<&Vec<u8>> {
        match self {
            WriteSet::Sorted(v) => v.last().map(|(k, _)| k),
            WriteSet::Map(m) => m.keys().next_back(),
        }
    }

    /// The writes in key order.
    pub(crate) fn iter(&self) -> Range<'_> {
        self.range(Bound::Unbounded, Bound::Unbounded)
    }

    /// The keys in order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.iter().map(|(k, _)| k)
    }

    /// The writes in `(start, end)`, in key order (none when the range is
    /// empty or reversed).
    pub(crate) fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Range<'_> {
        let reversed = match (start, end) {
            (Bound::Included(s), Bound::Included(e)) => s > e,
            (Bound::Included(s) | Bound::Excluded(s), Bound::Included(e) | Bound::Excluded(e)) => {
                s >= e
            }
            _ => false,
        };
        match self {
            WriteSet::Sorted(v) => {
                let at = |b: Bound<&[u8]>, past_equal: bool| match b {
                    Bound::Unbounded => None,
                    Bound::Included(k) => Some(v.partition_point(|(x, _)| {
                        x.as_slice() < k || (past_equal && x.as_slice() == k)
                    })),
                    Bound::Excluded(k) => Some(v.partition_point(|(x, _)| {
                        x.as_slice() < k || (!past_equal && x.as_slice() == k)
                    })),
                };
                let lo = at(start, false).unwrap_or(0);
                let hi = at(end, true).unwrap_or(v.len()).max(lo);
                Range::Sorted(if reversed {
                    [].iter()
                } else {
                    v[lo..hi].iter()
                })
            }
            WriteSet::Map(m) => Range::Map(if reversed {
                None
            } else {
                Some(m.range::<[u8], _>((start, end)))
            }),
        }
    }
}

impl IntoIterator for WriteSet {
    type Item = Write;
    type IntoIter = IntoIter;

    fn into_iter(self) -> IntoIter {
        match self {
            WriteSet::Sorted(v) => IntoIter::Sorted(v.into_iter()),
            WriteSet::Map(m) => IntoIter::Map(m.into_iter()),
        }
    }
}

/// [`WriteSet::range`]'s iterator.
pub(crate) enum Range<'a> {
    Sorted(std::slice::Iter<'a, Write>),
    Map(Option<btree_map::Range<'a, Vec<u8>, Option<Vec<u8>>>>),
}

impl<'a> Iterator for Range<'a> {
    type Item = (&'a Vec<u8>, &'a Option<Vec<u8>>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Range::Sorted(it) => it.next().map(|(k, v)| (k, v)),
            Range::Map(it) => it.as_mut()?.next(),
        }
    }
}

/// The writes, owned, in key order.
pub(crate) enum IntoIter {
    Sorted(std::vec::IntoIter<Write>),
    Map(btree_map::IntoIter<Vec<u8>, Option<Vec<u8>>>),
}

impl Iterator for IntoIter {
    type Item = Write;

    fn next(&mut self) -> Option<Write> {
        match self {
            IntoIter::Sorted(it) => it.next(),
            IntoIter::Map(it) => it.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes in an order that starts sorted, goes a little out of order,
    /// then far: every read agrees with a B-tree map's.
    #[test]
    fn agrees_with_a_map_whatever_the_order() {
        let mut set = WriteSet::default();
        let mut model = BTreeMap::new();
        let mut keys: Vec<u32> = (0..2000).map(|i| i * 3).collect();
        keys.extend((0..20).map(|i| 5990 - i * 4)); // near the end
        keys.extend([3, 3, 5999, 0]); // repeats, one past the end
        keys.extend((0..500).map(|i| (i * 7919) % 7000)); // anywhere
        let mut was_sorted_at = None;
        for (n, k) in keys.iter().enumerate() {
            let key = k.to_be_bytes().to_vec();
            let value = (k % 5 != 0).then(|| vec![n as u8]);
            assert_eq!(set.insert(&key, value.clone()), model.insert(key, value));
            if matches!(set, WriteSet::Sorted(_)) {
                was_sorted_at = Some(n);
            }
            assert_eq!(set.len(), model.len());
        }
        assert!(
            was_sorted_at.is_some_and(|n| n >= 2023),
            "sorted through the near writes"
        );
        assert!(matches!(set, WriteSet::Map(_)), "a map after the far ones");
        check(&set, &model);
    }

    #[test]
    fn a_sorted_set_reads_as_a_map() {
        let mut set = WriteSet::default();
        let mut model = BTreeMap::new();
        for k in (0..300u32).map(|i| i * 2) {
            let key = k.to_be_bytes().to_vec();
            set.insert(&key, Some(vec![1]));
            model.insert(key, Some(vec![1]));
        }
        assert!(matches!(set, WriteSet::Sorted(_)));
        check(&set, &model);
    }

    fn check(set: &WriteSet, model: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
        let all: Vec<_> = set.iter().collect();
        assert_eq!(all, model.iter().collect::<Vec<_>>());
        assert_eq!(set.first_key(), model.keys().next());
        assert_eq!(set.last_key(), model.keys().next_back());
        for k in [0u32, 1, 2, 3, 300, 599, 600, 4000, 7001] {
            let key = k.to_be_bytes();
            assert_eq!(set.get(&key), model.get(&key[..]));
        }
        let bounds = |k: u32| {
            let key = k.to_be_bytes().to_vec();
            [
                Bound::Included(key.clone()),
                Bound::Excluded(key),
                Bound::Unbounded,
            ]
        };
        for (a, b) in [(0, 10), (3, 3), (4, 3), (100, 7000), (599, 600)] {
            for s in bounds(a) {
                for e in bounds(b) {
                    let got: Vec<_> = set
                        .range(s.as_ref().map(|k| &k[..]), e.as_ref().map(|k| &k[..]))
                        .collect();
                    let empty = matches!((&s, &e), (Bound::Included(x), Bound::Included(y)) if x > y)
                        || matches!((&s, &e), (Bound::Included(x) | Bound::Excluded(x), Bound::Excluded(y)) if x >= y)
                        || matches!((&s, &e), (Bound::Excluded(x), Bound::Included(y)) if x >= y);
                    let want: Vec<_> = if empty {
                        Vec::new()
                    } else {
                        model
                            .range::<[u8], _>((
                                s.as_ref().map(|k| &k[..]),
                                e.as_ref().map(|k| &k[..]),
                            ))
                            .collect()
                    };
                    assert_eq!(got, want, "{s:?}..{e:?}");
                }
            }
        }
        let owned: Vec<_> = WriteSet::clone_for_test(set).into_iter().collect();
        assert_eq!(owned, model.clone().into_iter().collect::<Vec<_>>());
    }

    impl WriteSet {
        fn clone_for_test(&self) -> WriteSet {
            match self {
                WriteSet::Sorted(v) => WriteSet::Sorted(v.clone()),
                WriteSet::Map(m) => WriteSet::Map(m.clone()),
            }
        }
    }
}
