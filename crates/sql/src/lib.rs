//! The SQL layer (PLAN.md §F1): types, encoding, catalog, parser, binder,
//! optimizer and executor over the KV contract.
//!
//! [`Session`] is the entry point: `Session::new(kv).execute("SELECT 1")`.

pub mod access;
pub mod advisor;
pub mod ast;
pub mod audit;
pub mod binder;
pub mod budget;
pub mod builtin;
pub mod bytea;
pub mod catalog;
pub mod check;
pub mod copy;
pub mod datetime;
pub mod dbsettings;
pub mod decimal;
pub mod dml;
pub mod encoding;
pub mod enums;
pub mod error;
pub mod exec;
pub mod expr;
pub mod ext;
pub mod fk;
pub mod hint;
pub mod inet;
pub use nucleus_kernels as kernels;
pub mod large;
pub mod laws;
pub mod lexer;
pub mod lint;
pub mod ncatalog;
pub mod optimizer;
pub mod parser;

pub mod plan;
pub mod policies;
pub mod privileges;
pub mod profile;
pub mod prune;
pub mod roles;
pub mod rules;
pub mod session;
pub mod sessions;
pub mod spill;
pub mod statements;
pub mod stats;
pub mod tokens;
pub mod types;
pub mod uuid;
pub mod vector;

pub use ast::Statement;
pub use error::{Error, Result};
pub use parser::parse;
pub use session::{OutColumn, Output, Session, Sink, TxnStatus, command_tag};
pub use types::{DataType, Row, Value};
