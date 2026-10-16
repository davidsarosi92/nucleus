//! The SQL layer (PLAN.md §F1): types, encoding, catalog, parser, planner
//! and executor over the KV contract.

pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{DataType, Row, Value};
