//! An aggregate call's `FILTER (WHERE cond)` (SQL:2003, issue #16): the
//! call is bound as an aggregate of its own — the inner one wrapped — fed
//! `cond` before the inner's arguments, which passes on only the rows
//! `cond` holds for (true; not false, not `NULL`).
//!
//! As an aggregate it goes through every executor unchanged: a row at a
//! time, vectorized, on the workers (its partial states are the inner's),
//! in window frames, under `DISTINCT` (the condition one of the values
//! made distinct, the rows it fails left out after).  Its name is the
//! inner's with ` FILTER` after it, so nothing that knows an aggregate by
//! its name takes it for the unfiltered call.
//!
//! Rewriting the call to `agg(CASE WHEN cond THEN x END)` would hold only
//! for an aggregate that leaves out `NULL`s — not `array_agg` or
//! `json_agg`, which keep them.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{Accumulator, AggRef, Aggregate, ArgInfo, GroupAccumulator, Signature};
use crate::types::Value;
use crate::vector::Vector;

/// The suffix of a filtered aggregate's name.
pub(crate) const SUFFIX: &str = " FILTER";

struct Filtered {
    inner: AggRef,
    name: String,
}

/// `inner` fed only the rows its first argument (the condition) holds for.
pub(crate) fn filtered(inner: AggRef) -> AggRef {
    let name = format!("{}{SUFFIX}", inner.name());
    AggRef::new(Arc::new(Filtered { inner, name }))
}

/// Whether row `r` passes: the condition true, and the inner's first
/// argument not `NULL` unless it keeps them (rows the executor leaves out
/// for an aggregate that does not — this one keeps every row, to judge it
/// here).
fn passes(cond: &Vector, first: Option<&Vector>, keeps_nulls: bool, r: usize) -> bool {
    let holds = match cond {
        Vector::Bool(d, n) => d[r] && !n.as_ref().is_some_and(|n| n[r]),
        c => c.value(r) == Value::Bool(true),
    };
    holds && (keeps_nulls || first.is_none_or(|f| !f.is_null(r)))
}

impl Aggregate for Filtered {
    fn name(&self) -> &str {
        &self.name
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=usize::MAX
    }

    fn keeps_nulls(&self) -> bool {
        true
    }

    fn infallible(&self) -> bool {
        self.inner.infallible()
    }

    fn order_free(&self) -> bool {
        self.inner.order_free()
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Err(Error::internal(
            "a filtered aggregate is bound by its inner one",
        ))
    }

    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(One {
            inner: self.inner.start(),
            keeps_nulls: self.inner.keeps_nulls(),
        })
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        let inner = self.inner.grouped()?;
        Some(Box::new(Groups {
            inner,
            keeps_nulls: self.inner.keeps_nulls(),
            rows: Vec::new(),
            groups: Vec::new(),
        }))
    }
}

struct One {
    inner: Box<dyn Accumulator>,
    keeps_nulls: bool,
}

impl Accumulator for One {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        let passes = args[0] == Value::Bool(true)
            && (self.keeps_nulls || args.get(1).is_none_or(|a| !a.is_null()));
        if passes {
            self.inner.add(&args[1..])?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        self.inner.finish()
    }
}

struct Groups {
    inner: Box<dyn GroupAccumulator>,
    keeps_nulls: bool,
    /// The rows and groups passed on, reused batch to batch.
    rows: Vec<u32>,
    groups: Vec<u32>,
}

impl GroupAccumulator for Groups {
    fn resize(&mut self, groups: usize) {
        self.inner.resize(groups);
    }

    fn group_bytes(&self) -> usize {
        self.inner.group_bytes()
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        let (cond, rest) = args.split_first().expect("the condition");
        self.rows.clear();
        self.groups.clear();
        for (&r, &g) in rows.iter().zip(groups) {
            if passes(cond, rest.first().copied(), self.keeps_nulls, r as usize) {
                self.rows.push(r);
                self.groups.push(g);
            }
        }
        if self.rows.is_empty() {
            return Ok(());
        }
        self.inner.update(&self.rows, &self.groups, rest)
    }

    fn combinable(&self) -> bool {
        self.inner.combinable()
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        self.inner.partial(group)
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        self.inner.combine(group, partial)
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.inner.finish(group)
    }

    fn finish_keeps_state(&self) -> bool {
        self.inner.finish_keeps_state()
    }
}
