//! Ordered-set aggregates (PLAN.md §M14.27 S5): `percentile_cont`,
//! `percentile_disc` and `mode` — PostgreSQL's, called `name(…) WITHIN
//! GROUP (ORDER BY x)` — and `median(x)` (DuckDB's).  Exact, and found by
//! **selection** rather than by sorting the group.
//!
//! A group's values are kept in memory as they come — integers, booleans,
//! dates, times and doubles as order-preserving 8-byte keys, any other
//! type as values.  Its result is then the values at a few ranks: each
//! found by `select_nth_unstable` (introselect: quickselect with a
//! worst-case fallback, O(n)), the next rank searched for only above the
//! last; `mode` sorts its group and counts runs.  States combine — a
//! partial state is the group's values, keys as bytes — so the values are
//! collected on every worker.  An accumulator holding more than
//! `work_mem` moves its values to scratch space: keys as (group, key)
//! records, read back by partitions of half of `work_mem` — groups packed
//! whole, a larger group split into ranges of its keys by a 2¹⁶-slice
//! count of them ([`Radix`]), so a rank is found in the one partition that
//! holds it; other values into a [`Sorter`] of (group, value), read once
//! in group order.

//! Results are PostgreSQL's: `percentile_cont(f)` interpolates between
//! the values at `floor` and `ceil` of `f·(n − 1)` (`lo + (hi − lo)·t`, in
//! the call's order, so `DESC` gives PostgreSQL's bits); `percentile_disc`
//! takes the value at `ceil(f·n)`; `mode` the most frequent value, ties to
//! the first in the call's order.  A fraction outside `[0, 1]` is an error
//! when the group has rows; an array of fractions gives an array.

use std::cmp::Ordering;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, GroupAccumulator, Registry, Returns, Signature,
};
use crate::sorter::{self, Budget, Part, Sorter};
use crate::spill::{Run, Sealed};
use crate::types::{DataType, Row, Value};
use crate::vector::{TimeKind, Vector};

/// Which aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `percentile_cont`.
    Cont,
    /// `percentile_disc`.
    Disc,
    /// `mode`.
    Mode,
    /// `median`: `Cont` over numbers and intervals, else `Disc`.
    Median,
}

/// Registers the ordered-set aggregates.
pub fn register(r: &mut Registry) -> Result<()> {
    for (kind, name) in [
        (Kind::Cont, "percentile_cont"),
        (Kind::Disc, "percentile_disc"),
        (Kind::Mode, "mode"),
        (Kind::Median, "median"),
    ] {
        r.add_aggregate(&[name], Arc::new(OrderedSet(kind, name)))?;
    }
    Ok(())
}

struct OrderedSet(Kind, &'static str);

/// Where a call's arguments are: the value first; then, for the
/// `WITHIN GROUP` forms, the fraction (`percentile_*`) and the direction.
#[derive(Clone, Copy, Debug)]
struct Layout {
    kind: Kind,
    fraction: Option<usize>,
    desc: Option<usize>,
}

impl OrderedSet {
    fn layout(&self) -> Layout {
        let (fraction, desc) = match self.0 {
            Kind::Cont | Kind::Disc => (Some(1), Some(2)),
            Kind::Mode => (None, Some(1)),
            Kind::Median => (None, None),
        };
        Layout {
            kind: self.0,
            fraction,
            desc,
        }
    }

    fn no_such(&self, args: &[ArgInfo]) -> Error {
        let names: Vec<&str> = args
            .iter()
            .map(|a| {
                a.ty.map_or("unknown", |t| crate::builtin::type_of(t).name())
            })
            .collect();
        let shown = match self.0 {
            Kind::Median => names.join(", "),
            _ => names[1..names.len() - 1].join(", "),
        };
        let e = Error::undefined_function(match self.0 {
            Kind::Median => format!("function median({shown}) does not exist"),
            _ => format!(
                "function {}({shown}) WITHIN GROUP (ORDER BY {}) does not exist",
                self.1, names[0]
            ),
        });
        e.with_hint(match self.0 {
            Kind::Cont => "percentile_cont interpolates numbers and intervals at a fraction (or an array of them); percentile_disc takes any ordered type.",
            Kind::Disc => "percentile_disc takes a fraction (or an array of them) and orders any built-in type.",
            Kind::Mode => "mode takes no direct argument and orders any built-in type.",
            Kind::Median => "median takes a number, an interval or any ordered built-in type.",
        })
    }
}

impl Aggregate for OrderedSet {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            Kind::Cont | Kind::Disc => 3..=3,
            Kind::Mode => 2..=2,
            Kind::Median => 1..=1,
        }
    }

    fn order_free(&self) -> bool {
        true
    }

    fn ordered_set(&self) -> bool {
        self.0 != Kind::Median
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::*;
        let x = args[0].ty;
        let ordered = |t: DataType| !matches!(t, Ext(_)) && t.element().is_none_or(|e| e != t);
        // The value: interpolated (numbers as doubles, intervals) or kept.
        let interpolated = match (self.0, x) {
            (Kind::Cont | Kind::Median, None) => Some(Float),
            (Kind::Cont | Kind::Median, Some(t)) if t.is_number() => Some(Float),
            (Kind::Cont | Kind::Median, Some(Interval)) => Some(Interval),
            (Kind::Cont, _) => return Err(self.no_such(args)),
            _ => None,
        };
        let (x_arg, one) = match (interpolated, x) {
            (Some(Float), _) => (
                if x.is_some() {
                    Arg::Convert(Float)
                } else {
                    Arg::Untyped(Float)
                },
                Some(Float),
            ),
            (Some(t), _) => (Arg::Keep, Some(t)),
            (None, Some(t)) if ordered(t) => (Arg::Keep, Some(t)),
            (None, None) => (Arg::Keep, None),
            (None, Some(_)) => return Err(self.no_such(args)),
        };
        let mut sig = vec![x_arg];
        let mut returns = one;
        if self.0 == Kind::Cont || self.0 == Kind::Disc {
            let f = &args[1];
            let (arg, many) = match f.ty {
                None => (Arg::Untyped(Float), false),
                Some(t) if t.is_number() => (Arg::Convert(Float), false),
                Some(Array(e)) if e.is_number() => (Arg::Convert(Array(&Float)), true),
                Some(_) => return Err(self.no_such(args)),
            };
            sig.push(arg);
            if many {
                returns = match one {
                    Some(t) => Some(t.array_of().ok_or_else(|| {
                        Error::feature(format!(
                            "{} of {} values at an array of fractions is not supported",
                            self.1,
                            crate::builtin::type_of(t).name()
                        ))
                    })?),
                    None => None,
                };
            }
        }
        if self.0 != Kind::Median {
            sig.push(Arg::Keep); // the direction
        }
        Ok(Signature::new(sig, Returns::Type(returns)))
    }

    fn check_direct(&self, direct: &[Option<Value>]) -> Result<()> {
        match direct.first() {
            Some(Some(Value::Array(fs))) => fs
                .iter()
                .filter_map(as_f64)
                .try_for_each(|f| check(f).map(drop)),
            Some(Some(f)) => as_f64(f).map_or(Ok(()), |f| check(f).map(drop)),
            _ => Ok(()),
        }
    }

    fn start(&self) -> Box<dyn Accumulator> {
        let mut g = Groups::new(self.layout());
        g.resize(1);
        Box::new(One(g))
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Groups::new(self.layout())))
    }
}

