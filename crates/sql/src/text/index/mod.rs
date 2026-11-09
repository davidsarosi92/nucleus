//! The text index (PLAN.md §M14.21 T4): `CREATE INDEX … USING text
//! (col) WITH (analyzer = 'english')` on a text column — an inverted index
//! of the column's terms by the analyzer named ([`crate::text`]; `simple`
//! by default), kept exact in every snapshot ([`store`]).
//!
//! The index decides what `col @@ q` means: the binder gives the operator
//! the index's analyzer ([`crate::index_method::IndexMethod::operand`]),
//! so a row matches alike whether the index finds it or the row is read;
//! and the planner asks the index for the rows of a filter's `col @@ q`
//! with `q` fixed for the scan ([`crate::plan::Access::Method`]).
//!
//! A write puts a delta row or a tombstone in the statement's transaction.
//! The upkeep — a seal of the delta into a segment once it holds
//! [`SEAL`] rows, merges after — runs as the HNSW index's does: after a
//! commit, by the session whose commit finds it due, in transactions of
//! its own.  A `NULL` is not indexed.

pub(crate) mod segment;
pub(crate) mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kv::{Kv, KvTxn};

use crate::catalog::{Index as IndexDef, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, RowRef};
use crate::types::{DataType, Row, Value};

use segment::Doc;
use store::{Keys, Store};

/// The method's name.
pub const NAME: &str = "text";

/// Delta rows past which a seal is due.  A search reads the delta from
/// the KV and analyzes nothing (its rows hold their terms): about a
/// microsecond a row against the seal's few milliseconds.
pub const SEAL: u64 = 1024;
/// Documents a build puts in a segment at most (merges then join them).
const BUILD_BATCH: usize = 100_000;

/// The text index method.
pub struct TextIndex;

/// Index `ix`'s analyzer, if it is a text index.
pub fn analyzer_of(ix: &IndexDef) -> Option<&str> {
    if ix.method.as_deref() != Some(NAME) {
        return None;
    }
    Some(
        ix.options
            .iter()
            .find(|(k, _)| k == "analyzer")
            .map_or(crate::text::DEFAULT, |(_, v)| v.as_str()),
    )
}

fn store(t: &Table, i: usize) -> Store {
    Store {
        keys: Keys::new(t.index_prefix(t.indexes[i].id)),
    }
}

/// A text's hash: what a tombstone names the version of a row by.
fn text_hash(s: &str) -> u64 {
    crate::object_cache::object_id(13, s.as_bytes())
}

/// The text of `row` the index holds (`None`: a `NULL`, not indexed).
fn text_of<'r>(t: &Table, i: usize, row: &'r Row) -> Result<Option<&'r str>> {
    match &row[t.indexes[i].columns[0]] {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        _ => Err(Error::internal("a text index of a column not text")),
    }
}

/// An index, where it is: its database, table and id.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Id {
    instance: u64,
    table: u32,
    index: u32,
}

/// Delta rows written by index since its upkeep last ran (this process's).
#[derive(Default)]
struct Pending {
    any: AtomicBool,
    by: Mutex<HashMap<Id, u64>>,
    /// Held while an index is sealed or merged: one at a time.
    busy: Mutex<()>,
}

fn pending() -> &'static Pending {
    static P: OnceLock<Pending> = OnceLock::new();
    P.get_or_init(Pending::default)
}

impl Pending {
    fn note(&self, id: Id) {
        *self
            .by
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id)
            .or_default() += 1;
        self.any.store(true, Relaxed);
    }
}

impl IndexMethod for TextIndex {
    fn name(&self) -> &str {
        NAME
    }

