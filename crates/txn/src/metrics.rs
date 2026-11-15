//! The transaction layer's metrics (PLAN.md §M14.18).

use nucleus_platform::metrics::{Counter, global};

/// Transactions committed.
pub static COMMITS: Counter = Counter::new(
    "nucleus_commits_total",
    "Transactions committed (read-only ones too).",
);

/// Transactions that ended without committing.
pub static ROLLBACKS: Counter = Counter::new(
    "nucleus_rollbacks_total",
    "Transactions that ended without committing: rolled back, dropped, or their commit failed.",
);

/// Serialization conflicts.
pub static CONFLICTS: Counter = Counter::new(
    "nucleus_conflicts_total",
    "Serialization conflicts (SQLSTATE 40001): a write or commit that met a concurrent one.",
);

/// Adds the layer's instruments (and the storage layer's) to the
/// process's registry.
pub fn register() {
    nucleus_storage::metrics::register();
    global().register(&[&COMMITS, &ROLLBACKS, &CONFLICTS]);
}
