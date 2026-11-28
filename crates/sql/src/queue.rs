//! Task queues (PLAN.md §M16.5 S5): tasks taken under a lease, finished
//! in the transaction of their work, retried with backoff, and past
//! their attempts moved to a dead-letter stream.
//!
//! `CREATE QUEUE q (columns) [WITH (lease = '30 seconds', max_attempts =
//! 5, backoff = '1 second', max_backoff = '1 hour', dead_letter = name)]`
//! makes a table whose first columns are the queue's own — `id` (its key,
//! an identity), `priority` (lower first; 0 by default), `available_at`
//! (when it may be taken; a delay is a later one), `attempts` (how many
//! times it was taken), `taken_at` (while taken), `last_error` — then the
//! columns given; an index on `(priority, available_at, id)`; and a
//! stream (§M16.5 S2), `q_dead` by default, for its dead letters.
//! `INSERT INTO q (…)` adds tasks; `UPDATE` and `DELETE` change and cancel
//! them, as a table's rows.
//!
//! `TAKE [columns] FROM q [LIMIT n] [LEASE interval]` takes the next
//! available tasks — lower `priority` first, then earlier `available_at`
//! — and leases them: `attempts` counted, `taken_at` set, `available_at`
//! moved past the lease (SQS's visibility timeout), so an expired lease
//! makes the task available again with no sweeper.  Run alone it is a
//! short transaction of its own, retried on a conflict as every
//! statement.  It reads one priority at a time through the index (a skip
//! scan) and takes the head in order; run again after a conflict —
//! another taker took the same — it chooses at random among a few times
//! as many as it takes, twice as many after each conflict, so competing
//! takers part: under contention the order within a priority is roughly,
//! not strictly, first in, first out.  A task whose last attempt's lease
//! ran out goes to the dead letters instead.
//!
//! `FINISH TASK id OF q ATTEMPT n` deletes the task in the work's
//! transaction, if attempt `n` still holds it (`55000` otherwise); a
//! taker that lost its lease to another while working fails at its
//! commit (the row changed after its snapshot), so the work's effects are
//! committed once.  `FAIL TASK id OF q ATTEMPT n [ERROR text] [RETRY
//! AFTER interval]` makes it available again after the backoff — `backoff
//! · 2^(attempts − 1)`, at most `max_backoff` — or, past `max_attempts`,
//! moves it to the dead letters (`task_id`, `attempts`, `error`,
//! `priority` and its columns).

use std::sync::Arc;

use nucleus_kv::KvTxn;

use crate::ast::{Auto, ColumnSpec, SqlType, TableConstraint};
use crate::catalog::{self, QueueSpec, Table};
use crate::datetime::Interval;
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::session::{OutColumn, Output};
use crate::types::{DataType, Row, Value};

/// The queue's own columns, first in its table.
pub const OWN: [&str; 6] = [
    "id",
    "priority",
    "available_at",
    "attempts",
    "taken_at",
    "last_error",
];
/// The dead letters' own columns after the stream's `seq` and `time`.
pub const DEAD_OWN: [&str; 4] = ["task_id", "attempts", "error", "priority"];

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Whether `t` is a queue.
pub fn is_queue(t: &Table) -> bool {
    t.queue.is_some()
}

/// Queue `name`.
pub(crate) fn of(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    let t = catalog::table(txn, name)?;
    if !is_queue(&t) {
        return Err(Error::wrong_object_type(format!(
            "\"{name}\" is not a queue"
        )));
    }
    Ok(t)
}

fn seconds(s: i64) -> Interval {
    Interval {
        months: 0,
        days: 0,
        micros: s * 1_000_000,
    }
}

/// A queue's settings when none are given.
pub fn defaults() -> QueueSpec {
    QueueSpec {
        lease: seconds(30),
        max_attempts: 5,
        backoff: seconds(1),
        max_backoff: seconds(3600),
        dead: 0,
    }
}

/// An interval's length in µs (a day 24 hours, a month 30 days, as
/// PostgreSQL's `justify_*`), saturating.
fn micros(i: Interval) -> i64 {
    i64::from(i.months)
        .saturating_mul(30 * 86_400_000_000)
        .saturating_add(i64::from(i.days).saturating_mul(86_400_000_000))
        .saturating_add(i.micros)
}