    fn define(&self, t: &Table, ix: &mut IndexDef) -> Result<()> {
        if ix.columns.len() != 1 {
            return Err(Error::feature("a text index has one column"));
        }
        let col = &t.columns[ix.columns[0]];
        if col.ty != DataType::Text {
            return Err(Error::feature(format!(
                "a text index needs a text column, not {}",
                col.ty.name()
            )));
        }
        if let Some(o) = &ix.opclasses[0] {
            return Err(Error::undefined_object(format!(
                "operator class \"{o}\" does not exist for access method \"text\""
            )));
        }
        let mut analyzer = crate::text::DEFAULT.to_string();
        for (k, v) in &ix.options {
            match k.as_str() {
                "analyzer" => analyzer = v.clone(),
                _ => {
                    return Err(Error::invalid_parameter(format!(
                        "unrecognized parameter \"{k}\""
                    )));
                }
            }
        }
        crate::text::named(&analyzer)?;
        ix.options = vec![("analyzer".into(), analyzer)];
        Ok(())
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        i: usize,
        old: Option<RowRef>,
        new: Option<RowRef>,
    ) -> Result<()> {
        let old = match old {
            Some((pk, row)) => text_of(t, i, row)?.map(|s| (pk, s)),
            None => None,
        };
        let new = match new {
            Some((pk, row)) => text_of(t, i, row)?.map(|s| (pk, s)),
            None => None,
        };
        if old.is_some() && old == new {
            return Ok(()); // the text and the row's key unchanged
        }
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let s = store(t, i);
        if let Some((pk, text)) = old {
            let doc = Doc::analyze(a.as_ref(), text);
            let key = s
                .keys
                .tombstone(pk, text_hash(text), crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_gone(&doc))?;
        }
        if let Some((pk, text)) = new {
            let doc = Doc::analyze(a.as_ref(), text);
            let key = s.keys.delta(pk, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_insert(text_hash(text), &doc))?;
        }
        pending().note(Id {
            instance: txn.instance(),
            table: t.id,
            index: t.indexes[i].id,
        });
        Ok(())
    }

    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let mut wanted = vec![false; t.columns.len()];
        wanted[t.indexes[i].columns[0]] = true;
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let s = store(t, i);
        let mut batch = Vec::new();
        let rows: Vec<(Vec<u8>, Row)> =
            crate::index_method::rows(&*txn, t, Some(&wanted))?.collect::<Result<_>>()?;
        s.append(txn, &[])?;
        for (pk, row) in rows {
            if let Some(text) = text_of(t, i, &row)? {
                batch.push((pk, text_hash(text), Doc::analyze(a.as_ref(), text)));
            }
            if batch.len() == BUILD_BATCH {
                s.append(txn, &std::mem::take(&mut batch))?;
            }
        }
        s.append(txn, &batch)?;
        while s.merge(txn)? {}
        Ok(())
    }

    fn check(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        rows: &[(Vec<u8>, Row)],
        problems: &mut Vec<String>,
    ) -> Result<()> {
        let name = &t.indexes[i].name;
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let view = store(t, i).view(txn)?;
        // The rows' versions, and the statistics their texts give.
        let mut want = Vec::new();
        let (mut docs, mut len) = (0u64, 0u64);
        let mut df: HashMap<String, u64> = HashMap::new();
        for (pk, row) in rows {
            if let Some(text) = text_of(t, i, row)? {
                want.push((pk.clone(), text_hash(text)));
                let d = Doc::analyze(a.as_ref(), text);
                docs += 1;
                len += u64::from(d.len);
                for (term, _) in d.terms {
                    *df.entry(term).or_default() += 1;
                }
            }
        }
        want.sort();
        let have = view.live();
        if have != want {
            let extra = have
                .iter()
                .filter(|x| want.binary_search(x).is_err())
                .count();
            let missing = want
                .iter()
                .filter(|x| have.binary_search(x).is_err())
                .count();
            problems.push(format!(
                "text index \"{name}\": {missing} rows missing, {extra} it should not have"
            ));
        }
        let terms: Vec<&str> = df.keys().map(String::as_str).collect();
        let st = view.stats(&terms);
        if (st.docs, st.len) != (docs, len) {
            problems.push(format!(
                "text index \"{name}\": counts {} documents of {} terms, the rows {docs} of {len}",
                st.docs, st.len
            ));
        }
        let wrong = terms
            .iter()
            .zip(&st.df)
            .filter(|(t, n)| df[**t] != **n)
            .count();
        if wrong > 0 {
            problems.push(format!(
                "text index \"{name}\": {wrong} terms' document counts differ from the rows'"
            ));
        }
        Ok(())
    }

    fn after_statement(&self, kv: &Arc<dyn Kv>, _wrote: bool) -> Result<()> {
        upkeep(kv, false)
    }

    fn operand(&self, ix: &IndexDef, op: &str) -> Option<Value> {
        (op == "@@").then(|| Value::Text(analyzer_of(ix).unwrap_or(crate::text::DEFAULT).into()))
    }

    fn serves(&self, _: &IndexDef, function: &str) -> bool {
        function == "@@"
    }

    fn rows_where(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        function: &str,
        args: &[Value],
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if function != "@@" {
            return Ok(None);
        }
        let q = match args.first() {
            Some(Value::Text(q)) => q,
            Some(Value::Null) => return Ok(Some(Vec::new())),
            _ => return Ok(None),
        };
        let a = crate::text::named(analyzer_of(&t.indexes[i]).unwrap_or(crate::text::DEFAULT))?;
        let q = crate::text::query::cached(q, &a)?;
        Ok(Some(store(t, i).view(txn)?.matching(&q)?))
    }
}

