//! Window functions (PLAN.md §M14.27 S4): `func(…) OVER (PARTITION BY …
//! ORDER BY … frame)` — every aggregate over its frame, and the ranking
//! and value functions.
//!
//! The functions over one window share its sort: the rows sorted by the
//! partition and order keys (a `Sort` below, so the input may be past
//! `work_mem`), then a partition at a time in memory (as PostgreSQL's
//! tuplestore, and HyPer's per-partition evaluation — Leis et al.,
//! "Efficient Processing of Window Functions in Analytical SQL Queries",
//! VLDB 2015).  Over a partition, an aggregate's frames are evaluated by
//! the cheapest way that fits them:
//!
//! - the whole partition: one state, one result;
//! - frames that start at the partition's start (a running total): one
//!   state grown row by row, a copy finished at each distinct frame end;
//! - any other frames: Leis et al.'s segment tree of partial states, a
//!   frame the combination of O(log n) of them in order — for every
//!   aggregate whose states combine ([`GroupAccumulator::combinable`]);
//! - an aggregate whose states do not combine: each distinct frame's rows
//!   added to a fresh state.
//!
//! Rows come out in the order of the last window's sort.

use std::cmp::Ordering;

use crate::ast::{BinaryOp, FrameExclude, FrameUnits};
use crate::error::{Error, Result};
use crate::exec::{Rows, env};
use crate::expr::{Ctx, Env, Expr};
use crate::ext::{AggRef, GroupAccumulator};
use crate::plan::{WinBound, WinFunc, WindowFunc};
use crate::types::{Row, Value};
use crate::vector::Vector;

/// The rows of `rows` (each `width` columns), each followed by the value of
/// every function in `funcs` — functions over one window, its partition
/// and order shared.  `rows` come sorted by the partition's keys, then the
/// order's: the binder puts a `Sort` below (so the sort is the one every
/// `ORDER BY` uses: in parallel over a pipeline, past `work_mem` by runs).
pub fn run<'a>(
    rows: Rows<'a>,
    width: usize,
    funcs: &'a [WindowFunc],
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
) -> Result<Rows<'a>> {
    let w = &funcs[0];
    let mut flags = vec![(false, false); w.partition.len()];
    flags.extend(w.order.iter().map(|k| (k.desc, k.nulls_first)));
    // Each key's column: a column's own, or one computed after the
    // functions' while the row is in its partition.
    let full = width + funcs.len();
    let (mut cols, mut computed) = (Vec::new(), Vec::new());
    for x in w.partition.iter().chain(w.order.iter().map(|k| &k.expr)) {
        match x {
            Expr::Column(c) => cols.push(*c),
            _ => {
                cols.push(full + computed.len());
                computed.push(x);
            }
        }
    }
    let e = env(&[], ctx, outer);
    let frames = funcs
        .iter()
        .map(|f| Frame::new(f, &e))
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(Partitions {
        input: rows,
        pending: None,
        out: Vec::new().into_iter(),
        width,
        np: w.partition.len(),
        flags,
        cols,
        computed,
        funcs,
        frames,
        ctx,
        outer,
        failed: false,
    }))
}

/// The rows a partition at a time, each with its functions' values.
struct Partitions<'a> {
    input: Rows<'a>,
    /// The next partition's first row, read to find this one's end.
    pending: Option<Row>,
    out: std::vec::IntoIter<Row>,
    width: usize,
    /// The partition's keys (the first `np`), then the order's: their
    /// columns, and the expressions computed into those past the
    /// functions'.
    np: usize,
    flags: Vec<(bool, bool)>,
    cols: Vec<usize>,
    computed: Vec<&'a Expr>,
    funcs: &'a [WindowFunc],
    frames: Vec<Frame>,
    ctx: &'a Ctx<'a>,
    outer: Option<&'a Env<'a>>,
    failed: bool,
}

