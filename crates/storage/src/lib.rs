//! Storage layer: pager, buffer pool, B+tree and write-ahead log.
//!
//! So far: the on-disk page format ([`page`]) and the [`pager`] (PLAN.md
//! §M1).  Reaches the outside world only through `nucleus-platform`.

pub mod crc32c;
mod error;
pub mod page;
pub mod pager;

pub use error::{Error, Result};
pub use page::{PAGE_SIZE, PAYLOAD_SIZE, Page, PageId, PageType};
pub use pager::Pager;