/// The fractions asked for, and the call's order.
#[derive(Clone, Debug)]
struct Spec {
    fractions: Fractions,
    desc: bool,
}

#[derive(Clone, Debug)]
enum Fractions {
    /// One (`NULL`: the result is `NULL`).
    One(Option<f64>),
    /// An array's (each `NULL` gives a `NULL`).
    Many(Vec<Option<f64>>),
}

impl Spec {
    /// From a fraction argument's value (`None`: the call has none).
    fn of(fraction: Option<Value>, desc: bool) -> Spec {
        let fractions = match fraction {
            None => Fractions::One(Some(0.5)),
            Some(Value::Array(items)) => Fractions::Many(items.iter().map(as_f64).collect()),
            Some(v) => Fractions::One(as_f64(&v)),
        };
        Spec { fractions, desc }
    }

    /// As a partial state carries it: `[fraction, descending]`.
    fn value(&self) -> Value {
        let f = |x: &Option<f64>| x.map_or(Value::Null, Value::Float);
        let fraction = match &self.fractions {
            Fractions::One(x) => f(x),
            Fractions::Many(xs) => Value::Array(xs.iter().map(f).collect()),
        };
        Value::Array(vec![fraction, Value::Bool(self.desc)])
    }
}

/// What an 8-byte key stands for: a value of an integer kind (integers,
/// booleans, dates and times) or a double.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyKind {
    Int,
    Bool,
    Time(TimeKind),
    Float,
}

const SIGN: u64 = 1 << 63;

fn int_key(x: i64) -> u64 {
    (x as u64) ^ SIGN
}

fn int_of(k: u64) -> i64 {
    (k ^ SIGN) as i64
}

/// A double's order-preserving key: every `NaN` one, the greatest.
fn float_key(f: f64) -> u64 {
    let b = if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    };
    if b & SIGN != 0 { !b } else { b | SIGN }
}

fn float_of(k: u64) -> f64 {
    f64::from_bits(if k & SIGN != 0 { k & !SIGN } else { !k })
}

impl KeyKind {
    fn of(v: &Value) -> Option<(KeyKind, u64)> {
        Some(match v {
            Value::Int(i) => (KeyKind::Int, int_key(*i)),
            Value::Bool(b) => (KeyKind::Bool, int_key(i64::from(*b))),
            Value::Float(f) => (KeyKind::Float, float_key(*f)),
            v => {
                let (k, x) = TimeKind::of(v)?;
                (KeyKind::Time(k), int_key(x))
            }
        })
    }

    fn value(self, k: u64) -> Value {
        match self {
            KeyKind::Float => Value::Float(float_of(k)),
            KeyKind::Int => Value::Int(int_of(k)),
            KeyKind::Bool => Value::Bool(int_of(k) != 0),
            KeyKind::Time(t) => t.value(int_of(k)),
        }
    }

    fn tag(self) -> u8 {
        match self {
            KeyKind::Int => 0,
            KeyKind::Bool => 1,
            KeyKind::Float => 2,
            KeyKind::Time(TimeKind::Date) => 3,
            KeyKind::Time(TimeKind::Time) => 4,
            KeyKind::Time(TimeKind::Timestamp) => 5,
            KeyKind::Time(TimeKind::TimestampTz) => 6,
        }
    }

    fn from_tag(t: u8) -> Option<KeyKind> {
        Some(match t {
            0 => KeyKind::Int,
            1 => KeyKind::Bool,
            2 => KeyKind::Float,
            3 => KeyKind::Time(TimeKind::Date),
            4 => KeyKind::Time(TimeKind::Time),
            5 => KeyKind::Time(TimeKind::Timestamp),
            6 => KeyKind::Time(TimeKind::TimestampTz),
            _ => return None,
        })
    }
}

/// A group's values in memory.
#[derive(Clone, Debug, Default)]
enum Vals {
    #[default]
    Empty,
    Keys(KeyKind, Vec<u64>),
    Any(Vec<Value>),
}

impl Vals {
    fn values(self) -> Vec<Value> {
        match self {
            Vals::Empty => Vec::new(),
            Vals::Keys(k, v) => v.into_iter().map(|x| k.value(x)).collect(),
            Vals::Any(v) => v,
        }
    }

