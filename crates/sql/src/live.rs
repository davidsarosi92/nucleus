//! Live queries (PLAN.md §M16.6 S2): `SUBSCRIBE name AS query`,
//! `CHANGES name [WAIT ms]`, `UNSUBSCRIBE name | ALL`.
//!
//! A subscription keeps its query, its parameters, its result as a
//! multiset of rows and the snapshot it was read at, and a watch of the
//! tables the query read (`Kv::watch_writes`), with the catalog's
//! definitions.  `CHANGES` runs the query again at a new snapshot once
//! a commit the last run did not see wrote one of those ranges, and
//! answers the difference as rows of `(nc_ts, nc_diff, columns…)` — the
//! snapshot's commit, `+n` or `−n` copies of the row (an update is a −1
//! and a +1 at the same `nc_ts`); prior art: Materialize's `SUBSCRIBE`
//! (`mz_timestamp`, `mz_diff`), Convex's invalidation by read set.  No
//! transaction is held while it waits.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use nucleus_kv::{Kv, KvTxn, TxnOptions, WriteWatch};

use crate::ast::{Query, Wait};
use crate::binder::Binder;
use crate::error::{Error, Result};
use crate::expr::{Ctx, Expr};
use crate::ext::{Registry, Volatility};
use crate::optimizer::optimize;
use crate::plan::Plan;
use crate::session::{OutColumn, Output};
use crate::types::{DataType, Row, Value};

/// Subscriptions one session may hold.
pub const PER_SESSION: usize = 64;
/// Live watches the database may hold (every session's subscriptions).
pub const IN_DATABASE: usize = 10_000;
/// Rows a subscription's result may hold.
pub const ROWS: usize = 100_000;
/// The longest `WAIT`.
const MOST_WAIT: Duration = Duration::from_secs(3600);
/// A wait's slice: between two, a stopping server is noticed.
const SLICE: Duration = Duration::from_secs(1);

/// The functions whose answer changes with time, not with the data: a
/// live query re-run on commits would not follow them.
const TIMED: [&str; 7] = [
    "now",
    "current_date",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "statement_timestamp",
    "transaction_timestamp",
];

/// A session's subscriptions, by name.
#[derive(Default)]
pub struct Subscriptions {
    by_name: BTreeMap<String, Subscription>,
}

impl std::fmt::Debug for Subscriptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.by_name.keys()).finish()
    }
}

struct Subscription {
    query: Query,
    params: Vec<Value>,
    columns: Vec<OutColumn>,
    /// The result as of `seen`: each row, and how many times.
    rows: HashMap<Row, i64>,
    /// The snapshot the last run read.
    seen: nucleus_kv::Timestamp,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
    watch: WriteWatch,
}

/// One run of a live query.
struct Run {
    at: nucleus_kv::Timestamp,
    columns: Vec<OutColumn>,
    rows: Vec<Row>,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
}

/// What a run needs of the session: its database, its functions, and a
/// fresh budget for each run (the session's identity is set around).
pub(crate) struct Context<'a> {
    pub(crate) kv: &'a dyn Kv,
    pub(crate) reg: &'a Registry,
    pub(crate) budget: &'a dyn Fn() -> Result<crate::budget::Budget>,
}

/// The query run at a new snapshot, under a budget of its own: its rows,
/// columns, the snapshot, and the key ranges it read — each table its
/// checks looked at, whole, and the catalog's definitions.
fn run(cx: &Context<'_>, q: &Query, params: &[Value]) -> Result<Run> {
    let budget = (cx.budget)()?;
    crate::budget::with_budget(budget, || run_in(cx.kv, cx.reg, q, params))
}

fn run_in(kv: &dyn Kv, reg: &Registry, q: &Query, params: &[Value]) -> Result<Run> {
    let txn = kv.begin(TxnOptions::read_only())?;
    let at = txn.read_ts();
    let (r, tables) = crate::audit::noting(|| -> Result<(Vec<OutColumn>, Vec<Row>)> {
        let ro: &dyn KvTxn = &*txn;
        let bound = Binder::new(ro, reg).checked().query(q)?;
        refuse_timed(&bound.plan)?;
        let columns: Vec<OutColumn> = bound
            .scope
            .cols
            .into_iter()
            .map(|c| OutColumn {
                name: c.name,
                ty: c.ty.unwrap_or(DataType::Text),
                not_null: c.not_null,
            })
            .collect();
        let plan = optimize(bound.plan, ro);
        let mut ctx = Ctx::new(ro, reg, params);
        ctx.needed = crate::prune::needed(&plan);
        let mut rows = Vec::new();
        for row in crate::exec::build(&plan, &ctx, None)? {
            rows.push(row?);
            crate::budget::charge_rows(rows.len())?;
            if rows.len() > ROWS {
                return Err(Error::program_limit(format!(
                    "a live query's result may hold {ROWS} rows at most"
                )));
            }
        }
        Ok((columns, rows))
    });
    let (columns, rows) = r?;
    let ranges = ranges_of(&*txn, &tables)?;
    Ok(Run {
        at,
        columns,
        rows,
        ranges,
    })
}

