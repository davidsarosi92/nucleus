use std::fmt;
use std::io;

use crate::page::PageId;

/// Errors of the storage layer.
#[derive(Debug)]
pub enum Error {
    /// The platform reported an I/O error.
    Io(io::Error),
    /// The file is not a nucleus database this version can open (wrong
    /// magic, format version or page size).
    NotADatabase(String),
    /// Stored data failed validation: a checksum mismatch, a page that is
    /// not where it claims to be, a broken free list.
    Corrupt {
        /// The page that failed.
        page: PageId,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// The caller named a page that does not exist or is reserved, or freed
    /// a page twice.
    InvalidPage(PageId),
    /// The caller tried to write a page of the durable state (one not
    /// allocated since the last sync).  Copy it instead.
    Immutable(PageId),
    /// Every buffer pool frame is pinned; no page can be brought in.
    PoolExhausted,
    /// The page is pinned and cannot be freed.
    Pinned(PageId),
    /// An earlier `sync` failed, so it is unknown what reached the disk;
    /// this handle refuses all further work.  Reopen the database.
    Poisoned,
}

/// The result type of the storage layer.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::NotADatabase(why) => write!(f, "not a nucleus database: {why}"),
            Error::Corrupt { page, reason } => write!(f, "page {} is corrupt: {reason}", page.0),
            Error::InvalidPage(page) => write!(f, "page {} does not exist or is reserved", page.0),
            Error::Immutable(page) => write!(f, "page {} belongs to the durable state", page.0),
            Error::PoolExhausted => f.write_str("every buffer pool frame is pinned"),
            Error::Pinned(page) => write!(f, "page {} is pinned", page.0),
            Error::Poisoned => f.write_str("an earlier sync failed; reopen the database"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
