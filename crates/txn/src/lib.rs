//! The transaction layer: multi-version concurrency control over the store.
//!
//! - [`Engine`] owns a [`nucleus_storage::Store`] and hands out
//!   [`Transaction`]s.
//! - A transaction reads a **snapshot**: every commit up to its start
//!   timestamp, nothing after.  It buffers its writes and applies them at
//!   commit as one atomic batch with a new commit timestamp.
//! - **Snapshot isolation** (PLAN.md D5): a commit fails with
//!   [`Error::Conflict`] if any key it writes has a version committed after
//!   the transaction started (first committer wins; reported already at the
//!   write when it is visible then).  Write skew is possible.
//! - **Serializable** (§M14, D12): additionally, the keys and key ranges the
//!   transaction read (a scan counts up to where it was consumed) must not
//!   have been written by a commit after its snapshot.  The engine keeps
//!   the keys of recent commits in memory for this, pruned below the oldest
//!   running transaction.  Read-only transactions never conflict.
//! - Readers never block writers or each other; a commit takes one lock for
//!   its conflict check and apply, then waits for the WAL with the others
//!   (group commit).
//!
//! See [`mvcc`] for the on-disk layout of versions.

mod engine;
mod error;
pub mod mvcc;

pub use engine::{
    BACKGROUND_CHUNK, Chunk, Engine, EngineOptions, Isolation, Scan, Timestamp, Transaction,
    TxnOptions,
};
pub use error::{Error, Result};
