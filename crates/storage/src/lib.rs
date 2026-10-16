//! Storage layer: pager, buffer pool, B+tree and write-ahead log.
//!
//! So far: the on-disk page format ([`page`]), the [`pager`] (PLAN.md §M1)
//! the [`buffer`] pool (§M2) and the copy-on-write [`btree`] (§M3).  Reaches the outside world only through `nucleus-platform`.

pub mod btree;
pub mod buffer;
pub mod crc32c;
mod error;
pub mod node;
pub mod page;
pub mod pager;

pub use btree::BTree;
pub use buffer::{BufferPool, PageRef};
pub use error::{Error, Result};
pub use page::{PAGE_SIZE, PAYLOAD_SIZE, Page, PageId, PageType};
pub use pager::Pager;