impl Iterator for Partitions<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Result<Row>> {
        if let Some(r) = self.out.next() {
            return Some(Ok(r));
        }
        if self.failed {
            return None;
        }
        let first = match self.pending.take().map(Ok).or_else(|| self.pull())? {
            Ok(r) => r,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        };
        let mut part = vec![first];
        while let Some(next) = self.pull() {
            match next {
                Ok(row) => {
                    if !same(&self.cols[..self.np], &row, &part[0]) {
                        self.pending = Some(row);
                        break;
                    }
                    part.push(row);
                }
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
        if let Err(e) = self.fill(&mut part) {
            self.failed = true;
            return Some(Err(e));
        }
        if !self.computed.is_empty() {
            let full = self.width + self.funcs.len();
            for r in &mut part {
                r.truncate(full);
            }
        }
        self.out = part.into_iter();
        self.out.next().map(Ok)
    }
}

impl Partitions<'_> {
    /// The next row, with room for the functions' values, and its
    /// computed keys after them.
    fn pull(&mut self) -> Option<Result<Row>> {
        let full = self.width + self.funcs.len();
        let (ctx, outer, computed) = (self.ctx, self.outer, &self.computed);
        Some(self.input.next()?.and_then(|mut row| {
            let keys = if computed.is_empty() {
                Vec::new()
            } else {
                let e = env(&row, ctx, outer);
                computed.iter().map(|x| x.eval(&e)).collect::<Result<_>>()?
            };
            row.resize(full, Value::Null);
            row.extend(keys);
            Ok(row)
        }))
    }

    /// Fills the functions' slots of one partition's rows.
    fn fill(&self, part: &mut [Row]) -> Result<()> {
        let n = part.len();
        let ocols = &self.cols[self.np..];
        // Peer groups: rows whose order keys are equal.
        let mut group = vec![0usize; n];
        let mut starts = vec![0usize];
        for i in 1..n {
            if !same(ocols, &part[i], &part[i - 1]) {
                starts.push(i);
            }
            group[i] = starts.len() - 1;
        }
        let shape = Shape {
            n,
            group,
            starts,
            rows: part,
            key: ocols.first().copied(),
            key_flags: self.flags.get(self.np).copied(),
        };
        let values = self
            .funcs
            .iter()
            .zip(&self.frames)
            .map(|(f, frame)| self.evaluate(f, frame, &shape))
            .collect::<Result<Vec<_>>>()?;
        for (k, vs) in values.into_iter().enumerate() {
            for (r, v) in part.iter_mut().zip(vs) {
                r[self.width + k] = v;
            }
        }
        Ok(())
    }

    /// `x` over `row`: a column or a constant read without an environment.
    fn value(&self, x: &Expr, row: &Row) -> Result<Value> {
        match x {
            Expr::Column(c) => Ok(row[*c].clone()),
            Expr::Literal(v) => Ok(v.clone()),
            _ => x.eval(&env(row, self.ctx, self.outer)),
        }
    }