    /// The values at ascending ranks `ranks` (each below the count).
    fn select(&mut self, ranks: &[usize]) -> Vec<Value> {
        match self {
            Vals::Empty => Vec::new(),
            Vals::Keys(k, v) => {
                select(v, ranks, u64::cmp);
                ranks.iter().map(|&r| k.value(v[r])).collect()
            }
            Vals::Any(v) => {
                select(v, ranks, Value::sort_cmp);
                ranks.iter().map(|&r| v[r].clone()).collect()
            }
        }
    }

    /// Hands `visit` the values in ascending order, each run of equal
    /// keys once with its length (values: one at a time).
    fn visit_sorted(self, visit: &mut dyn FnMut(Value, u64)) {
        match self {
            Vals::Empty => {}
            Vals::Keys(k, mut v) => {
                v.sort_unstable();
                for run in v.chunk_by(|a, b| a == b) {
                    visit(k.value(run[0]), run.len() as u64);
                }
            }
            Vals::Any(mut v) => {
                v.sort_by(Value::sort_cmp);
                for x in v {
                    visit(x, 1);
                }
            }
        }
    }
}

/// Puts the elements at ascending `ranks` of `v` where a sort would.
fn select<T>(v: &mut [T], ranks: &[usize], cmp: impl Fn(&T, &T) -> Ordering + Copy) {
    let mut lo = 0;
    for &r in ranks {
        if r < lo {
            continue; // a rank repeated
        }
        v[lo..].select_nth_unstable_by(r - lo, cmp);
        lo = r + 1;
    }
}

/// A group: its count, the fractions and order (from its first row), its
/// values while they are in memory.
#[derive(Clone, Debug, Default)]
struct Group {
    n: u64,
    spec: Option<Spec>,
    vals: Vals,
}

/// An accumulator's values past `work_mem`.
enum Spill {
    /// 8-byte keys of one kind, radix-partitioned ([`Radix`]).
    Keys(KeyKind, Radix),
    /// Other values: one sort of (group, value) records.
    Sort(SortSpill),
}

/// (group, key) records in scratch space, read back by partitions that
/// each fit half of `work_mem`: groups packed whole in their order, and a
/// group larger than that split by ranges of its keys — 2¹⁶ equal slices
/// of its least to greatest key, counted in one pass.  A partition holds
/// a contiguous range of (group, key), so a group's ranks are found in
/// the one partition holding them, and its values in order partition by
/// partition.  A slice of one key (every value equal) is never written:
/// its count is enough.
struct Radix {
    run: Option<Run>,
    /// Per group: its keys written, least and greatest.
    count: Vec<u64>,
    min: Vec<u64>,
    max: Vec<u64>,
    /// The values a partition holds.
    cap: u64,
    parts: Option<Parts>,
}

struct Parts {
    runs: Vec<Arc<Sealed>>,
    place: Vec<Place>,
    /// The partition read last: its index, first group, groups' keys.
    loaded: Option<(usize, usize, Vec<Vec<u64>>)>,
}

#[derive(Clone, Debug)]
enum Place {
    None,
    /// With other whole groups in a partition, from group `first`.
    Small {
        part: usize,
        first: usize,
    },
    /// Over partitions in key order: each one's count and, when every
    /// value in it is one key, that key (the partition then empty).
    Big(Vec<(usize, u64, Option<u64>)>),
}

/// A record: the group (4 bytes), the key (8).
fn record(g: usize, key: u64) -> [u8; 12] {
    let mut r = [0u8; 12];
    r[..4].copy_from_slice(&(g as u32).to_le_bytes());
    r[4..].copy_from_slice(&key.to_le_bytes());
    r
}

fn unrecord(r: &[u8]) -> (usize, u64) {
    (
        u32::from_le_bytes(r[..4].try_into().unwrap()) as usize,
        u64::from_le_bytes(r[4..12].try_into().unwrap()),
    )
}

/// The slice of a group's key range a key falls in, and the shift that
/// makes 2¹⁶ slices of it.
fn slice_shift(min: u64, max: u64) -> u32 {
    (64 - (max - min).leading_zeros()).saturating_sub(16)
}

const SLICES: usize = 1 << 16;

impl Radix {
    fn new(limit: usize) -> Result<Radix> {
        Ok(Radix {
            run: Some(Run::new()?),
            count: Vec::new(),
            min: Vec::new(),
            max: Vec::new(),
            cap: (limit / 16).max(1024) as u64,
            parts: None,
        })
    }

    fn push(&mut self, g: usize, key: u64) -> Result<()> {
        if self.parts.is_some() {
            self.reopen()?;
        }
        if g >= self.count.len() {
            self.count.resize(g + 1, 0);
            self.min.resize(g + 1, u64::MAX);
            self.max.resize(g + 1, 0);
        }
        self.count[g] += 1;
        self.min[g] = self.min[g].min(key);
        self.max[g] = self.max[g].max(key);
        let run = self.run.as_mut().expect("a radix spill being written");
        run.push_record(&[&record(g, key)])
    }

    /// Back to one run, to take more keys (after partitions were read:
    /// a window frame growing).
    fn reopen(&mut self) -> Result<()> {
        let Some(parts) = self.parts.take() else {
            return Ok(());
        };
        let mut run = Run::new()?;
        for (g, place) in parts.place.iter().enumerate() {
            if let Place::Big(list) = place {
                for &(_, n, one) in list {
                    if let Some(k) = one {
                        for _ in 0..n {
                            run.push_record(&[&record(g, k)])?;
                        }
                    }
                }
            }
        }
        for r in &parts.runs {
            let mut it = r.iter();
            while let Some(rec) = it.next_record()? {
                run.push_record(&[rec])?;
            }
        }
        self.run = Some(run);
        Ok(())
    }

    /// The run partitioned (once).
    fn prepare(&mut self) -> Result<&mut Parts> {
        if self.parts.is_none() {
            let parts = self.partition()?;
            self.parts = Some(parts);
        }
        Ok(self.parts.as_mut().unwrap())
    }

