//! Running a statement's work on several threads (PLAN.md §M14.20 V4):
//! what a worker thread needs of the statement — the transaction's reads,
//! shared ([`nucleus_kv::KvTxn::shared`]), and the statement's ambient
//! state (who runs it, its time and clock, its budget).

use std::ops::Bound;
use std::sync::Arc;

use nucleus_kv::{KvChunks, KvIter, KvRead, KvTxn, Timestamp};

use crate::access::Who;
use crate::budget::Budget;

/// A transaction's shared reads as a transaction a worker evaluates with:
/// reads as the transaction's, writes refused (a worker only reads).
pub struct SharedTxn<'a>(pub &'a dyn KvRead);

fn read_only() -> nucleus_kv::Error {
    nucleus_kv::Error::ReadOnly
}

impl KvTxn for SharedTxn<'_> {
    fn read_ts(&self) -> Timestamp {
        self.0.read_ts()
    }

    fn get(&self, key: &[u8]) -> nucleus_kv::Result<Option<Vec<u8>>> {
        self.0.get(key)
    }

    fn scan<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> nucleus_kv::Result<KvIter<'a>> {
        self.0.scan(start, end)
    }

    fn get_validated(&self, key: &[u8]) -> nucleus_kv::Result<Option<Vec<u8>>> {
        self.0.get_validated(key)
    }

    fn scan_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> nucleus_kv::Result<KvIter<'a>> {
        self.0.scan_validated(start, end)
    }

    fn scan_chunks<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> nucleus_kv::Result<Box<dyn KvChunks + 'a>> {
        self.0.scan_chunks(start, end)
    }

    fn scan_chunks_validated<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> nucleus_kv::Result<Box<dyn KvChunks + 'a>> {
        self.0.scan_chunks_validated(start, end)
    }

    fn put(&mut self, _: &[u8], _: &[u8]) -> nucleus_kv::Result<()> {
        Err(read_only())
    }

    fn delete(&mut self, _: &[u8]) -> nucleus_kv::Result<()> {
        Err(read_only())
    }

    fn commit(self: Box<Self>) -> nucleus_kv::Result<Timestamp> {
        Err(read_only())
    }

    fn rollback(self: Box<Self>) {}
}

/// The statement's thread-local state, carried to a worker.
#[derive(Clone)]
pub struct Ambient {
    who: Option<Arc<Who>>,
    now: Option<(i64, Arc<dyn nucleus_kv::Kv>)>,
    budget: Option<Budget>,
}

impl Ambient {
    /// The current thread's.
    pub fn capture() -> Self {
        Ambient {
            who: crate::access::current_who(),
            now: crate::datetime::current(),
            budget: crate::budget::current(),
        }
    }

    /// Runs `f` with it in place on this thread; returns what `f` returned
    /// and the rows it read (to be counted by the statement's thread,
    /// [`crate::budget::add_scanned`]).
    pub fn run<T>(&self, f: impl FnOnce() -> T) -> (T, u64) {
        let with_now = |f: &mut dyn FnMut() -> (T, u64)| match &self.now {
            Some((ts, kv)) => crate::datetime::with_now(*ts, kv, f),
            None => f(),
        };
        let mut f = Some(f);
        let mut inner = || {
            let f = f.take().expect("run once");
            match &self.budget {
                Some(b) => crate::budget::with_budget(b.clone(), || {
                    let out = f();
                    (out, crate::budget::scanned())
                }),
                None => (f(), 0),
            }
        };
        match &self.who {
            Some(w) => crate::access::with_who(w, || with_now(&mut inner)),
            None => with_now(&mut inner),
        }
    }
}
