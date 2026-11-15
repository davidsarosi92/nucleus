//! The storage layer's metrics (PLAN.md §M14.18): what it does that is
//! not counted elsewhere.  The page cache's counters and the pages in use
//! are read from the store when a snapshot is taken (`nucleus-kv`'s
//! collector), not counted twice.

use nucleus_platform::metrics::{Counter, global};

/// Bytes appended to the write-ahead logs.
pub static WAL_BYTES: Counter = Counter::new(
    "nucleus_wal_bytes_total",
    "Bytes appended to the write-ahead log.",
);

/// Syncs that made the write-ahead log durable.
pub static WAL_SYNCS: Counter = Counter::new(
    "nucleus_wal_syncs_total",
    "Syncs (fsync) that made the write-ahead log durable; a group commit is one.",
);

/// Checkpoints finished.
pub static CHECKPOINTS: Counter = Counter::new(
    "nucleus_checkpoints_total",
    "Checkpoints finished: the trees written to the data file, the log trimmed.",
);

/// Adds the layer's instruments to the process's registry (once is
/// enough; again is harmless).
pub fn register() {
    global().register(&[&WAL_BYTES, &WAL_SYNCS, &CHECKPOINTS]);
}