    fn partition(&mut self) -> Result<Parts> {
        let run = self.run.take().expect("a radix spill").seal()?;
        let groups = self.count.len();
        let big: Vec<bool> = self.count.iter().map(|&n| n > self.cap).collect();
        // The big groups' slice counts, in one pass.
        let mut hist: Vec<Option<Vec<u64>>> =
            big.iter().map(|&b| b.then(|| vec![0u64; SLICES])).collect();
        if big.contains(&true) {
            let mut it = run.iter();
            while let Some(rec) = it.next_record()? {
                let (g, k) = unrecord(rec);
                if let Some(h) = &mut hist[g] {
                    h[((k - self.min[g]) >> slice_shift(self.min[g], self.max[g])) as usize] += 1;
                }
            }
        }
        // Partitions in (group, key) order; where each record goes.
        const SKIP: u32 = u32::MAX;
        let mut place = vec![Place::None; groups];
        let mut small_route = vec![SKIP; groups];
        let mut big_route: Vec<Option<Vec<u32>>> = vec![None; groups];
        let mut parts = 0usize;
        let mut open: Option<(usize, u64, usize)> = None; // part, filled, first group
        for g in 0..groups {
            let n = self.count[g];
            if n == 0 {
                continue;
            }
            if let Some(h) = hist[g].take() {
                open = None;
                let shift = slice_shift(self.min[g], self.max[g]);
                let mut route = vec![SKIP; SLICES];
                let mut list: Vec<(usize, u64, Option<u64>)> = Vec::new();
                // The partition being filled: part, count, its slices.
                let mut cur: Option<(usize, u64, usize, usize)> = None;
                let close = |cur: (usize, u64, usize, usize),
                             list: &mut Vec<(usize, u64, Option<u64>)>,
                             route: &mut Vec<u32>| {
                    let (p, c, first, last) = cur;
                    let one = (first == last && shift == 0).then(|| self.min[g] + first as u64);
                    if one.is_some() {
                        route[first] = SKIP;
                    }
                    list.push((p, c, one));
                };
                for (s, &c) in h.iter().enumerate() {
                    if c == 0 {
                        continue;
                    }
                    cur = match cur {
                        Some((p, filled, first, _)) if filled + c <= self.cap => {
                            Some((p, filled + c, first, s))
                        }
                        prev => {
                            if let Some(prev) = prev {
                                close(prev, &mut list, &mut route);
                            }
                            parts += 1;
                            Some((parts - 1, c, s, s))
                        }
                    };
                    route[s] = cur.unwrap().0 as u32;
                }
                if let Some(c) = cur {
                    close(c, &mut list, &mut route);
                }
                place[g] = Place::Big(list);
                big_route[g] = Some(route);
                continue;
            }
            let (p, first) = match open {
                Some((p, filled, first)) if filled + n <= self.cap => {
                    open = Some((p, filled + n, first));
                    (p, first)
                }
                _ => {
                    parts += 1;
                    open = Some((parts - 1, n, g));
                    (parts - 1, g)
                }
            };
            small_route[g] = p as u32;
            place[g] = Place::Small { part: p, first };
        }
        let mut runs: Vec<Run> = (0..parts).map(|_| Run::new()).collect::<Result<_>>()?;
        let mut it = run.iter();
        while let Some(rec) = it.next_record()? {
            let (g, k) = unrecord(rec);
            let p = match &big_route[g] {
                Some(route) => {
                    route[((k - self.min[g]) >> slice_shift(self.min[g], self.max[g])) as usize]
                }
                None => small_route[g],
            };
            if p != SKIP {
                runs[p as usize].push_record(&[rec])?;
            }
        }
        Ok(Parts {
            runs: runs.into_iter().map(Run::seal).collect::<Result<_>>()?,
            place,
            loaded: None,
        })
    }

    /// Partition `p`'s keys, by group from `first` (a big group's: all
    /// one group's).
    fn read(parts: &Parts, p: usize, first: usize) -> Result<Vec<Vec<u64>>> {
        let mut by: Vec<Vec<u64>> = Vec::new();
        let mut it = parts.runs[p].iter();
        while let Some(rec) = it.next_record()? {
            let (g, k) = unrecord(rec);
            let i = g - first;
            if i >= by.len() {
                by.resize_with(i + 1, Vec::new);
            }
            by[i].push(k);
        }
        Ok(by)
    }

    /// A small group's keys (its partition read once for all its groups).
    fn small(&mut self, g: usize) -> Result<&mut Vec<u64>> {
        let parts = self.prepare()?;
        let Place::Small { part, first } = parts.place[g] else {
            return Err(Error::internal("a radix group not small"));
        };
        if parts.loaded.as_ref().is_none_or(|l| l.0 != part) {
            let by = Radix::read(parts, part, first)?;
            parts.loaded = Some((part, first, by));
        }
        let by = &mut parts.loaded.as_mut().unwrap().2;
        if g - first >= by.len() {
            by.resize_with(g - first + 1, Vec::new);
        }
        Ok(&mut by[g - first])
    }

    /// Group `g`'s keys at ascending ranks.
    fn select(&mut self, g: usize, ranks: &[usize]) -> Result<Vec<u64>> {
        let place = self.prepare()?.place[g].clone();
        match place {
            Place::None => Ok(Vec::new()),
            Place::Small { .. } => {
                let v = self.small(g)?;
                select(v, ranks, u64::cmp);
                Ok(ranks.iter().map(|&r| v[r]).collect())
            }
            Place::Big(list) => {
                let parts = self.parts.as_ref().unwrap();
                let mut out = Vec::with_capacity(ranks.len());
                let (mut before, mut i) = (0u64, 0);
                for (p, c, one) in list {
                    let here: Vec<usize> = ranks[i..]
                        .iter()
                        .take_while(|&&r| (r as u64) < before + c)
                        .map(|&r| r - before as usize)
                        .collect();
                    i += here.len();
                    if !here.is_empty() {
                        match one {
                            Some(k) => out.extend(here.iter().map(|_| k)),
                            None => {
                                let mut v = Radix::read(parts, p, g)?.pop().unwrap_or_default();
                                select(&mut v, &here, u64::cmp);
                                out.extend(here.iter().map(|&r| v[r]));
                            }
                        }
                    }
                    before += c;
                }
                Ok(out)
            }
        }
    }

