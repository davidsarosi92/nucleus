//! Key filters (PLAN.md §M14.20): what a hash join's table lets through,
//! handed down to the scans its probe side reads.
//!
//! When an inner or semi join hashes its right side and that side was
//! narrowed (a filter, a join, an aggregate: not a whole table), every
//! integer key the table holds goes into a filter — its least and greatest
//! value, and the values themselves (a short list, else a Bloom filter) —
//! for the table scan below the probe side that the join's key column comes
//! from, through projections, filters, sorts and the joins that keep their
//! rows' columns.  The scan then drops a row whose key no table row has
//! (that row would leave at the join anyway), and a column store skips the
//! segments whose least and greatest values miss the range.  The filter is
//! made once the right side is in hand, in every executor alike — a row at
//! a time and a batch at a time, on one thread and on workers — so their
//! rows and errors stay the same.

use std::collections::HashMap;
use std::sync::Arc;

use crate::expr::{Ctx, Env, Expr};
use crate::plan::{JoinType, Plan};
use crate::types::{Row, Value};

/// Keys of up to this many values are kept as a sorted list; more go into
/// a Bloom filter.
const LIST: usize = 64;

/// The integer keys a join's table holds, as a filter.
#[derive(Debug)]
pub(crate) struct KeyFilter {
    min: i64,
    max: i64,
    /// The keys, sorted (when few).
    list: Option<Vec<i64>>,
    /// A Bloom filter: a word a key, three bits set in it.
    bloom: Vec<u64>,
}

impl KeyFilter {
    /// The filter of `keys` (none: one nothing passes).
    pub(crate) fn of(mut keys: Vec<i64>) -> Self {
        keys.sort_unstable();
        keys.dedup();
        let (min, max) = match (keys.first(), keys.last()) {
            (Some(&a), Some(&b)) => (a, b),
            _ => (1, 0),
        };
        if keys.len() <= LIST {
            return KeyFilter {
                min,
                max,
                list: Some(keys),
                bloom: Vec::new(),
            };
        }
        // About 16 bits a key: a few false positives in a hundred.
        let words = (keys.len() / 4).next_power_of_two();
        let mut bloom = vec![0u64; words];
        for &k in &keys {
            let (w, bits) = Self::spot(k, words);
            bloom[w] |= bits;
        }
        KeyFilter {
            min,
            max,
            list: None,
            bloom,
        }
    }

    /// A key's word and bits.
    #[inline(always)]
    fn spot(k: i64, words: usize) -> (usize, u64) {
        let h = nucleus_kernels::mix(k as u64);
        let w = (h as usize) & (words - 1);
        let bits =
            (1u64 << ((h >> 40) & 63)) | (1u64 << ((h >> 46) & 63)) | (1u64 << ((h >> 52) & 63));
        (w, bits)
    }

    /// Whether `k` may be a key (never `false` for one).
    #[inline]
    pub(crate) fn contains(&self, k: i64) -> bool {
        if k < self.min || k > self.max {
            return false;
        }
        match &self.list {
            Some(list) => list.binary_search(&k).is_ok(),
            None => {
                let (w, bits) = Self::spot(k, self.bloom.len());
                self.bloom[w] & bits == bits
            }
        }
    }

    /// Whether a row's value may be a key: an integer the filter holds
    /// (`NULL` equals nothing; another kind no integer key).
    pub(crate) fn holds(&self, v: &Value) -> bool {
        matches!(v, Value::Int(k) if self.contains(*k))
    }

    /// The range the keys lie in, for a storage to skip by.
    pub(crate) fn bound(&self, column: usize) -> crate::storage::ColumnBound {
        use std::ops::Bound::Included;
        crate::storage::ColumnBound {
            column,
            lower: Included(Value::Int(self.min)),
            upper: Included(Value::Int(self.max)),
        }
    }
}

/// The filters on one scan: `(join, column, filter)`.
pub(crate) type Filters = Vec<(usize, usize, Arc<KeyFilter>)>;

/// Every scan's filters in an execution, by the scan's address.
pub(crate) type Registry = HashMap<usize, Filters>;

/// The filters registered for `scan`.
/// (`EXPLAIN ANALYZE` notes it on the scan.)
pub(crate) fn of_scan(ctx: &Ctx, scan: &Plan) -> Vec<(usize, Arc<KeyFilter>)> {
    let filters: Vec<_> = ctx
        .key_filters
        .borrow()
        .get(&(scan as *const Plan as usize))
        .map(|f| f.iter().map(|(_, c, k)| (*c, k.clone())).collect())
        .unwrap_or_default();
    if let Some(p) = ctx.profile
        && !filters.is_empty()
    {
        p.keyed(scan);
    }
    filters
}