/// The key ranges of `tables` (those that are tables: each whole, its
/// rows and indexes, under its 5-byte prefix; a partitioned one's
/// partitions too), and the catalog's.
fn ranges_of(txn: &dyn KvTxn, tables: &BTreeSet<String>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut ids = BTreeSet::new();
    for name in tables {
        if let Some(t) = crate::catalog::get_table(txn, name)? {
            ids.insert(t.id);
            if t.partition_by.is_some() {
                for p in crate::catalog::tables(txn)? {
                    if p.partition_of.as_ref().is_some_and(|(of, _)| of == &t.name) {
                        ids.insert(p.id);
                    }
                }
            }
        }
    }
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = ids
        .into_iter()
        .map(|id| {
            let start = crate::catalog::index_prefix_of(id, 0)[..5].to_vec();
            let end = match id.checked_add(1) {
                Some(n) => crate::catalog::index_prefix_of(n, 0)[..5].to_vec(),
                None => vec![start[0] + 1],
            };
            (start, end)
        })
        .collect();
    out.extend(
        crate::catalog_cache::WATCHED
            .iter()
            .map(|(s, e)| (s.to_vec(), e.to_vec())),
    );
    Ok(out)
}

/// Refused: a function whose answer changes with time or by itself
/// (`now()`, `random()`, `nextval`) — a re-run on commits would not
/// follow it.
fn refuse_timed(plan: &Plan) -> Result<()> {
    fn walk(plan: &Plan, bad: &mut Option<String>) {
        plan.for_each_expr(&mut |e| {
            e.walk(&mut |x| match x {
                Expr::Subquery(s) => walk(&s.plan, bad),
                Expr::Call(f, _)
                    if f.volatility() == Volatility::Volatile
                        || TIMED.contains(&f.name().to_ascii_lowercase().as_str()) =>
                {
                    bad.get_or_insert_with(|| f.name().to_string());
                }
                _ => {}
            })
        });
        for (c, _) in plan.child_levels() {
            walk(c, bad);
        }
    }
    let mut bad = None;
    walk(plan, &mut bad);
    match bad {
        Some(f) => Err(Error::feature(format!(
            "a live query cannot call {f}(): its answer changes without a commit"
        ))),
        None => Ok(()),
    }
}

/// `(nc_ts, nc_diff, columns…)`: what `SUBSCRIBE` and `CHANGES` answer
/// for a query of `columns`.
pub(crate) fn out_columns(columns: &[OutColumn]) -> Vec<OutColumn> {
    let mut out = vec![
        OutColumn {
            name: "nc_ts".into(),
            ty: DataType::BigInt,
            not_null: true,
        },
        OutColumn {
            name: "nc_diff".into(),
            ty: DataType::BigInt,
            not_null: true,
        },
    ];
    out.extend(columns.iter().cloned());
    out
}

fn ts_value(ts: nucleus_kv::Timestamp) -> Value {
    Value::Int(i64::try_from(u64::from(ts)).unwrap_or(i64::MAX))
}

fn counted(rows: Vec<Row>) -> HashMap<Row, i64> {
    let mut m = HashMap::new();
    for r in rows {
        *m.entry(r).or_insert(0) += 1;
    }
    m
}

/// What changed from `old` to `new`: each row and its count's change,
/// the rows gone first, each part in the rows' order.
fn diff(old: &HashMap<Row, i64>, new: &HashMap<Row, i64>) -> Vec<(Row, i64)> {
    let mut gone = Vec::new();
    let mut came = Vec::new();
    for (r, n) in old {
        let d = new.get(r).copied().unwrap_or(0) - n;
        if d < 0 {
            gone.push((r.clone(), d));
        } else if d > 0 {
            came.push((r.clone(), d));
        }
    }
    for (r, n) in new {
        if !old.contains_key(r) {
            came.push((r.clone(), *n));
        }
    }
    let key = |(r, _): &(Row, i64)| crate::encoding::encode_row(r);
    gone.sort_by_cached_key(key);
    came.sort_by_cached_key(key);
    gone.extend(came);
    gone
}