    /// Hands `visit` group `g`'s keys in ascending order, a run of equal
    /// ones at a time with its length.
    fn visit(&mut self, g: usize, visit: &mut dyn FnMut(u64, u64)) -> Result<()> {
        let place = self.prepare()?.place[g].clone();
        fn runs(v: &mut [u64], visit: &mut dyn FnMut(u64, u64)) {
            v.sort_unstable();
            for run in v.chunk_by(|a, b| a == b) {
                visit(run[0], run.len() as u64);
            }
        }
        match place {
            Place::None => {}
            Place::Small { .. } => runs(&mut self.small(g)?.clone(), visit),
            Place::Big(list) => {
                let parts = self.parts.as_ref().unwrap();
                for (p, c, one) in list {
                    match one {
                        Some(k) => visit(k, c),
                        None => runs(
                            &mut Radix::read(parts, p, g)?.pop().unwrap_or_default(),
                            visit,
                        ),
                    }
                }
            }
        }
        Ok(())
    }
}

/// Values past `work_mem` that are not 8-byte keys: one sort.
struct SortSpill {
    sorter: Option<Sorter>,
    parts: Vec<Arc<Part>>,
    rows: Option<Box<dyn Iterator<Item = Result<Row>>>>,
    /// The next group the merged rows hold.
    next: usize,
}

fn sort_flags() -> Arc<[(bool, bool)]> {
    Arc::from(vec![(false, false); 2])
}

impl SortSpill {
    fn new() -> SortSpill {
        SortSpill {
            sorter: Some(Sorter::new(sort_flags(), Budget::new(1))),
            parts: Vec::new(),
            rows: None,
            next: 0,
        }
    }

    fn push(&mut self, g: usize, v: Value) -> Result<()> {
        if self.sorter.is_none() {
            // Read already (a window frame growing): sorted again with
            // the new values.
            let mut sorter = Sorter::new(sort_flags(), Budget::new(1));
            for row in sorter::merge(std::mem::take(&mut self.parts), sort_flags())? {
                let row = row?;
                sorter.push(&row, &row[1..])?;
            }
            self.sorter = Some(sorter);
            self.rows = None;
        }
        let key = [Value::Int(g as i64), v];
        self.sorter.as_mut().unwrap().push(&key, &key[1..])
    }

    /// Hands `visit` group `g`'s values in ascending order, the groups
    /// before it passed over (`counts`: each group's).
    fn visit(&mut self, g: usize, counts: &[u64], visit: &mut dyn FnMut(Value)) -> Result<()> {
        if let Some(sorter) = self.sorter.take() {
            self.parts = sorter.finish()?;
            self.rows = None;
        }
        if self.rows.is_none() || g < self.next {
            self.rows = Some(sorter::merge(self.parts.clone(), sort_flags())?);
            self.next = 0;
        }
        let rows = self.rows.as_mut().unwrap();
        let mut next = || -> Result<Row> {
            rows.next()
                .ok_or_else(|| Error::internal("an ordered-set sort ended early"))?
        };
        for &n in &counts[self.next..g] {
            for _ in 0..n {
                next()?;
            }
        }
        self.next = g + 1;
        for _ in 0..counts[g] {
            visit(next()?.swap_remove(0));
        }
        Ok(())
    }
}

/// The states of an ordered-set aggregate's groups.
struct Groups {
    layout: Layout,
    groups: Vec<Group>,
    /// Bytes of values in memory, and the most before they spill.
    held: usize,
    limit: usize,
    spill: Option<Spill>,
    /// The values are doubles or intervals (seen at the first): `median`
    /// interpolates them.
    interpolable: Option<bool>,
}

impl Groups {
    fn new(layout: Layout) -> Self {
        Groups {
            layout,
            groups: Vec::new(),
            held: 0,
            limit: crate::budget::work_mem(),
            spill: None,
            interpolable: None,
        }
    }

    /// The fractions and order from a row's direct arguments.
    fn spec(&self, read: impl Fn(usize) -> Value) -> Spec {
        let desc = self
            .layout
            .desc
            .is_some_and(|i| matches!(read(i), Value::Bool(true)));
        Spec::of(self.layout.fraction.map(&read), desc)
    }

    fn push_key(&mut self, g: usize, kind: KeyKind, key: u64) -> Result<()> {
        self.interpolable.get_or_insert(kind == KeyKind::Float);
        self.groups[g].n += 1;
        match &mut self.spill {
            Some(Spill::Keys(k, radix)) if *k == kind => return radix.push(g, key),
            Some(Spill::Sort(s)) => return s.push(g, kind.value(key)),
            Some(Spill::Keys(..)) => {
                return Err(Error::internal("values of two kinds in one ordered set"));
            }
            None => {}
        }
        let vals = &mut self.groups[g].vals;
        match vals {
            Vals::Keys(k, v) if *k == kind => v.push(key),
            Vals::Empty => *vals = Vals::Keys(kind, vec![key]),
            other => {
                let mut all = std::mem::take(other).values();
                all.push(kind.value(key));
                *other = Vals::Any(all);
            }
        }
        self.grew(8)
    }