    fn evaluate(&self, f: &WindowFunc, frame: &Frame, shape: &Shape) -> Result<Vec<Value>> {
        let (n, part) = (shape.n, shape.rows);
        let eval = |i: usize, a: usize| self.value(&f.args[a], &part[i]);
        let int = |v: Value| match v {
            Value::Int(i) => Ok(Some(i)),
            Value::Null => Ok(None),
            v => Err(Error::internal(format!("a window argument {v}"))),
        };
        Ok(match &f.func {
            WinFunc::Agg(agg) => self.aggregate(agg, f, frame, shape)?,
            WinFunc::RowNumber => (0..n).map(|i| Value::Int(i as i64 + 1)).collect(),
            WinFunc::Rank => (0..n)
                .map(|i| Value::Int(shape.starts[shape.group[i]] as i64 + 1))
                .collect(),
            WinFunc::DenseRank => (0..n)
                .map(|i| Value::Int(shape.group[i] as i64 + 1))
                .collect(),
            WinFunc::PercentRank => (0..n)
                .map(|i| {
                    let rank = shape.starts[shape.group[i]];
                    Value::Float(if n > 1 {
                        rank as f64 / (n - 1) as f64
                    } else {
                        0.0
                    })
                })
                .collect(),
            WinFunc::CumeDist => (0..n)
                .map(|i| Value::Float(shape.group_end(shape.group[i]) as f64 / n as f64))
                .collect(),
            WinFunc::Ntile => {
                // The bucket count of the partition's first row.
                let Some(k) = int(eval(0, 0)?)? else {
                    return Ok(vec![Value::Null; n]);
                };
                if k <= 0 {
                    return Err(Error::invalid_ntile(
                        "argument of ntile must be greater than zero",
                    ));
                }
                let k = (k as usize).min(n.max(1));
                // n / k rows a bucket, the first n % k buckets one more.
                let (per, extra) = (n / k, n % k);
                (0..n)
                    .map(|i| {
                        let big = extra * (per + 1);
                        let b = if i < big {
                            i / (per + 1)
                        } else {
                            extra + (i - big) / per.max(1)
                        };
                        Value::Int(b as i64 + 1)
                    })
                    .collect()
            }
            WinFunc::Lag | WinFunc::Lead => {
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let Some(off) = int(eval(i, 1)?)? else {
                        out.push(Value::Null);
                        continue;
                    };
                    let off = if f.func == WinFunc::Lag {
                        off.saturating_neg()
                    } else {
                        off
                    };
                    let j = (i as i64).saturating_add(off);
                    out.push(if (0..n as i64).contains(&j) {
                        eval(j as usize, 0)?
                    } else {
                        eval(i, 2)?
                    });
                }
                out
            }
            WinFunc::FirstValue | WinFunc::LastValue | WinFunc::NthValue => {
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let ranges = frame.ranges(i, shape)?;
                    let at = match f.func {
                        WinFunc::FirstValue => ranges.first().map(|r| r.0),
                        WinFunc::LastValue => ranges.last().map(|r| r.1 - 1),
                        _ => {
                            let Some(k) = int(eval(i, 1)?)? else {
                                out.push(Value::Null);
                                continue;
                            };
                            if k <= 0 {
                                return Err(Error::invalid_nth_value(
                                    "argument of nth_value must be greater than zero",
                                ));
                            }
                            nth(&ranges, k as usize - 1)
                        }
                    };
                    out.push(match at {
                        Some(j) => eval(j, 0)?,
                        None => Value::Null,
                    });
                }
                out
            }
        })
    }

    /// An aggregate over each row's frame.
    fn aggregate(
        &self,
        agg: &AggRef,
        f: &WindowFunc,
        frame: &Frame,
        shape: &Shape,
    ) -> Result<Vec<Value>> {
        let (n, part) = (shape.n, shape.rows);
        // Each row's arguments, and whether it is fed (a NULL first
        // argument is not, unless the aggregate keeps them).
        let mut args: Vec<Vec<Value>> = vec![Vec::with_capacity(n); f.args.len()];
        let mut fed = vec![true; n];
        for (i, row) in part.iter().enumerate() {
            for (a, x) in f.args.iter().enumerate() {
                args[a].push(self.value(x, row)?);
            }
            if !agg.keeps_nulls() && args.first().is_some_and(|a| a[i].is_null()) {
                fed[i] = false;
            }
        }
        let frames: Vec<Ranges> = (0..n)
            .map(|i| frame.ranges(i, shape))
            .collect::<Result<_>>()?;
        let row_args = |i: usize| args.iter().map(|a| a[i].clone()).collect::<Vec<_>>();
        // The whole partition: one state.
        if frames.iter().all(|r| **r == [(0, n)]) {
            let mut acc = agg.start();
            for i in (0..n).filter(|&i| fed[i]) {
                acc.add(&row_args(i))?;
            }
            return Ok(vec![acc.finish()?; n]);
        }
        let combinable = agg.grouped().filter(|g| g.combinable());
        let Some(mut ga) = combinable else {
            // States that do not combine: each distinct frame from scratch.
            let mut out: Vec<Value> = Vec::with_capacity(n);
            for i in 0..n {
                if i > 0 && frames[i] == frames[i - 1] {
                    out.push(out[i - 1].clone());
                    continue;
                }
                let mut acc = agg.start();
                for &(a, b) in frames[i].iter() {
                    for j in (a..b).filter(|&j| fed[j]) {
                        acc.add(&row_args(j))?;
                    }
                }
                out.push(acc.finish()?);
            }
            return Ok(out);
        };
        let vectors: Vec<Vector> = args.into_iter().map(Vector::Values).collect();
        let refs: Vec<&Vector> = vectors.iter().collect();
        let mut out: Vec<Value> = Vec::with_capacity(n);
        let running = frames
            .iter()
            .all(|r| r.len() <= 1 && r.first().is_none_or(|r| r.0 == 0));
        if running {
            // From the partition's start: group 0 grows, and is finished
            // at each frame's end — itself where finishing keeps its state,
            // else a copy of it.
            let keeps = ga.finish_keeps_state();
            let mut scratch = Scratch::new(agg)?;
            ga.resize(1);
            let mut fed_to = 0;
            for i in 0..n {
                let end = frames[i].first().map_or(0, |r| r.1);
                if i > 0 && frames[i] == frames[i - 1] {
                    out.push(out[i - 1].clone());
                    continue;
                }
                if end > fed_to {
                    let rows: Vec<u32> = (fed_to..end)
                        .filter(|&j| fed[j])
                        .map(|j| j as u32)
                        .collect();
                    ga.update(&rows, &vec![0; rows.len()], &refs)?;
                    fed_to = end;
                }
                if keeps {
                    out.push(ga.finish(0)?);
                    continue;
                }
                let (acc, g) = scratch.group()?;
                if end > 0 {
                    acc.combine(g, ga.partial(0)?)?;
                }
                out.push(acc.finish(g)?);
            }
            return Ok(out);
        }
        // A frame of few rows: its rows added to a fresh group (cheaper
        // than combining a tree's O(log n) partial states).  Larger ones:
        // a segment tree of partial states, leaf m + i the row i, node t
        // nodes 2t and 2t + 1 combined (in order: the rows' order kept),
        // built when a frame first needs it.
        const SMALL: usize = 64;
        let small = |r: &Ranges| r.iter().map(|(a, b)| b - a).sum::<usize>() <= SMALL;
        let m = n;
        let mut partials: Vec<Option<Value>> = Vec::new();
        let mut scratch = Scratch::new(agg)?;
        let mut rows: Vec<u32> = Vec::new();
        let mut groups: Vec<u32> = Vec::new();
        for i in 0..n {
            if i > 0 && frames[i] == frames[i - 1] {
                out.push(out[i - 1].clone());
                continue;
            }
            let (acc, g) = scratch.group()?;
            if small(&frames[i]) {
                rows.clear();
                for &(a, b) in frames[i].iter() {
                    rows.extend((a..b).filter(|&j| fed[j]).map(|j| j as u32));
                }
                groups.clear();
                groups.resize(rows.len(), g as u32);
                acc.update(&rows, &groups, &refs)?;
                out.push(acc.finish(g)?);
                continue;
            }
            if partials.is_empty() {
                ga.resize(2 * m);
                let rows: Vec<u32> = (0..n).filter(|&j| fed[j]).map(|j| j as u32).collect();
                let groups: Vec<u32> = rows.iter().map(|&j| (m as u32) + j).collect();
                ga.update(&rows, &groups, &refs)?;
                partials = vec![None; 2 * m];
                for t in (1..m).rev() {
                    for c in [2 * t, 2 * t + 1] {
                        let p = ga.partial(c)?;
                        partials[c] = Some(p.clone());
                        ga.combine(t, p)?;
                    }
                }
            }
            for &(a, b) in frames[i].iter() {
                let (mut l, mut r) = (a + m, b + m);
                let (mut left, mut right) = (Vec::new(), Vec::new());
                while l < r {
                    if l & 1 == 1 {
                        left.push(l);
                        l += 1;
                    }
                    if r & 1 == 1 {
                        r -= 1;
                        right.push(r);
                    }
                    l >>= 1;
                    r >>= 1;
                }
                for node in left.into_iter().chain(right.into_iter().rev()) {
                    let p = match &partials[node] {
                        Some(p) => p.clone(),
                        None => {
                            let p = ga.partial(node)?;
                            partials[node] = Some(p.clone());
                            p
                        }
                    };
                    acc.combine(g, p)?;
                }
            }
            out.push(acc.finish(g)?);
        }
        Ok(out)
    }
}

