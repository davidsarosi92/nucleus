//! SQL errors, with PostgreSQL's SQLSTATE codes so clients understand them.

use std::fmt;

/// A SQL error: a SQLSTATE code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The five-character SQLSTATE.
    pub code: &'static str,
    /// The message.
    pub message: String,
}

/// The result type of the SQL layer.
pub type Result<T> = std::result::Result<T, Error>;

macro_rules! codes {
    ($($fn:ident => $code:literal),* $(,)?) => {
        impl Error {
            $(
                #[doc = concat!("An error with SQLSTATE ", $code, ".")]
                pub fn $fn(message: impl Into<String>) -> Self {
                    Self { code: $code, message: message.into() }
                }
            )*
        }
    };
}

codes! {
    syntax => "42601",
    undefined_table => "42P01",
    undefined_column => "42703",
    undefined_function => "42883",
    undefined_object => "42704",
    duplicate_table => "42P07",
    duplicate_column => "42701",
    duplicate_object => "42710",
    ambiguous_column => "42702",
    invalid_column_reference => "42P10",
    invalid_recursion => "42P19",
    duplicate_alias => "42712",
    grouping => "42803",
    type_mismatch => "42804",
    invalid_text => "22P02",
    datetime_format => "22007",
    datetime_overflow => "22008",
    out_of_range => "22003",
    division_by_zero => "22012",
    string_too_long => "22001",
    not_null_violation => "23502",
    unique_violation => "23505",
    check_violation => "23514",
    foreign_key_violation => "23503",
    invalid_foreign_key => "42830",
    dependent_objects => "2BP01",
    cardinality => "21000",
    serialization => "40001",
    in_failed_txn => "25P02",
    active_txn => "25001",
    no_active_txn => "25P01",
    read_only => "25006",
    feature => "0A000",
    insufficient_privilege => "42501",
    invalid_schema => "3F000",
    invalid_regex => "2201B",
    invalid_parameter => "22023",
    array_subscript => "2202E",
    object_in_use => "55006",
    invalid_grant => "0LP01",
    invalid_authorization => "28000",
    undefined_database => "3D000",
    snapshot_too_old => "72000",
    internal => "XX000",
    io => "58030",
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for Error {}

impl From<nucleus_kv::Error> for Error {
    fn from(e: nucleus_kv::Error) -> Self {
        match e {
            nucleus_kv::Error::Conflict { .. } => {
                Error::serialization("could not serialize access due to concurrent update")
            }
            nucleus_kv::Error::SnapshotTooOld => Error::snapshot_too_old("snapshot too old"),
            nucleus_kv::Error::ReadOnly => {
                Error::read_only("cannot execute this in a read-only transaction")
            }
            nucleus_kv::Error::KeyTooLarge(n) => {
                Error::out_of_range(format!("index row of {n} bytes is too large"))
            }
            nucleus_kv::Error::Storage(e) => Error::io(e.to_string()),
        }
    }
}
