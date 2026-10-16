//! The SQL layer (PLAN.md §F1): types, encoding, catalog, parser, binder,
//! optimizer and executor over the KV contract.
//!
//! [`Session`] is the entry point: `Session::new(kv).execute("SELECT 1")`.

pub mod ast;
pub mod binder;
pub mod catalog;
pub mod dml;
pub mod encoding;
pub mod error;
pub mod exec;
pub mod expr;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod plan;
pub mod session;
pub mod slt;
pub mod stats;
pub mod types;

pub use error::{Error, Result};
pub use session::{OutColumn, Output, Session, TxnStatus};
pub use types::{DataType, Row, Value};