fn positive(key: &str, v: &str) -> Result<Interval> {
    let i = crate::datetime::parse_interval(v)
        .ok()
        .filter(|&i| micros(i) > 0);
    i.ok_or_else(|| {
        Error::invalid_parameter(format!(
            "a queue's {key} is a positive interval ('30 seconds'), not \"{v}\""
        ))
    })
}

const KEYS: &str = "lease, max_attempts, backoff, max_backoff";

/// Sets `key` of `spec`; whether it is one of the settings.
fn set(spec: &mut QueueSpec, key: &str, v: &str) -> Result<bool> {
    match key {
        "lease" => spec.lease = positive(key, v)?,
        "backoff" => spec.backoff = positive(key, v)?,
        "max_backoff" => spec.max_backoff = positive(key, v)?,
        "max_attempts" => {
            spec.max_attempts = v.parse::<u32>().ok().filter(|&n| n > 0).ok_or_else(|| {
                Error::invalid_parameter(format!(
                    "a queue's max_attempts is a positive whole number, not \"{v}\""
                ))
            })?
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// `CREATE QUEUE name (columns, constraints) [WITH (…)]`, in `txn`.
pub(crate) fn create(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    columns: &[ColumnSpec],
    constraints: &[TableConstraint],
    with: &[(String, String)],
) -> Result<()> {
    let mut spec = defaults();
    let mut dead = format!("{name}_dead");
    let mut rest = Vec::new();
    for (k, v) in with {
        if set(&mut spec, k, v)? {
            continue;
        }
        match k.as_str() {
            "dead_letter" => dead = v.clone(),
            "engine" => rest.push((k.clone(), v.clone())),
            _ => {
                return Err(Error::invalid_parameter(format!(
                    "unrecognized queue option \"{k}\" ({KEYS}, dead_letter, engine)"
                )));
            }
        }
    }
    for c in columns {
        if OWN.contains(&c.name.as_str()) {
            return Err(Error::duplicate_column(format!(
                "column \"{}\" is a queue's own ({})",
                c.name,
                OWN.join(", ")
            )));
        }
        if c.primary_key {
            return Err(Error::feature(
                "a queue's key is id: its columns are not PRIMARY KEY",
            ));
        }
    }
    if constraints
        .iter()
        .any(|c| matches!(c, TableConstraint::PrimaryKey(_)))
    {
        return Err(Error::feature(
            "a queue's key is id: it has no other PRIMARY KEY",
        ));
    }
    if catalog::get_table(&*txn, &dead)?.is_some() {
        return Err(Error::duplicate_table(format!(
            "relation \"{dead}\" already exists (a queue's dead letters: WITH (dead_letter = …) names another)"
        )));
    }
    let builtin = |ty| SqlType::Builtin { schema: None, ty };
    let own = |name: &str, ty, not_null: bool, default: Option<&str>| ColumnSpec {
        name: name.into(),
        ty: builtin(ty),
        max_len: None,
        numeric: None,
        not_null,
        primary_key: false,
        unique: false,
        default: default.map(str::to_string),
        check: None,
        references: None,
        generated: None,
        auto: None,
    };
    let mut id = own("id", DataType::BigInt, true, None);
    id.primary_key = true;
    id.auto = Some(Auto::Identity {
        always: true,
        name: None,
        options: Vec::new(),
    });
    let mut specs = vec![
        id,
        own("priority", DataType::Integer, true, Some("0")),
        own("available_at", DataType::TimestampTz, true, Some("now()")),
        own("attempts", DataType::Integer, true, Some("0")),
        own("taken_at", DataType::TimestampTz, false, None),
        own("last_error", DataType::Text, false, None),
    ];
    specs.extend(columns.iter().cloned());
    let opts = crate::session::TableOptions::defaults()?.with(&rest)?;
    crate::session::create_table(txn, reg, name, &specs, constraints, &opts, None)?;
    run(
        txn,
        reg,
        &format!(
            "CREATE INDEX {} ON {} (priority, available_at, id)",
            quote(&format!("{name}_ready")),
            quote(name)
        ),
        &[],
    )?;
    // The dead letters: the task's own, then its columns' types alone.
    let mut dead_specs = vec![
        own("task_id", DataType::BigInt, true, None),
        own("attempts", DataType::Integer, true, None),
        own("error", DataType::Text, false, None),
        own("priority", DataType::Integer, true, None),
    ];
    for c in columns {
        dead_specs.push(ColumnSpec {
            name: c.name.clone(),
            ty: c.ty.clone(),
            max_len: c.max_len,
            numeric: c.numeric,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
            check: None,
            references: None,
            generated: None,
            auto: None,
        });
    }
    crate::stream::create(txn, reg, &dead, &dead_specs, &[], &rest)?;
    let owner = crate::access::current().user.clone();
    crate::privileges::on_create_table(txn, &dead, &owner)?;
    let mut t = catalog::table(&*txn, name)?;
    let mut d = catalog::table(&*txn, &dead)?;
    if let Some(s) = d.stream.as_mut() {
        s.queue = Some(t.id);
    }
    catalog::update_table(txn, &d)?;
    spec.dead = d.id;
    t.queue = Some(spec);
    catalog::update_table(txn, &t)
}

/// `ALTER QUEUE name SET (…) | RESET (…)`, in `txn`.
pub(crate) fn alter(
    txn: &mut dyn KvTxn,
    name: &str,
    settings: &[(String, String)],
    reset: bool,
) -> Result<()> {
    let mut t = of(&*txn, name)?;
    let mut spec = t.queue.clone().unwrap_or_else(defaults);
    let base = defaults();
    for (k, v) in settings {
        let known = if reset {
            match k.as_str() {
                "lease" => spec.lease = base.lease,
                "backoff" => spec.backoff = base.backoff,
                "max_backoff" => spec.max_backoff = base.max_backoff,
                "max_attempts" => spec.max_attempts = base.max_attempts,
                _ => {}
            }
            matches!(
                k.as_str(),
                "lease" | "backoff" | "max_backoff" | "max_attempts"
            )
        } else {
            set(&mut spec, k, v)?
        };
        if !known {
            return Err(Error::invalid_parameter(format!(
                "unrecognized queue option \"{k}\" ({KEYS}; dead_letter and engine are set when it is made)"
            )));
        }
    }
    t.queue = Some(spec);
    catalog::update_table(txn, &t)
}

/// Runs `sql` (one statement) in `txn`: its rows (a query's, or a
/// change's `RETURNING`).
fn run(txn: &mut dyn KvTxn, reg: &Registry, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    let stmt = crate::parser::parse(sql)?.remove(0);
    let mut sink = crate::session::Collect::default();
    let out = crate::session::execute_statement(txn, reg, &stmt, params, &mut sink)?;
    let mut rows = sink.rows;
    if let Output::Rows { rows: more, .. } = out {
        rows.extend(more);
    }
    Ok(rows)
}

/// The values of expressions (their texts, each cast to its type) with
/// the statement's parameters.
fn values(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    exprs: &[(Option<&str>, &str)],
    params: &[Value],
) -> Result<Vec<Value>> {
    let list: Vec<String> = exprs
        .iter()
        .map(|(e, ty)| format!("({})::{ty}", e.unwrap_or("NULL")))
        .collect();
    let mut rows = run(txn, reg, &format!("SELECT {}", list.join(", ")), params)?;
    Ok(rows.pop().unwrap_or_default())
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

/// The places of the columns `TAKE` returns after `id` and `attempt`.
fn picked(t: &Table, wanted: &[String]) -> Result<Vec<usize>> {
    if wanted.is_empty() {
        return Ok((OWN.len()..t.columns.len()).collect());
    }
    let mut out = Vec::new();
    for w in wanted {
        let i = t.column(w).ok_or_else(|| {
            Error::undefined_column(format!(
                "column \"{w}\" of queue \"{}\" does not exist",
                t.name
            ))
        })?;
        // (`id` comes first anyway.)
        if i != 0 && !out.contains(&i) {
            out.push(i);
        }
    }
    Ok(out)
}

/// The columns `TAKE` returns: `id`, `attempt`, then those asked for (by
/// default the queue's columns after its own).
pub(crate) fn take_columns(
    txn: &dyn KvTxn,
    name: &str,
    wanted: &[String],
) -> Result<Vec<OutColumn>> {
    let t = of(txn, name)?;
    let mut out = vec![
        OutColumn {
            name: "id".into(),
            ty: DataType::BigInt,
            not_null: true,
        },
        OutColumn {
            name: "attempt".into(),
            ty: DataType::BigInt,
            not_null: true,
        },
    ];
    for i in picked(&t, wanted)? {
        let c = &t.columns[i];
        out.push(OutColumn {
            name: c.name.clone(),
            ty: c.ty,
            not_null: !c.nullable,
        });
    }
    Ok(out)
}

/// The columns `FAIL TASK` returns.
pub(crate) fn fail_columns() -> Vec<OutColumn> {
    vec![
        OutColumn {
            name: "outcome".into(),
            ty: DataType::Text,
            not_null: true,
        },
        OutColumn {
            name: "available_at".into(),
            ty: DataType::TimestampTz,
            not_null: false,
        },
    ]
}

/// `n` of `among` at random, in their order (all when there are no
/// more).
fn choose(mut among: Vec<i64>, n: usize) -> Result<Vec<i64>> {
    if among.len() <= n {
        return Ok(among);
    }
    let mut keep = vec![false; among.len()];
    let mut left = n;
    while left > 0 {
        let mut b = [0u8; 8];
        crate::datetime::random_bytes(&mut b)?;
        let i = (u64::from_le_bytes(b) % among.len() as u64) as usize;
        if !keep[i] {
            keep[i] = true;
            left -= 1;
        }
    }
    let mut i = 0;
    among.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    Ok(among)
}

/// How many ready tasks a taker run again after its first conflict
/// chooses among, for each it takes (doubled after each conflict).
const WINDOW: i64 = 4;

thread_local! {
    /// How many times the statement running has been run before (a
    /// conflict each: [`set_retry`]).
    static RETRY: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Notes that the statement about to run is run for the `n`-th time
/// after a conflict (0: the first time) — the session's retry of a
/// statement on its own.
pub(crate) fn set_retry(n: u32) {
    RETRY.with(|r| r.set(n));
}

/// `TAKE [columns] FROM name [LIMIT n] [LEASE interval]`, in `txn`.
pub(crate) fn take(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    wanted: &[String],
    limit: Option<&str>,
    lease: Option<&str>,
    params: &[Value],
) -> Result<Output> {
    let t = of(&*txn, name)?;
    let spec = t.queue.clone().unwrap_or_else(defaults);
    let columns = take_columns(&*txn, name, wanted)?;
    let v = values(txn, reg, &[(limit, "bigint"), (lease, "interval")], params)?;
    let mut need = match v.first() {
        Some(Value::Int(n)) if *n >= 0 => *n,
        None | Some(Value::Null) => 1,
        _ => return Err(Error::invalid_parameter("TAKE's LIMIT is 0 or more")),
    };
    let lease = match v.get(1) {
        Some(Value::Interval(i)) if micros(*i) > 0 => *i,
        None | Some(Value::Null) => spec.lease,
        _ => {
            return Err(Error::invalid_parameter(
                "TAKE's LEASE is a positive interval",
            ));
        }
    };
    let q = quote(name);
    let list: String = picked(&t, wanted)?
        .iter()
        .map(|&i| format!(", {}", quote(&t.columns[i].name)))
        .collect();
    let lease_sql = format!(
        "UPDATE {q} SET attempts = attempts + 1, taken_at = now(), available_at = now() + $2 \
         WHERE id = $1 RETURNING id, attempts::bigint AS attempt{list}"
    );
    let mut rows = Vec::new();
    // The lowest priority not yet read (a skip scan: the next priority
    // the index has, then its ready tasks).
    let mut from = i64::from(i32::MIN);
    while need > 0 {
        let next = run(
            txn,
            reg,
            &format!(
                "SELECT priority FROM {q} WHERE priority >= $1 \
                 ORDER BY priority, available_at, id LIMIT 1"
            ),
            &[Value::Int(from)],
        )?;
        let Some(p) = int(next.first().and_then(|r| r.first())) else {
            break;
        };
        // The first time, the head in order; after a conflict — another
        // taker took the same — a few times as many, twice as many after
        // each, chosen at random, so competing takers part.
        let retry = RETRY.with(std::cell::Cell::get);
        let window = if retry == 0 {
            need
        } else {
            need.saturating_mul(WINDOW << (retry - 1).min(8))
                .min(10_000)
        };
        let ready = run(
            txn,
            reg,
            &format!(
                "SELECT id, attempts FROM {q} WHERE priority = $1 AND available_at <= now() \
                 ORDER BY priority, available_at, id LIMIT $2"
            ),
            &[Value::Int(p), Value::Int(window)],
        )?;
        let mut eligible = Vec::new();
        let mut gone = 0;
        for r in &ready {
            let (Some(id), Some(attempts)) = (int(r.first()), int(r.get(1))) else {
                continue;
            };
            if attempts >= i64::from(spec.max_attempts) {
                dead_letter(
                    txn,
                    reg,
                    &t,
                    &spec,
                    id,
                    "its lease ran out on its last attempt",
                )?;
                gone += 1;
            } else {
                eligible.push(id);
            }
        }
        for id in choose(eligible, need as usize)? {
            rows.extend(run(
                txn,
                reg,
                &lease_sql,
                &[Value::Int(id), Value::Interval(lease)],
            )?);
            need -= 1;
        }
        // A full window with dead letters in it: this priority again.
        if !(gone > 0 && ready.len() as i64 == window) {
            if p >= i64::from(i32::MAX) {
                break;
            }
            from = p + 1;
        }
    }
    Ok(Output::Rows { columns, rows })
}

/// Moves task `id` of queue `t` to its dead letters with `error`.  The
/// stream's append is the queue's own act (no privilege of the caller's
/// on the stream needed); the delete is the caller's.
fn dead_letter(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    t: &Table,
    spec: &QueueSpec,
    id: i64,
    error: &str,
) -> Result<()> {
    let dead = crate::changefeed::table_by_id(&*txn, spec.dead)?.ok_or_else(|| {
        Error::internal(format!("queue \"{}\" has no dead-letter stream", t.name))
    })?;
    // Its columns the dead letters have (one added to the queue since is
    // not kept).
    let cols: String = t.columns[OWN.len()..]
        .iter()
        .filter(|c| dead.column(&c.name).is_some())
        .map(|c| format!(", {}", quote(&c.name)))
        .collect();
    let sql = format!(
        "INSERT INTO {} (task_id, attempts, error, priority{cols}) \
         SELECT id, attempts, $2, priority{cols} FROM {} WHERE id = $1",
        quote(&dead.name),
        quote(&t.name)
    );
    let system = Arc::new(crate::access::Who::system());
    crate::access::with_who(&system, || {
        run(txn, reg, &sql, &[Value::Int(id), Value::Text(error.into())])
    })?;
    run(
        txn,
        reg,
        &format!("DELETE FROM {} WHERE id = $1", quote(&t.name)),
        &[Value::Int(id)],
    )?;
    Ok(())
}

/// The wait before a task failed on attempt `attempts` is retried.
fn backoff(spec: &QueueSpec, attempts: i64) -> Interval {
    let shift = (attempts - 1).clamp(0, 62) as u32;
    let base = micros(spec.backoff);
    let wait = base
        .checked_mul(1i64 << shift)
        .unwrap_or(i64::MAX)
        .min(micros(spec.max_backoff));
    Interval {
        months: 0,
        days: 0,
        micros: wait,
    }
}

/// `FINISH TASK id OF name ATTEMPT n` (`fail` false) or `FAIL TASK id OF
/// name ATTEMPT n [ERROR text] [RETRY AFTER interval]`, in `txn`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn end(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    fail: bool,
    name: &str,
    id: &str,
    attempt: &str,
    error: Option<&str>,
    retry_after: Option<&str>,
    params: &[Value],
) -> Result<Output> {
    let t = of(&*txn, name)?;
    let spec = t.queue.clone().unwrap_or_else(defaults);
    let v = values(
        txn,
        reg,
        &[
            (Some(id), "bigint"),
            (Some(attempt), "bigint"),
            (error, "text"),
            (retry_after, "interval"),
        ],
        params,
    )?;
    let (Some(id), Some(attempt)) = (int(v.first()), int(v.get(1))) else {
        return Err(Error::invalid_parameter(
            "a task's id and attempt are numbers",
        ));
    };
    let q = quote(name);
    let found = run(
        txn,
        reg,
        &format!("SELECT attempts::bigint, taken_at IS NOT NULL FROM {q} WHERE id = $1"),
        &[Value::Int(id)],
    )?;
    let held = found.first().map(|r| (int(r.first()), r.get(1)));
    match held {
        None => {
            return Err(Error::prerequisite(format!(
                "task {id} of queue \"{name}\" is not there: finished, cancelled or a dead letter"
            )));
        }
        Some((Some(n), Some(Value::Bool(true)))) if n == attempt => {}
        Some((Some(n), _)) if n > attempt => {
            return Err(Error::prerequisite(format!(
                "task {id} of queue \"{name}\" is no longer attempt {attempt}'s: its lease ran out and attempt {n} took it"
            )));
        }
        Some(_) => {
            return Err(Error::prerequisite(format!(
                "task {id} of queue \"{name}\" is not taken by attempt {attempt}"
            )));
        }
    }
    if !fail {
        run(
            txn,
            reg,
            &format!("DELETE FROM {q} WHERE id = $1"),
            &[Value::Int(id)],
        )?;
        return Ok(Output::Command("FINISH 1".into()));
    }
    let error = match v.get(2) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let columns = fail_columns();
    if attempt >= i64::from(spec.max_attempts) {
        let why = error.unwrap_or_else(|| format!("failed on attempt {attempt}"));
        dead_letter(txn, reg, &t, &spec, id, &why)?;
        return Ok(Output::Rows {
            columns,
            rows: vec![vec![Value::Text("dead".into()), Value::Null]],
        });
    }
    let wait = match v.get(3) {
        Some(Value::Interval(i)) if micros(*i) >= 0 => *i,
        None | Some(Value::Null) => backoff(&spec, attempt),
        _ => {
            return Err(Error::invalid_parameter(
                "RETRY AFTER is an interval, 0 or more",
            ));
        }
    };
    let rows = run(
        txn,
        reg,
        &format!(
            "UPDATE {q} SET available_at = now() + $2, taken_at = NULL, last_error = $3 \
             WHERE id = $1 RETURNING available_at"
        ),
        &[
            Value::Int(id),
            Value::Interval(wait),
            error.map_or(Value::Null, Value::Text),
        ],
    )?;
    let at = rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or(Value::Null);
    Ok(Output::Rows {
        columns,
        rows: vec![vec![Value::Text("retry".into()), at]],
    })
}

/// What dropping table `t` (in `txn`, with the tables `dropped`) does to
/// queues: a queue's dead letters go with it (their names returned, to
/// drop first); a queue's dead letters alone are refused (`2BP01`).
pub(crate) fn on_drop(txn: &mut dyn KvTxn, t: &Table, dropped: &[String]) -> Result<Vec<String>> {
    if let Some(q) = t.stream.as_ref().and_then(|s| s.queue)
        && let Some(queue) = crate::changefeed::table_by_id(&*txn, q)?
        && !dropped.contains(&queue.name)
    {
        return Err(Error::dependent_objects(format!(
            "cannot drop stream {} because queue {} keeps its dead letters in it (DROP QUEUE {} drops both)",
            t.name, queue.name, queue.name
        )));
    }
    if let Some(spec) = &t.queue
        && let Some(mut dead) = crate::changefeed::table_by_id(&*txn, spec.dead)?
        && !dropped.contains(&dead.name)
    {
        if let Some(s) = dead.stream.as_mut() {
            s.queue = None;
        }
        catalog::update_table(txn, &dead)?;
        return Ok(vec![dead.name]);
    }
    Ok(Vec::new())
}