    fn push_value(&mut self, g: usize, v: Value) -> Result<()> {
        if let Some((kind, key)) = KeyKind::of(&v) {
            return self.push_key(g, kind, key);
        }
        self.interpolable
            .get_or_insert(matches!(v, Value::Interval(_)));
        self.groups[g].n += 1;
        match &mut self.spill {
            Some(Spill::Sort(s)) => return s.push(g, v),
            Some(Spill::Keys(..)) => {
                return Err(Error::internal("values of two kinds in one ordered set"));
            }
            None => {}
        }
        let size = crate::spill::row_bytes(std::slice::from_ref(&v));
        let vals = &mut self.groups[g].vals;
        match vals {
            Vals::Any(all) => all.push(v),
            other => {
                let mut all = std::mem::take(other).values();
                all.push(v);
                *other = Vals::Any(all);
            }
        }
        self.grew(size)
    }

    fn grew(&mut self, bytes: usize) -> Result<()> {
        self.held += bytes;
        if self.held > self.limit {
            self.start_spill()?;
        }
        Ok(())
    }

    /// Moves every value in memory to scratch space: 8-byte keys of one
    /// kind radix-partitioned, other values sorted.
    fn start_spill(&mut self) -> Result<()> {
        let mut kind = None;
        let keys = self.groups.iter().all(|grp| match &grp.vals {
            Vals::Empty => true,
            Vals::Keys(k, _) => *kind.get_or_insert(*k) == *k,
            Vals::Any(_) => false,
        });
        let mut spill = match kind {
            Some(k) if keys => Spill::Keys(k, Radix::new(self.limit)?),
            _ => Spill::Sort(SortSpill::new()),
        };
        for (g, grp) in self.groups.iter_mut().enumerate() {
            match (std::mem::take(&mut grp.vals), &mut spill) {
                (Vals::Keys(_, v), Spill::Keys(_, radix)) => {
                    for k in v {
                        radix.push(g, k)?;
                    }
                }
                (vals, Spill::Sort(s)) => {
                    for v in vals.values() {
                        s.push(g, v)?;
                    }
                }
                (vals, Spill::Keys(..)) => {
                    debug_assert!(matches!(vals, Vals::Empty));
                }
            }
        }
        self.held = 0;
        self.spill = Some(spill);
        Ok(())
    }

    fn add_row(&mut self, g: usize, read: impl Fn(usize) -> Value) -> Result<()> {
        if self.groups[g].spec.is_none() {
            self.groups[g].spec = Some(self.spec(&read));
        }
        self.push_value(g, read(0))
    }

    fn counts(&self) -> Vec<u64> {
        self.groups.iter().map(|grp| grp.n).collect()
    }

    /// Group `g`'s values at ascending ranks.
    fn values_at(&mut self, g: usize, ranks: &[usize]) -> Result<Vec<Value>> {
        let counts = self.counts();
        match &mut self.spill {
            None => Ok(self.groups[g].vals.select(ranks)),
            Some(Spill::Keys(kind, radix)) => {
                let kind = *kind;
                Ok(radix
                    .select(g, ranks)?
                    .into_iter()
                    .map(|k| kind.value(k))
                    .collect())
            }
            Some(Spill::Sort(s)) => {
                let mut at = Vec::with_capacity(ranks.len());
                let mut i = 0;
                s.visit(g, &counts, &mut |v| {
                    if at.len() < ranks.len() && ranks[at.len()] == i {
                        at.push(v);
                    }
                    i += 1;
                })?;
                Ok(at)
            }
        }
    }

    /// Hands `visit` group `g`'s values in ascending order, runs of equal
    /// 8-byte keys once with their lengths.  `keep`: leave the group's
    /// values in memory as they were.
    fn visit_sorted(
        &mut self,
        g: usize,
        keep: bool,
        visit: &mut dyn FnMut(Value, u64),
    ) -> Result<()> {
        let counts = self.counts();
        match &mut self.spill {
            None => {
                let vals = if keep {
                    self.groups[g].vals.clone()
                } else {
                    std::mem::take(&mut self.groups[g].vals)
                };
                vals.visit_sorted(visit);
                Ok(())
            }
            Some(Spill::Keys(kind, radix)) => {
                let kind = *kind;
                radix.visit(g, &mut |k, n| visit(kind.value(k), n))
            }
            Some(Spill::Sort(s)) => s.visit(g, &counts, &mut |v| visit(v, 1)),
        }
    }

    /// Group `g`'s result.
    fn result(&mut self, g: usize) -> Result<Value> {
        let n = self.groups[g].n;
        let Some(spec) = self.groups[g].spec.clone() else {
            return Ok(Value::Null);
        };
        if n == 0 {
            return Ok(Value::Null);
        }
        let kind = self.layout.kind;
        if kind == Kind::Mode {
            let mut fold = Mode::new(spec.desc);
            self.visit_sorted(g, false, &mut |v, c| fold.push(v, c))?;
            return Ok(fold.finish());
        }
        let fractions: Vec<Option<f64>> = match &spec.fractions {
            Fractions::One(f) => vec![*f],
            Fractions::Many(fs) => fs.clone(),
        };
        let fractions = fractions
            .into_iter()
            .map(|f| f.map(check).transpose())
            .collect::<Result<Vec<_>>>()?;
        let cont = match kind {
            Kind::Cont => true,
            Kind::Median => self.interpolable == Some(true),
            _ => false,
        };
        let wanted: Vec<Option<(usize, usize, f64)>> = fractions
            .iter()
            .map(|f| f.map(|f| ranks_of(cont, f, n, spec.desc)))
            .collect();
        let mut ranks: Vec<usize> = wanted
            .iter()
            .flatten()
            .flat_map(|&(a, b, _)| [a, b])
            .collect();
        ranks.sort_unstable();
        ranks.dedup();
        let at = self.values_at(g, &ranks)?;
        if self.spill.is_none() {
            self.groups[g].vals = Vals::Empty;
        }
        let value = |r: usize| &at[ranks.binary_search(&r).unwrap()];
        let mut out = Vec::with_capacity(wanted.len());
        for w in &wanted {
            out.push(match *w {
                None => Value::Null,
                Some((a, b, _)) if a == b => value(a).clone(),
                Some((a, b, t)) => lerp(value(a), value(b), t)?,
            });
        }
        Ok(match spec.fractions {
            Fractions::One(_) => out.pop().unwrap(),
            Fractions::Many(_) => Value::Array(out),
        })
    }

