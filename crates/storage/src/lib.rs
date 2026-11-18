//! Storage layer: pager, buffer pool, B+tree and write-ahead log.
//!
//! So far: the on-disk page format ([`page`]), the [`pager`] (PLAN.md §M1)
//! the [`buffer`] pool (§M2) and the copy-on-write [`btree`]
//! (§M3), the [`wal`] and the crash-safe [`store`] (§M4).  Reaches the outside world only through `nucleus-platform`.

pub mod btree;
pub mod buffer;
pub mod cipher;
pub mod crc32c;
pub mod engines;
mod error;
pub mod lsm;
pub mod lz;
pub mod metrics;
pub mod node;
pub mod page;
pub mod pager;
pub mod scan;
pub mod sealed;
pub mod store;
pub mod wal;

pub use btree::BTree;
pub use buffer::{BufferPool, PageRef};
pub use cipher::PageCipher;
pub use engines::{EngineKind, Route, Tables};
pub use error::{Error, Result};
pub use page::{PAGE_SIZE, PAYLOAD_SIZE, Page, PageId, PageType};
pub use pager::{Pager, Rekey};
pub use store::{
    Batch, BranchInfo, Checkpoint, DEFERRED_MAX, Deferred, MAIN, MAIN_NAME, Recovery, Scrubbed,
    Store, StoreOptions, valid_branch_name,
};
pub use wal::{Lsn, Wal};
