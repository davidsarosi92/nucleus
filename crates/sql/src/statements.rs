//! Statement statistics (PLAN.md §M14.9), for `nc_catalog.statements`:
//! per role and normalized statement — its constants replaced by `$n`,
//! so `WHERE id = 1` and `WHERE id = 2` count as one — how often it ran,
//! how long it took, the rows it read and returned, and how often it
//! failed.  `nc_statements_reset()` starts over.
//!
//! The statistics live in the process, by database, like the session list
//! ([`crate::sessions`]): a restart starts them over.  At most [`MAX`]
//! statements are kept; past that the least called makes room.  Each keeps
//! the text of its last run (constants and all) for the index advisor
//! to plan again; it is never shown.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nucleus_kv::Kv;

use crate::error::{Error, Result};
use crate::ext::{ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature};
use crate::lexer::{Token, tokenize_spans};
use crate::types::{DataType, Value};

/// The most statements kept per database.
pub const MAX: usize = 5000;

/// One normalized statement's statistics, for one role.
#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    /// The role that ran it (`current_user`).
    pub user: String,
    /// Its text, constants replaced by `$n` (as first seen).
    pub query: String,
    /// Runs.
    pub calls: u64,
    /// Runs that failed.
    pub errors: u64,
    /// Time spent, over all runs.
    pub total: Duration,
    /// The shortest run.
    pub min: Duration,
    /// The longest run.
    pub max: Duration,
    /// Rows read (scanned), over all runs.
    pub rows_read: u64,
    /// Rows returned or changed, over all runs.
    pub rows_returned: u64,
    /// When it last ran (µs since 2000-01-01 UTC).
    pub last: i64,
    /// The text of its last run, for the index advisor; never shown.
    pub(crate) sample: String,
}

/// One run of a statement.
pub struct Run<'a> {
    /// Its text.
    pub text: &'a str,
    /// The role that ran it.
    pub user: &'a str,
    /// How long it took.
    pub elapsed: Duration,
    /// Rows it read.
    pub rows_read: u64,
    /// Rows it returned or changed.
    pub rows_returned: u64,
    /// It went through.
    pub ok: bool,
    /// When (µs since 2000-01-01 UTC).
    pub at: i64,
}

type Table = HashMap<(String, String), Stat>;

static STATS: Mutex<BTreeMap<usize, Table>> = Mutex::new(BTreeMap::new());

fn database(kv: &Arc<dyn Kv>) -> usize {
    Arc::as_ptr(kv) as *const () as usize
}

/// `text` normalized: the key two runs share when they differ only in
/// constants, case and spacing, and the text shown — the constants
/// replaced by `$n` (numbered after the statement's own parameters),
/// runs of white space by one space.  Text that does not lex is kept as
/// it is.
pub fn normalize(text: &str) -> (String, String) {
    let Ok(tokens) = tokenize_spans(text) else {
        let t = text.trim().to_string();
        return (t.clone(), t);
    };
    let mut n = tokens
        .iter()
        .map(|(t, _)| match t {
            Token::Param(n) => *n,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let mut key = String::new();
    let mut shown = String::new();
    let mut from = 0;
    for (t, span) in &tokens {
        let literal = matches!(t, Token::Str(_) | Token::Number(_));
        // The text before the token: its spacing (and comments) as one space.
        if span.start > from && !shown.is_empty() {
            shown.push(' ');
        }
        if !key.is_empty() {
            key.push(' ');
        }
        if literal {
            n += 1;
            shown.push_str(&format!("${n}"));
            key.push_str(&format!("${n}"));
        } else {
            shown.push_str(&text[span.clone()]);
            match t {
                Token::Word(w) => key.push_str(w),
                Token::Quoted(q) => key.push_str(&format!("\"{}\"", q.replace('"', "\"\""))),
                Token::Param(p) => key.push_str(&format!("${p}")),
                Token::Op(o) => key.push_str(o),
                Token::UserOp(o) => key.push_str(o),
                Token::Str(_) | Token::Number(_) => {}
            }
        }
        from = span.end;
    }
    (key, shown)
}

/// Records one run in `kv`'s statistics.
pub fn record(kv: &Arc<dyn Kv>, run: Run) {
    let (key, shown) = normalize(run.text);
    let Ok(mut all) = STATS.lock() else {
        return;
    };
    let table = all.entry(database(kv)).or_default();
    let id = (run.user.to_string(), key);
    if !table.contains_key(&id) && table.len() >= MAX {
        // The least called (the oldest of those) makes room.
        if let Some(evict) = table
            .iter()
            .min_by_key(|(_, s)| (s.calls, s.last))
            .map(|(k, _)| k.clone())
        {
            table.remove(&evict);
        }
    }
    let s = table.entry(id).or_insert_with(|| Stat {
        user: run.user.to_string(),
        query: shown,
        calls: 0,
        errors: 0,
        total: Duration::ZERO,
        min: Duration::MAX,
        max: Duration::ZERO,
        rows_read: 0,
        rows_returned: 0,
        last: run.at,
        sample: String::new(),
    });
    s.calls += 1;
    s.errors += u64::from(!run.ok);
    s.total += run.elapsed;
    s.min = s.min.min(run.elapsed);
    s.max = s.max.max(run.elapsed);
    s.rows_read += run.rows_read;
    s.rows_returned += run.rows_returned;
    s.last = run.at;
    if run.ok {
        s.sample = run.text.to_string();
    }
}

/// `kv`'s statistics: every role's for an administrator (`user` `None`),
/// else `user`'s.
pub fn all(kv: &Arc<dyn Kv>, user: Option<&str>) -> Vec<Stat> {
    let Ok(all) = STATS.lock() else {
        return Vec::new();
    };
    let mut out: Vec<Stat> = all
        .get(&database(kv))
        .into_iter()
        .flat_map(|t| t.values())
        .filter(|s| user.is_none_or(|u| s.user == u))
        .cloned()
        .collect();
    out.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.query.cmp(&b.query)));
    out
}