/// The statistics BM25 reads, of index `i` of `t`, for `terms`, as `txn`
/// sees its rows: documents, their total length, each term's documents.
pub fn stats(txn: &dyn KvTxn, t: &Table, i: usize, terms: &[&str]) -> Result<(u64, u64, Vec<u64>)> {
    let st = store(t, i).view(txn)?.stats(terms);
    Ok((st.docs, st.len, st.df))
}

/// Index `id`'s table and its position there, as `txn` sees the catalog.
fn find(txn: &dyn KvTxn, id: &Id) -> Result<Option<(Table, usize)>> {
    Ok(crate::catalog::tables(txn)?.into_iter().find_map(|t| {
        let i = t
            .indexes
            .iter()
            .position(|ix| ix.id == id.index && ix.method.as_deref() == Some(NAME))?;
        (t.id == id.table).then_some((t, i))
    }))
}

/// Seals each index whose delta holds [`SEAL`] rows (every index written,
/// with `all`) and merges what is due after.
fn upkeep(kv: &Arc<dyn Kv>, all: bool) -> Result<()> {
    let p = pending();
    if !p.any.load(Relaxed) {
        return Ok(());
    }
    let due: Vec<(Id, u64)> =
        p.by.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(k, n)| k.instance == kv.instance() && (all || **n >= SEAL))
            .map(|(k, n)| (k.clone(), *n))
            .collect();
    if due.is_empty() {
        return Ok(());
    }
    let _busy = if all {
        p.busy.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match p.busy.try_lock() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        }
    };
    for (id, n) in due {
        let mut queues = None;
        let found = crate::hnsw::retrying(kv, |txn| {
            let Some((t, i)) = find(&*txn, &id)? else {
                return Ok(false);
            };
            let s = store(&t, i);
            s.seal(txn)?;
            queues = Some([s.keys.range(b'd'), s.keys.range(b'x')]);
            Ok(true)
        })?;
        {
            let mut by = p.by.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = by.get_mut(&id) {
                *m = m.saturating_sub(n);
                if *m == 0 {
                    by.remove(&id);
                }
            }
            if by.is_empty() {
                p.any.store(false, Relaxed);
            }
        }
        if !found {
            continue;
        }
        // The moved keys' tombstones go now, or every search's scan of
        // the delta steps over them (RocksDB's queue pattern).
        for (start, end) in queues.iter().flatten() {
            kv.vacuum_range(start, end)?;
        }
        while crate::hnsw::retrying(kv, |txn| match find(&*txn, &id)? {
            Some((t, i)) => store(&t, i).merge(txn),
            None => Ok(false),
        })? {}
    }
    Ok(())
}

/// Every text index written since its upkeep last ran, sealed and merged
/// now whatever the sizes (tests; `VACUUM`).
pub fn upkeep_now(kv: &Arc<dyn Kv>) -> Result<()> {
    upkeep(kv, true)
}

/// Index `i` of `t` merged into one segment, its deleted rows dropped
/// (tests).
pub fn merge_all(kv: &Arc<dyn Kv>, t: &Table, i: usize) -> Result<()> {
    crate::hnsw::retrying(kv, |txn| store(t, i).merge_all(txn))
}

/// Segments of index `i` of `t` and rows waiting in its delta, as `txn`
/// sees them (tests).
pub fn shape(txn: &dyn KvTxn, t: &Table, i: usize) -> Result<(usize, usize)> {
    let v = store(t, i).view(txn)?;
    Ok((v.segs.len(), v.waiting()))
}

/// Registers the method (the built-in module does).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_index_method(Arc::new(TextIndex))
}
