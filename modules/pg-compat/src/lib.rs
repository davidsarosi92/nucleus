//! PostgreSQL compatibility for nucleus (PLAN.md principle 11, D14): a
//! module, built on nucleus's public API alone (PLAN.md §M14.19).
//!
//! - [`PgCompat`] — the SQL side: the `pg_catalog` and `information_schema`
//!   views and the `pg_*` functions PostgreSQL clients use, registered in
//!   a session's [`Registry`](nucleus_sql::ext::Registry).
//! - [`Pg`] — the front end: PostgreSQL's protocol, whose sessions have
//!   [`PgCompat`] ([`registry`]).
//!
//! ```no_run
//! # use std::sync::Arc;
//! let mut fronts = nucleus_server::FrontEnds::builtin();
//! fronts.add(Arc::new(nucleus_pg_compat::Pg)).unwrap();
//! ```

mod catalog;
mod protocol;

pub use catalog::{PgCompat, type_oid};
pub use protocol::{Pg, handle, registry};