/// Fresh groups for frames' states, each finished once: a new accumulator
/// every [`Scratch::GROUPS`] of them, so a partition of any length holds a
/// bounded number.
struct Scratch<'a> {
    agg: &'a AggRef,
    acc: Box<dyn GroupAccumulator>,
    used: usize,
}

impl<'a> Scratch<'a> {
    const GROUPS: usize = 1024;

    fn new(agg: &'a AggRef) -> Result<Scratch<'a>> {
        Ok(Scratch {
            agg,
            acc: Self::fresh(agg)?,
            used: 0,
        })
    }

    fn fresh(agg: &AggRef) -> Result<Box<dyn GroupAccumulator>> {
        let mut acc = agg
            .grouped()
            .ok_or_else(|| Error::internal("a window aggregate without groups"))?;
        acc.resize(Self::GROUPS);
        Ok(acc)
    }

    /// A group not used before, and its accumulator.
    fn group(&mut self) -> Result<(&mut dyn GroupAccumulator, usize)> {
        if self.used == Self::GROUPS {
            self.acc = Self::fresh(self.agg)?;
            self.used = 0;
        }
        self.used += 1;
        Ok((self.acc.as_mut(), self.used - 1))
    }
}

/// Whether rows `a` and `b` are equal in columns `cols` (`NULL`s equal, as
/// in a sort).
fn same(cols: &[usize], a: &Row, b: &Row) -> bool {
    cols.iter()
        .all(|&c| match (a[c].is_null(), b[c].is_null()) {
            (true, true) => true,
            (false, false) => a[c].sort_cmp(&b[c]) == Ordering::Equal,
            _ => false,
        })
}

/// The `k`-th row (from 0) of a frame's ranges.
fn nth(ranges: &[(usize, usize)], mut k: usize) -> Option<usize> {
    for &(a, b) in ranges {
        if k < b - a {
            return Some(a + k);
        }
        k -= b - a;
    }
    None
}

/// A frame's rows: up to three ranges, in order (`EXCLUDE` cuts one in
/// up to three).
#[derive(Clone, Copy, Default, PartialEq)]
struct Ranges {
    r: [(usize, usize); 3],
    len: usize,
}

impl std::ops::Deref for Ranges {
    type Target = [(usize, usize)];
    fn deref(&self) -> &[(usize, usize)] {
        &self.r[..self.len]
    }
}

/// A partition's rows, peer groups and ordering keys.
struct Shape<'p> {
    n: usize,
    rows: &'p [Row],
    /// Each row's peer group.
    group: Vec<usize>,
    /// Each group's first row.
    starts: Vec<usize>,
    /// The first ordering key's column (a `RANGE` offset's).
    key: Option<usize>,
    /// Its order: descending, `NULL`s first.
    key_flags: Option<(bool, bool)>,
}

