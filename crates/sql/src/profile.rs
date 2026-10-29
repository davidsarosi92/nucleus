//! `EXPLAIN ANALYZE` (PLAN.md §M14.9): a plan run with every node
//! measured — the rows it produced, how often it was started (a nested
//! loop's inner side, a correlated subquery), and the time spent in it,
//! its inputs included — shown beside the optimizer's estimates.  A large
//! gap between the two points at stale or missing statistics.
//!
//! Time is the database's monotonic clock ([`Kv::elapsed`], principle 4);
//! `TIMING OFF` leaves it out (and makes the output repeatable).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use nucleus_kv::{Kv, KvTxn};

use crate::error::Result;
use crate::exec::Rows;
use crate::plan::Plan;
use crate::stats::Stats;
use crate::types::Row;

/// What one plan node did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeStats {
    /// Rows produced, over all its starts.
    pub rows: u64,
    /// Times it was started.
    pub loops: u64,
    /// Time spent in it, its inputs included.
    pub time: Duration,
}

/// The measurements of one run, by plan node (its address).
pub struct Profile {
    clock: Option<Arc<dyn Kv>>,
    nodes: RefCell<HashMap<usize, NodeStats>>,
}

fn key(plan: &Plan) -> usize {
    plan as *const Plan as usize
}

impl Profile {
    /// A profile; with a clock, nodes are timed by it.
    pub fn new(clock: Option<Arc<dyn Kv>>) -> Self {
        Self {
            clock,
            nodes: RefCell::new(HashMap::new()),
        }
    }

    fn now(&self) -> Option<Duration> {
        self.clock.as_ref().map(|k| k.elapsed())
    }

    fn add(&self, plan: usize, start: Option<Duration>, rows: u64, loops: u64) {
        let spent = match (start, self.now()) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => Duration::ZERO,
        };
        let mut nodes = self.nodes.borrow_mut();
        let n = nodes.entry(plan).or_default();
        n.rows += rows;
        n.loops += loops;
        n.time += spent;
    }

    /// Adds to `plan`'s measurements: what an operator that runs several
    /// plan nodes at once (a vectorized pipeline) reports for the inner
    /// ones.
    pub fn record(&self, plan: &Plan, time: Duration, rows: u64, loops: u64) {
        let mut nodes = self.nodes.borrow_mut();
        let n = nodes.entry(key(plan)).or_default();
        n.rows += rows;
        n.loops += loops;
        n.time += time;
    }

    /// The profile's clock (`None`: untimed).
    pub fn clock(&self) -> Option<Duration> {
        self.now()
    }

    /// What `plan` did (`None`: it never ran).
    pub fn of(&self, plan: &Plan) -> Option<NodeStats> {
        self.nodes.borrow().get(&key(plan)).copied()
    }

    /// Starts `plan` (through `build`), measured.
    pub fn measure<'a>(
        &'a self,
        plan: &'a Plan,
        build: impl FnOnce() -> Result<Rows<'a>>,
    ) -> Result<Rows<'a>> {
        let start = self.now();
        let rows = build();
        self.add(key(plan), start, 0, 1);
        Ok(Box::new(Measured {
            rows: rows?,
            plan: key(plan),
            profile: self,
        }))
    }
}

struct Measured<'a> {
    rows: Rows<'a>,
    plan: usize,
    profile: &'a Profile,
}

impl Iterator for Measured<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.profile.now();
        let r = self.rows.next();
        let produced = u64::from(matches!(r, Some(Ok(_))));
        self.profile.add(self.plan, start, produced, 0);
        r
    }
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

/// The `EXPLAIN ANALYZE` text of `plan` after a run measured by `profile`
/// that returned `rows` rows in `total` (timed: `Some`).
pub fn explain(
    txn: &dyn KvTxn,
    plan: &Plan,
    profile: &Profile,
    rows: usize,
    total: Option<Duration>,
) -> Vec<String> {
    let st = Stats::new(txn);
    let timed = profile.clock.is_some();
    let mut out = plan.explain_annotated(&|node| {
        let mut s = format!(
            " (rows={:.0})",
            crate::optimizer::estimate(node, &st).max(1.0)
        );
        match profile.of(node) {
            None => s.push_str(" (never executed)"),
            Some(n) => {
                let _ = write!(s, " (actual rows={} loops={}", n.rows, n.loops);
                if timed {
                    let _ = write!(s, " time={}", ms(n.time));
                }
                s.push(')');
            }
        }
        s
    });
    let mut last = format!("Execution: {rows} rows");
    if let Some(t) = total {
        let _ = write!(last, " in {}", ms(t));
    }
    out.push(last);
    out
}
