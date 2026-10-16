//! The transaction layer: multi-version concurrency control over the store.
//!
//! - [`Engine`] owns a [`nucleus_storage::Store`] and hands out
//!   [`Transaction`]s.
//! - A transaction reads a **snapshot**: every commit up to its start
//!   timestamp, nothing after.  It buffers its writes and applies them at
//!   commit as one atomic batch with a new commit timestamp.
//! - **Snapshot isolation** (PLAN.md D5): a commit fails with
//!   [`Error::Conflict`] if any key it writes has a version committed after
//!   the transaction started (first committer wins).  Write skew is
//!   possible; serializable isolation comes with §M14.
//! - Readers never block writers or each other; a commit takes one lock for
//!   its conflict check and apply, then waits for the WAL with the others
//!   (group commit).
//!
//! See [`mvcc`] for the on-disk layout of versions.

mod engine;
mod error;
pub mod mvcc;

pub use engine::{Engine, EngineOptions, Isolation, Scan, Timestamp, Transaction, TxnOptions};
pub use error::{Error, Result};