/// Forgets `kv`'s statistics: every role's (`user` `None`), or `user`'s.
pub fn reset(kv: &Arc<dyn Kv>, user: Option<&str>) {
    if let Ok(mut all) = STATS.lock()
        && let Some(t) = all.get_mut(&database(kv))
    {
        t.retain(|(u, _), _| user.is_some_and(|me| u != me));
    }
}

/// The rows a statement's output returned or changed: a result's rows,
/// or the count its command tag ends with.
pub fn rows_of(out: &crate::session::Output) -> u64 {
    match out {
        crate::session::Output::Rows { rows, .. } => rows.len() as u64,
        crate::session::Output::Command(tag) => tag
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0),
    }
}

/// `nc_statements_reset()`: an administrator forgets every role's
/// statistics, anyone else their own.
struct Reset;

impl ScalarFunction for Reset {
    fn name(&self) -> &str {
        "nc_statements_reset"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        0..=0
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(vec![], Returns::Type(Some(DataType::Bool))))
    }

    fn call(&self, _: &CallContext<'_>, _: &[Value]) -> Result<Value> {
        let kv = crate::datetime::current_kv()
            .ok_or_else(|| Error::internal("nc_statements_reset() outside a statement"))?;
        let who = crate::access::current();
        reset(&kv, (!who.superuser).then_some(who.user.as_str()));
        Ok(Value::Bool(true))
    }
}

/// Registers `nc_statements_reset()`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["nc_statements_reset"], Arc::new(Reset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_become_parameters() {
        let (k1, s1) = normalize("SELECT *\n  FROM t WHERE id = 1 AND name = 'x'");
        let (k2, s2) = normalize("select * from t where id = 42 and name = 'it''s'");
        assert_eq!(k1, k2);
        assert_eq!(s1, "SELECT * FROM t WHERE id = $1 AND name = $2");
        assert_eq!(s2, "select * from t where id = $1 and name = $2");
        // After the statement's own parameters; quoted names kept.
        let (_, s) = normalize("UPDATE \"T\" SET a = $1 WHERE b = 3.5e2 -- why\n");
        assert_eq!(s, "UPDATE \"T\" SET a = $1 WHERE b = $2");
        assert_ne!(
            normalize("SELECT a FROM t").0,
            normalize("SELECT b FROM t").0
        );
        // A typed literal and an E-string are constants too.
        assert_eq!(
            normalize("SELECT DATE '2026-01-01', E'a\\'b'").1,
            "SELECT DATE $1, $2"
        );
    }
}
