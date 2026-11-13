//! What the fuzz targets share (PLAN.md §M14.29 S2): a database in
//! memory that keeps real time, a client made of the fuzzer's bytes, and
//! a session to run statements in.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::real::RealPlatform;
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{Clock, FileSystem, Platform, Rng};
use nucleus_sql::ext::Registry;
use nucleus_sql::{Session, Value};

/// The simulation's file system (in memory) under the machine's clock,
/// so a statement's time limit holds.
pub struct Memory {
    sim: SimPlatform,
    real: RealPlatform,
}

impl Memory {
    /// An empty one.
    pub fn new() -> Self {
        Self {
            sim: SimPlatform::new(1),
            real: RealPlatform::new().expect("the machine's clock"),
        }
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for Memory {
    fn fs(&self) -> &dyn FileSystem {
        self.sim.fs()
    }
    fn clock(&self) -> &dyn Clock {
        self.real.clock()
    }
    fn rng(&self) -> &dyn Rng {
        self.sim.rng()
    }
}

/// A new database in memory, its statements limited to a fraction of a
/// second (a fuzzer's input may ask for a billion rows).
pub fn database() -> Arc<dyn Kv> {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(Memory::new()),
            Path::new("/db"),
            Options::default(),
        )
        .expect("a database in memory"),
    );
    let mut s = Session::new(kv.clone());
    s.execute("ALTER DATABASE nucleus SET statement_timeout = '250ms'")
        .expect("a time limit");
    kv
}

/// The registry with PostgreSQL's functions and types (`tsvector`,
/// `to_tsquery`, …) beside nucleus's own.
pub fn pg_registry() -> Arc<Registry> {
    static R: std::sync::OnceLock<Arc<Registry>> = std::sync::OnceLock::new();
    R.get_or_init(|| nucleus_pg_compat::registry(&Arc::new(Registry::builtin())))
        .clone()
}

/// A client that sends the fuzzer's bytes and then nothing; what the
/// server writes back is dropped.
pub struct Client {
    input: io::Cursor<Vec<u8>>,
}

impl Client {
    /// One that sends `input`.
    pub fn new(input: &[u8]) -> Self {
        Self {
            input: io::Cursor::new(input.to_vec()),
        }
    }
}

impl Read for Client {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Client {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs each statement of `sql` with `params`, ignoring what fails
/// (an error is an answer; only a panic, a hang or a crash is a finding).
pub fn run(s: &mut Session, sql: &str, params: &[Value]) {
    if let Ok(parsed) = s.parse(sql) {
        for (stmt, _) in parsed.iter() {
            let _ = s.run(stmt, params);
        }
    }
}

/// Splits `data` at its first `0xFF` byte (never in UTF-8): two inputs
/// from one.
pub fn split(data: &[u8]) -> (&[u8], &[u8]) {
    match data.iter().position(|&b| b == 0xFF) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[]),
    }
}
