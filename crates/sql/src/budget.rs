//! A statement's budget (PLAN.md §M14.25): how long it may run, how many
//! rows it may read and return — a session's `statement_timeout`, the
//! database's, and a token's ceiling.  The session puts the budget in
//! place for the statement it runs ([`with_budget`]); scans charge it row
//! by row ([`charge_scan`]), so a statement over budget stops while it
//! reads, not after.

use std::cell::{Cell, RefCell};

use crate::error::{Error, Result};

/// What a statement may spend (`None`: no limit).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    /// When it must have ended (µs since 2000-01-01 UTC, by the database's
    /// clock).
    pub deadline: Option<i64>,
    /// The most rows it may read.
    pub max_scan: Option<u64>,
    /// The most rows it may return.
    pub max_rows: Option<u64>,
}

struct Spent {
    budget: Budget,
    scanned: Cell<u64>,
}

thread_local! {
    static SPENT: RefCell<Option<Spent>> = const { RefCell::new(None) };
}

/// Runs `f` within `budget`.
pub fn with_budget<T>(budget: Budget, f: impl FnOnce() -> T) -> T {
    let before = SPENT.with(|s| {
        s.replace(Some(Spent {
            budget,
            scanned: Cell::new(0),
        }))
    });
    let out = f();
    SPENT.with(|s| *s.borrow_mut() = before);
    out
}

/// `57014`: the statement ran out of time.
pub fn timed_out() -> Error {
    Error::query_canceled("canceling statement due to statement timeout")
}

/// One row read: over the scan budget is `54000`; past the deadline
/// (looked at every 64 rows) `57014`.
pub fn charge_scan() -> Result<()> {
    SPENT.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref() else {
            return Ok(());
        };
        let n = s.scanned.get() + 1;
        s.scanned.set(n);
        if s.budget.max_scan.is_some_and(|m| n > m) {
            return Err(Error::program_limit(format!(
                "the statement read more than {} rows, the session's budget",
                s.budget.max_scan.unwrap_or_default()
            )));
        }
        if n % 64 == 0 {
            check_time(&s.budget)?;
        }
        Ok(())
    })
}

/// The rows the current statement has read so far.
pub fn scanned() -> u64 {
    SPENT.with(|s| s.borrow().as_ref().map_or(0, |s| s.scanned.get()))
}

fn check_time(b: &Budget) -> Result<()> {
    if let Some(d) = b.deadline
        && crate::datetime::clock()? > d
    {
        return Err(timed_out());
    }
    Ok(())
}

/// The statement's result has `n` rows: over the row budget is `54000`;
/// past the deadline `57014`.
pub fn charge_rows(n: usize) -> Result<()> {
    SPENT.with(|s| {
        let s = s.borrow();
        let Some(s) = s.as_ref() else {
            return Ok(());
        };
        if s.budget.max_rows.is_some_and(|m| n as u64 > m) {
            return Err(Error::program_limit(format!(
                "the statement returns more than {} rows, the session's budget",
                s.budget.max_rows.unwrap_or_default()
            )));
        }
        check_time(&s.budget)
    })
}