    /// Group `g`'s state as one value: `[[fraction, descending], values]`,
    /// the values 8-byte keys in bytes (a kind's tag first) or an array.
    fn partial_of(&mut self, g: usize) -> Result<Value> {
        let Some(spec) = self.groups[g].spec.clone() else {
            return Ok(Value::Null);
        };
        let mut keys: Option<(KeyKind, Vec<u8>)> = None;
        let mut any: Vec<Value> = Vec::new();
        let tagged = |k: KeyKind, n: u64| {
            let mut b = Vec::with_capacity(1 + 8 * n as usize);
            b.push(k.tag());
            (k, b)
        };
        let n = self.groups[g].n;
        match (&self.spill, &self.groups[g].vals) {
            (None, Vals::Keys(k, v)) => {
                let (k, mut b) = tagged(*k, n);
                for x in v {
                    b.extend_from_slice(&x.to_le_bytes());
                }
                keys = Some((k, b));
            }
            (None, Vals::Any(v)) => any = v.clone(),
            (None, Vals::Empty) => {}
            (Some(Spill::Keys(kind, _)), _) => {
                let (k, mut b) = tagged(*kind, n);
                let Some(Spill::Keys(_, radix)) = &mut self.spill else {
                    unreachable!()
                };
                radix.visit(g, &mut |x, c| {
                    for _ in 0..c {
                        b.extend_from_slice(&x.to_le_bytes());
                    }
                })?;
                keys = Some((k, b));
            }
            (Some(Spill::Sort(_)), _) => {
                self.visit_sorted(g, true, &mut |v, _| any.push(v))?;
            }
        }
        let values = match keys {
            Some((_, b)) => Value::Bytea(b.into()),
            None => Value::Array(any),
        };
        Ok(Value::Array(vec![spec.value(), values]))
    }

