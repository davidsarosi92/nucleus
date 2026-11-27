//! MongoDB compatibility for nucleus (PLAN.md §M16 Q5, principle 11): a
//! module, built on nucleus's public API alone (PLAN.md §M14.19).
//!
//! [`Mongo`] is a front end speaking MongoDB's wire protocol — `OP_MSG`,
//! and `OP_QUERY` for the first `hello` — so `mongosh` and MongoDB's
//! drivers reach nucleus's collections: login by SCRAM-SHA-256 against
//! the server's own verifiers, then `find` / `getMore`, `insert`,
//! `update`, `delete`, `count`, `findAndModify`, the counting `aggregate`
//! drivers send, and the first commands a shell sends.  Each request is
//! compiled by `nucleus_sql::ext::documents`, as `ncp`'s `O` message is: one
//! compiler, one planner, one transaction.  Documents travel as BSON and
//! are kept as JSON, BSON's other types in Extended JSON ([`bson`]).
//!
//! ```no_run
//! # use std::sync::Arc;
//! let mut fronts = nucleus_server::FrontEnds::builtin();
//! fronts.add(Arc::new(nucleus_mongo_compat::Mongo)).unwrap();
//! ```

#![allow(missing_docs)]

pub mod bson;
mod server;
mod wire;

use std::io;
use std::net::TcpStream;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_server::{Config, FrontEnd};

pub use server::{handle, handle_stream};

/// MongoDB's default port.
pub const DEFAULT_PORT: u16 = 27017;

/// The front end: MongoDB's wire protocol (`--mongo-compat`).
pub struct Mongo;

impl FrontEnd for Mongo {
    fn name(&self) -> &str {
        "mongo"
    }
    fn default_addr(&self) -> String {
        format!("127.0.0.1:{DEFAULT_PORT}")
    }
    fn handle(
        &self,
        stream: TcpStream,
        kv: Arc<dyn Kv>,
        config: &Config,
        id: u32,
    ) -> io::Result<()> {
        server::handle(stream, kv, config, id)
    }
}