impl Shape<'_> {
    /// The row after group `g`'s last.
    fn group_end(&self, g: usize) -> usize {
        self.starts.get(g + 1).copied().unwrap_or(self.n)
    }
}

/// A frame's bounds, their offsets evaluated.
struct Frame {
    units: FrameUnits,
    start: Bound,
    end: Bound,
    exclude: FrameExclude,
}

enum Bound {
    UnboundedPreceding,
    Preceding(Offset),
    CurrentRow,
    Following(Offset),
    UnboundedFollowing,
}

enum Offset {
    /// Rows or peer groups.
    Count(usize),
    /// A distance in the ordering value (`RANGE`).
    Value(Value),
}

impl Frame {
    fn new(f: &WindowFunc, e: &Env) -> Result<Frame> {
        let units = f.frame.units;
        let bound = |b: &WinBound, start: bool| -> Result<Bound> {
            let which = if start { "starting" } else { "ending" };
            let offset = |x: &crate::expr::Expr| -> Result<Offset> {
                let v = x.eval(e)?;
                if v.is_null() {
                    return Err(Error::null_value(format!(
                        "frame {which} offset must not be null"
                    )));
                }
                if units == FrameUnits::Range {
                    let negative = matches!(
                        crate::expr::binary(BinaryOp::Lt, v.clone(), Value::Int(0)),
                        Ok(Value::Bool(true))
                    );
                    if negative {
                        return Err(Error::invalid_window_size(
                            "invalid preceding or following size in window function",
                        ));
                    }
                    return Ok(Offset::Value(v));
                }
                match v {
                    Value::Int(k) if k >= 0 => Ok(Offset::Count(k as usize)),
                    _ => Err(Error::invalid_window_size(format!(
                        "frame {which} offset must not be negative"
                    ))),
                }
            };
            Ok(match b {
                WinBound::UnboundedPreceding => Bound::UnboundedPreceding,
                WinBound::Preceding(x) => Bound::Preceding(offset(x)?),
                WinBound::CurrentRow => Bound::CurrentRow,
                WinBound::Following(x) => Bound::Following(offset(x)?),
                WinBound::UnboundedFollowing => Bound::UnboundedFollowing,
            })
        };
        Ok(Frame {
            units,
            start: bound(&f.frame.start, true)?,
            end: bound(&f.frame.end, false)?,
            exclude: f.frame.exclude,
        })
    }