impl Subscriptions {
    /// `SUBSCRIBE name AS query`.
    pub(crate) fn subscribe(
        &mut self,
        cx: &Context<'_>,
        name: &str,
        query: &Query,
        params: &[Value],
    ) -> Result<Output> {
        if self.by_name.contains_key(name) {
            return Err(Error::duplicate_object(format!(
                "subscription \"{name}\" already exists"
            )));
        }
        if self.by_name.len() >= PER_SESSION {
            return Err(Error::program_limit(format!(
                "a session may hold {PER_SESSION} subscriptions at most"
            )));
        }
        if cx.kv.write_watches() >= IN_DATABASE {
            return Err(Error::configuration_limit(format!(
                "the database holds {IN_DATABASE} live queries already"
            )));
        }
        let kv = cx.kv;
        let r = run(cx, query, params)?;
        // Begun after the run: its mark starts at the newest commit,
        // past the run's snapshot if anything came since.
        let Some(watch) = kv.watch_writes(r.ranges.clone()) else {
            return Err(Error::feature("this database has no live queries"));
        };
        let columns = out_columns(&r.columns);
        let ts = ts_value(r.at);
        let rows: Vec<Row> = r
            .rows
            .iter()
            .map(|row| {
                let mut out = vec![ts.clone(), Value::Int(1)];
                out.extend(row.iter().cloned());
                out
            })
            .collect();
        self.by_name.insert(
            name.to_string(),
            Subscription {
                query: query.clone(),
                params: params.to_vec(),
                columns: r.columns,
                rows: counted(r.rows),
                seen: r.at,
                ranges: r.ranges,
                watch,
            },
        );
        Ok(Output::Rows { columns, rows })
    }

    /// `CHANGES name [WAIT ms]`.
    pub(crate) fn changes(
        &mut self,
        cx: &Context<'_>,
        name: &str,
        wait: Option<&Wait>,
        params: &[Value],
    ) -> Result<Output> {
        let wait = match wait {
            None => Duration::ZERO,
            Some(Wait::Ms(ms)) => Duration::from_millis(*ms),
            Some(Wait::Param(n)) => match params.get(n.wrapping_sub(1)) {
                Some(Value::Int(ms)) if *ms >= 0 => Duration::from_millis(*ms as u64),
                _ => {
                    return Err(Error::invalid_parameter(format!(
                        "WAIT ${n}: milliseconds, a whole number not below 0"
                    )));
                }
            },
        }
        .min(MOST_WAIT);
        let Some(sub) = self.by_name.get_mut(name) else {
            return Err(Error::undefined_object(format!(
                "subscription \"{name}\" does not exist"
            )));
        };
        let deadline = std::time::Instant::now() + wait;
        loop {
            if sub.watch.last() > sub.seen {
                let r = match run(cx, &sub.query, &sub.params) {
                    Ok(r) => r,
                    Err(e) => {
                        // Its tables gone or out of reach: it ends.
                        self.by_name.remove(name);
                        return Err(e);
                    }
                };
                if r.columns != sub.columns {
                    self.by_name.remove(name);
                    return Err(Error::prerequisite(format!(
                        "subscription \"{name}\" ended: its query's columns changed"
                    )));
                }
                if r.ranges != sub.ranges {
                    // The tables it reads changed (a view's definition):
                    // watched from the run on.
                    let Some(w) = cx.kv.watch_writes(r.ranges.clone()) else {
                        return Err(Error::feature("this database has no live queries"));
                    };
                    sub.watch = w;
                    sub.ranges = r.ranges;
                }
                let new = counted(r.rows);
                let changed = diff(&sub.rows, &new);
                sub.rows = new;
                sub.seen = r.at;
                if !changed.is_empty() {
                    let ts = ts_value(r.at);
                    let rows = changed
                        .into_iter()
                        .map(|(row, d)| {
                            let mut out = vec![ts.clone(), Value::Int(d)];
                            out.extend(row);
                            out
                        })
                        .collect();
                    return Ok(Output::Rows {
                        columns: out_columns(&sub.columns),
                        rows,
                    });
                }
                continue;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(Output::Rows {
                    columns: out_columns(&sub.columns),
                    rows: Vec::new(),
                });
            }
            crate::budget::refuse_if_stopping()?;
            sub.watch.wait(sub.seen, (deadline - now).min(SLICE));
        }
    }

    /// `UNSUBSCRIBE name` (`Some`) or `UNSUBSCRIBE ALL`.
    pub(crate) fn unsubscribe(&mut self, name: Option<&str>) -> Result<Output> {
        match name {
            None => self.by_name.clear(),
            Some(n) => {
                if self.by_name.remove(n).is_none() {
                    return Err(Error::undefined_object(format!(
                        "subscription \"{n}\" does not exist"
                    )));
                }
            }
        }
        Ok(Output::Command("UNSUBSCRIBE".into()))
    }
}