/// Whether `row` passes `filters`.
pub(crate) fn passes(filters: &[(usize, Arc<KeyFilter>)], row: &Row) -> bool {
    filters
        .iter()
        .all(|(c, f)| row.get(*c).is_some_and(|v| f.holds(v)))
}

/// The scans `col` of `plan`'s rows comes from, unchanged, with nothing
/// between that keeps a row a later drop would not (a limit, a group, the
/// side of a join that pads): each scan and its column.
fn sources(plan: &Plan, col: usize, out: &mut Vec<(*const Plan, usize)>) -> bool {
    match plan {
        Plan::Scan { .. } => {
            out.push((plan as *const Plan, col));
            true
        }
        Plan::Filter { input, .. }
        | Plan::Secure { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Distinct { input } => sources(input, col, out),
        Plan::Project { input, exprs } => match exprs.get(col) {
            Some(Expr::Column(c)) => sources(input, *c, out),
            _ => false,
        },
        Plan::Join {
            left, right, kind, ..
        } => {
            let lw = left.width();
            match kind {
                JoinType::Inner if col >= lw => sources(right, col - lw, out),
                JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti if col < lw => {
                    sources(left, col, out)
                }
                _ => false,
            }
        }
        Plan::Append { inputs, .. } => inputs.iter().all(|p| sources(p, col, out)),
        _ => false,
    }
}

/// Whether `plan` gives fewer rows than a table holds: anything but a
/// whole table read (perhaps projected) — a filter that keeps what the
/// whole table would let through anyway is not worth one.
fn narrowed(plan: &Plan) -> bool {
    match plan {
        Plan::Scan { access, filter, .. } => {
            filter.is_some() || !matches!(access, crate::plan::Access::Full)
        }
        Plan::Project { input, .. } => narrowed(input),
        _ => true,
    }
}

/// After a hash join's right side is in hand: the filters of its keys
/// for the scans below `left` they come from — for an inner or semi join
/// whose right side was narrowed.  `keys(j)`: the integer values of the
/// right's key `j` (`None`: not all integers).
pub(crate) fn register(
    ctx: &Ctx,
    left: &Plan,
    right: &Plan,
    kind: JoinType,
    left_keys: &[Expr],
    keys: &dyn Fn(usize) -> Option<Vec<i64>>,
) {
    if !matches!(kind, JoinType::Inner | JoinType::Semi) || !narrowed(right) {
        return;
    }
    let join = left as *const Plan as usize;
    let mut registry = ctx.key_filters.borrow_mut();
    // Filters of an earlier run of this join (a correlated subquery's) go.
    for filters in registry.values_mut() {
        filters.retain(|(j, _, _)| *j != join);
    }
    for (j, key) in left_keys.iter().enumerate() {
        let Expr::Column(c) = key else { continue };
        let mut scans = Vec::new();
        if !sources(left, *c, &mut scans) || scans.is_empty() {
            continue;
        }
        let Some(values) = keys(j) else { continue };
        let filter = Arc::new(KeyFilter::of(values));
        for (scan, col) in scans {
            registry
                .entry(scan as usize)
                .or_default()
                .push((join, col, filter.clone()));
        }
    }
}

/// The integer values of each of `keys` over `rows` — a row with a
/// `NULL` key left out, as a join's table leaves it — (`None` for a key
/// with another kind of value, or for all when one fails: the join meets
/// the error itself).
pub(crate) fn key_values<'r>(
    rows: impl Iterator<Item = crate::error::Result<std::borrow::Cow<'r, Row>>>,
    keys: &[Expr],
    ctx: &Ctx,
    outer: Option<&Env>,
) -> Vec<Option<Vec<i64>>> {
    let mut out: Vec<Option<Vec<i64>>> = vec![Some(Vec::new()); keys.len()];
    for r in rows {
        let Ok(r) = r else {
            return vec![None; keys.len()];
        };
        let e = crate::exec::env(&r, ctx, outer);
        let Ok(vals) = keys
            .iter()
            .map(|k| k.eval(&e))
            .collect::<crate::error::Result<Vec<_>>>()
        else {
            return vec![None; keys.len()];
        };
        if vals.iter().any(Value::is_null) {
            continue;
        }
        for (v, o) in vals.into_iter().zip(out.iter_mut()) {
            match v {
                Value::Int(x) => {
                    if let Some(o) = o {
                        o.push(x);
                    }
                }
                _ => *o = None,
            }
        }
    }
    out
}