    fn combine_from(&mut self, g: usize, partial: Value) -> Result<()> {
        let Value::Array(mut parts) = partial else {
            return Ok(()); // no rows
        };
        let values = parts.pop().unwrap_or(Value::Null);
        if self.groups[g].spec.is_none()
            && let Some(Value::Array(spec)) = parts.pop()
        {
            let mut spec = spec.into_iter();
            let fraction = spec.next().unwrap_or(Value::Null);
            let desc = matches!(spec.next(), Some(Value::Bool(true)));
            let fraction = self.layout.fraction.map(|_| fraction);
            self.groups[g].spec = Some(Spec::of(fraction, desc));
        }
        match values {
            Value::Bytea(b) => {
                let kind = KeyKind::from_tag(b[0])
                    .ok_or_else(|| Error::internal("an ordered set's partial state"))?;
                for x in b[1..].as_chunks::<8>().0 {
                    self.push_key(g, kind, u64::from_le_bytes(*x))?;
                }
            }
            Value::Array(vs) => {
                for v in vs {
                    self.push_value(g, v)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Real(f) => Some(f64::from(*f)),
        Value::Int(i) => Some(*i as f64),
        Value::Numeric(d) => Some(d.to_f64()),
        _ => None,
    }
}

fn check(f: f64) -> Result<f64> {
    if (0.0..=1.0).contains(&f) {
        Ok(f)
    } else {
        Err(Error::out_of_range(format!(
            "percentile value {} is not between 0 and 1",
            Value::Float(f).render()
        ))
        .with_hint("A percentile is a fraction: 0.95, not 95."))
    }
}

/// One fraction's ranks in ascending order: the two to interpolate
/// between (the same twice when none is needed), or the one taken.
fn ranks_of(cont: bool, f: f64, n: u64, desc: bool) -> (usize, usize, f64) {
    let asc = |i: u64| (if desc { n - 1 - i } else { i }) as usize;
    if cont {
        let m = (n - 1) as f64;
        let pos = f * m;
        let (lo, hi) = (pos.floor(), pos.ceil());
        // `f·m − lo` rounded once (a fused multiply-add).
        (asc(lo as u64), asc(hi as u64), f.mul_add(m, -lo))
    } else {
        let row = (f * n as f64).ceil() as u64;
        let i = if row > 1 { (row - 1).min(n - 1) } else { 0 };
        (asc(i), asc(i), 0.0)
    }
}

/// `lo + (hi − lo)·t`, as PostgreSQL's `float8_lerp` and `interval_lerp`.
/// Doubles take `t·(hi − lo) + lo` rounded once, as the position's
/// fraction (`ranks_of`): PostgreSQL's bits where its compiler fuses them
/// (arm64's GCC does; an x86-64 build rounds each step), and the exact
/// interpolation's more often.
fn lerp(lo: &Value, hi: &Value, t: f64) -> Result<Value> {
    Ok(match (lo, hi) {
        (Value::Float(a), Value::Float(b)) => Value::Float(t.mul_add(b - a, *a)),
        (Value::Interval(a), Value::Interval(b)) => {
            Value::Interval(b.checked_sub(*a)?.checked_mul(t)?.checked_add(*a)?)
        }
        _ => {
            return Err(Error::internal(
                "percentile_cont of a value it cannot interpolate",
            ));
        }
    })
}

/// The most frequent of values given in ascending order, ties to the
/// first in the call's order: runs counted as they pass.
struct Mode {
    desc: bool,
    run: Option<(Value, u64)>,
    best: Option<(Value, u64)>,
}

impl Mode {
    fn new(desc: bool) -> Self {
        Mode {
            desc,
            run: None,
            best: None,
        }
    }

    fn push(&mut self, v: Value, count: u64) {
        match &mut self.run {
            Some((r, c)) if r.sort_cmp(&v) == Ordering::Equal => *c += count,
            _ => {
                self.end_run();
                self.run = Some((v, count));
            }
        }
    }

    fn end_run(&mut self) {
        if let Some((v, c)) = self.run.take() {
            let better = match &self.best {
                None => true,
                Some((_, b)) => c > *b || (self.desc && c == *b),
            };
            if better {
                self.best = Some((v, c));
            }
        }
    }

    fn finish(mut self) -> Value {
        self.end_run();
        self.best.map_or(Value::Null, |(v, _)| v)
    }
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        self.groups
            .resize_with(groups.max(self.groups.len()), Group::default);
    }

    fn group_bytes(&self) -> usize {
        64
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        for (&r, &g) in rows.iter().zip(groups) {
            let (r, g) = (r as usize, g as usize);
            if self.groups[g].spec.is_none() {
                let spec = self.spec(|i| args[i].value(r));
                self.groups[g].spec = Some(spec);
            }
            match args[0] {
                Vector::Float(d, _) => self.push_key(g, KeyKind::Float, float_key(d[r]))?,
                Vector::Int(d, _) => self.push_key(g, KeyKind::Int, int_key(d[r]))?,
                Vector::Time(k, d, _) => self.push_key(g, KeyKind::Time(*k), int_key(d[r]))?,
                v => self.push_value(g, v.value(r))?,
            }
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        self.partial_of(group)
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        self.combine_from(group, partial)
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.result(group)
    }
}

/// One group's state (the row-at-a-time executor's).
struct One(Groups);

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.0.add_row(0, |i| args[i].clone())
    }

    fn finish(&mut self) -> Result<Value> {
        self.0.result(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_order_as_sql() {
        let xs = [
            f64::NEG_INFINITY,
            -1e300,
            -1.0,
            -1e-300,
            -0.0,
            0.0,
            1e-300,
            1.0,
            f64::INFINITY,
            f64::NAN,
        ];
        for w in xs.windows(2) {
            assert!(float_key(w[0]) <= float_key(w[1]), "{w:?}");
        }
        for x in xs {
            assert_eq!(float_of(float_key(x)).to_bits(), x.to_bits());
        }
        assert_eq!(float_key(-f64::NAN), float_key(f64::NAN));
        for w in [i64::MIN, -1, 0, 1, i64::MAX].windows(2) {
            assert!(int_key(w[0]) < int_key(w[1]));
            assert_eq!(int_of(int_key(w[0])), w[0]);
        }
    }

    #[test]
    fn selection_finds_every_rank() {
        let mut seed = 7u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for n in [1usize, 2, 3, 10, 100, 1000] {
            let v: Vec<i64> = (0..n).map(|_| (next() % 50) as i64).collect();
            let mut sorted = v.clone();
            sorted.sort();
            let mut ranks: Vec<usize> = (0..5).map(|_| (next() as usize) % n).collect();
            ranks.sort();
            let mut w = v.clone();
            select(&mut w, &ranks, i64::cmp);
            for &r in &ranks {
                assert_eq!(w[r], sorted[r], "n {n} rank {r}");
            }
        }
    }

    /// The radix spill against the values sorted: small groups packed,
    /// big ones split by key ranges (spread, clustered, all equal, a few
    /// distinct), every rank and every run.
    #[test]
    fn radix_partitions_give_every_rank_and_run() {
        let mut seed = 11u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let shapes: Vec<Box<dyn Fn(u64) -> u64>> = vec![
            Box::new(|r| r),                       // spread over all 64 bits
            Box::new(|r| (1 << 40) + r % 5000),    // clustered
            Box::new(|_| 42),                      // all equal
            Box::new(|r| 7 + (r % 3) * (1 << 50)), // three far apart
            Box::new(|r| r % 70_000),              // more keys than slices
        ];
        for (si, shape) in shapes.iter().enumerate() {
            let mut radix = Radix::new(0).unwrap(); // cap: 1024 values
            radix.cap = 1024;
            let sizes = [3usize, 5000, 1, 800, 300, 12_000, 0, 50];
            let mut want: Vec<Vec<u64>> = vec![Vec::new(); sizes.len()];
            // Interleaved, as rows come.
            let mut left: Vec<usize> = sizes.to_vec();
            while left.iter().any(|&n| n > 0) {
                let g = (next() as usize) % sizes.len();
                if left[g] == 0 {
                    continue;
                }
                left[g] -= 1;
                let k = shape(next());
                want[g].push(k);
                radix.push(g, k).unwrap();
            }
            for (g, w) in want.iter_mut().enumerate() {
                w.sort_unstable();
                if w.is_empty() {
                    continue;
                }
                let ranks: Vec<usize> = {
                    let mut r: Vec<usize> = (0..20).map(|_| (next() as usize) % w.len()).collect();
                    r.extend([0, w.len() - 1]);
                    r.sort_unstable();
                    r.dedup();
                    r
                };
                let got = radix.select(g, &ranks).unwrap();
                let exp: Vec<u64> = ranks.iter().map(|&r| w[r]).collect();
                assert_eq!(got, exp, "shape {si} group {g}");
                let mut all = Vec::new();
                radix
                    .visit(g, &mut |k, n| all.extend((0..n).map(|_| k)))
                    .unwrap();
                assert_eq!(&all, w, "shape {si} group {g} in order");
            }
            // More keys after reading: back to one run, read again.
            radix.push(1, shape(1)).unwrap();
            want[1].push(shape(1));
            want[1].sort_unstable();
            let mut all = Vec::new();
            radix
                .visit(1, &mut |k, n| all.extend((0..n).map(|_| k)))
                .unwrap();
            assert_eq!(all, want[1], "shape {si} reopened");
        }
    }
}