    /// Row `i`'s frame: its rows as ranges, in order (more than one when
    /// `EXCLUDE` cuts it).
    fn ranges(&self, i: usize, s: &Shape) -> Result<Ranges> {
        let a = self.index(&self.start, i, s, true)?;
        let b = self.index(&self.end, i, s, false)?.max(a);
        let g = s.group[i];
        let (ps, pe) = (s.starts[g], s.group_end(g));
        let cut = |out: &mut Ranges, x: usize, y: usize| {
            if x < y {
                out.r[out.len] = (x, y);
                out.len += 1;
            }
        };
        let mut out = Ranges::default();
        match self.exclude {
            FrameExclude::NoOthers => cut(&mut out, a, b),
            FrameExclude::CurrentRow => {
                cut(&mut out, a, b.min(i));
                cut(&mut out, a.max(i + 1), b);
            }
            FrameExclude::Group => {
                cut(&mut out, a, b.min(ps));
                cut(&mut out, a.max(pe), b);
            }
            FrameExclude::Ties => {
                cut(&mut out, a, b.min(ps));
                cut(&mut out, a.max(i), b.min(i + 1));
                cut(&mut out, a.max(pe), b);
            }
        }
        Ok(out)
    }

    /// A bound's row: the frame's first (`start`) or the one after its
    /// last.
    fn index(&self, b: &Bound, i: usize, s: &Shape, start: bool) -> Result<usize> {
        let n = s.n;
        let g = s.group[i];
        let groups = s.starts.len();
        let group_start = |g: usize| s.starts.get(g).copied().unwrap_or(n);
        Ok(match (b, self.units) {
            (Bound::UnboundedPreceding, _) => 0,
            (Bound::UnboundedFollowing, _) => n,
            (Bound::CurrentRow, FrameUnits::Rows) => {
                if start {
                    i
                } else {
                    i + 1
                }
            }
            (Bound::CurrentRow, _) => {
                if start {
                    s.starts[g]
                } else {
                    s.group_end(g)
                }
            }
            (Bound::Preceding(Offset::Count(k)), FrameUnits::Rows) => {
                if start {
                    i.saturating_sub(*k)
                } else if *k > i {
                    0
                } else {
                    i - k + 1
                }
            }
            (Bound::Following(Offset::Count(k)), FrameUnits::Rows) => {
                let j = i.saturating_add(*k);
                if start {
                    j.min(n)
                } else {
                    j.saturating_add(1).min(n)
                }
            }
            (Bound::Preceding(Offset::Count(k)), FrameUnits::Groups) => {
                if start {
                    s.starts[g.saturating_sub(*k)]
                } else if *k > g {
                    0
                } else {
                    s.group_end(g - k)
                }
            }
            (Bound::Following(Offset::Count(k)), FrameUnits::Groups) => {
                let h = g.saturating_add(*k);
                if start {
                    group_start(h)
                } else {
                    s.group_end(h.min(groups - 1))
                }
            }
            (Bound::Preceding(Offset::Value(d)) | Bound::Following(Offset::Value(d)), _) => {
                let preceding = matches!(b, Bound::Preceding(_));
                self.range_index(d, preceding, i, s, start)?
            }
            (Bound::Preceding(_) | Bound::Following(_), _) => {
                return Err(Error::internal("a frame offset of the wrong kind"));
            }
        })
    }

    /// A `RANGE` offset's row: by the ordering value `d` before or after
    /// row `i`'s (its peers alone when it is `NULL`).
    fn range_index(
        &self,
        d: &Value,
        preceding: bool,
        i: usize,
        s: &Shape,
        start: bool,
    ) -> Result<usize> {
        let (desc, nulls_first) = s.key_flags.unwrap_or((false, false));
        let Some(col) = s.key else {
            return Err(Error::internal("a RANGE offset without an ordering"));
        };
        let key = s.rows[i][col].clone();
        if key.is_null() {
            let g = s.group[i];
            return Ok(if start { s.starts[g] } else { s.group_end(g) });
        }
        // Toward the partition's start is down for an ascending order.
        let down = preceding != desc;
        let op = if down { BinaryOp::Sub } else { BinaryOp::Add };
        let bound = crate::expr::binary(op, key, d.clone())?;
        let flags = [(desc, nulls_first)];
        let cmp = |j: usize| {
            crate::spill::compare(
                &flags,
                std::slice::from_ref(&s.rows[j][col]),
                std::slice::from_ref(&bound),
            )
        };
        // The first row past the bound (the rows are in order): a binary
        // search.
        let past = |j: usize| match cmp(j) {
            Ordering::Less => false,
            Ordering::Equal => start,
            Ordering::Greater => true,
        };
        let (mut lo, mut hi) = (0, s.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if past(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(lo)
    }
}
