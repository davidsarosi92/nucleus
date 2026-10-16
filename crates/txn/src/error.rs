use std::fmt;

/// Errors of the transaction layer.
#[derive(Debug)]
pub enum Error {
    /// The storage layer failed.
    Storage(nucleus_storage::Error),
    /// Another transaction committed a conflicting change first.  Retry the
    /// transaction from the start.
    Conflict {
        /// A key the conflict is about (written by both, or — under
        /// serializable isolation — read here and written there).
        key: Vec<u8>,
    },
    /// The requested snapshot is older than the versions kept.
    SnapshotTooOld,
    /// A write in a read-only transaction.
    ReadOnly,
    /// A key too long once encoded.
    KeyTooLarge(usize),
    /// Stored data that does not decode.
    Corrupt(&'static str),
}

impl Error {
    /// Whether retrying the whole transaction may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Conflict { .. })
    }
}

/// The result type of the transaction layer.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Storage(e) => write!(f, "{e}"),
            Error::Conflict { key } => {
                write!(f, "conflict with a concurrent transaction on key {key:?}")
            }
            Error::SnapshotTooOld => f.write_str("snapshot too old"),
            Error::ReadOnly => f.write_str("write in a read-only transaction"),
            Error::KeyTooLarge(n) => write!(f, "key of {n} bytes is too large"),
            Error::Corrupt(why) => write!(f, "corrupt version data: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(e) => Some(e),
            _ => None,
        }
    }
}

impl From<nucleus_storage::Error> for Error {
    fn from(e: nucleus_storage::Error) -> Self {
        match e {
            nucleus_storage::Error::KeyTooLarge(n) => Error::KeyTooLarge(n),
            e => Error::Storage(e),
        }
    }
}
