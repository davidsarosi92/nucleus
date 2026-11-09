//! Large values stored out of line (PLAN.md §M14.26), so a column can hold
//! files and model artefacts without every write of its row rewriting
//! them.
//!
//! A value whose row encoding is longer than [`THRESHOLD`] bytes — of a
//! type not [stored with its row](crate::ext::Type::stored_with_row) — is
//! stored in chunks of up to [`CHUNK`] bytes under the table's reserved
//! index id [`LARGE_INDEX`]: `index_prefix(LARGE_INDEX)`, the row's
//! primary key suffix, the column (`u16` big-endian) and the chunk number
//! (`u32` big-endian).  The row keeps tag 16 and the encoding's length.
//! The chunks hold the value's own row encoding, so reading one back is
//! decoding it (a registered type's upgrade included).
//!
//! The chunks are keyed by the row, so they need no identifiers of their
//! own: they are written and removed with the row in its transaction, a
//! snapshot sees the chunks of the row version it sees, and dropping the
//! table removes them with its other keys.  An update that leaves a large
//! value as it was keeps its chunks.

use std::ops::Bound;

use nucleus_kv::{KvTxn, prefix_end};

use crate::catalog::{self, Table};
use crate::encoding::{Reader, RowWriter, decode_value, encode_row_value, out_of_line};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::exec::pk_suffix;
use crate::types::{DataType, Row, Value};

/// A value's encoding longer than this is stored out of line.
pub const THRESHOLD: usize = 2000;
/// The largest chunk.
pub const CHUNK: usize = 32 * 1024;
/// The index id the chunks are stored under.
pub const LARGE_INDEX: u32 = u32::MAX;

/// Whether `v`, its row encoding `len` bytes, is stored out of line.
fn out_of_line_value(v: &Value, len: usize) -> bool {
    len > THRESHOLD && !matches!(v, Value::Ext(t, _) if t.imp().stored_with_row())
}

/// The prefix of the chunks of row `pk`'s column `col`.
fn chunks_prefix(t: &Table, pk: &[u8], col: usize) -> Vec<u8> {
    let mut k = t.index_prefix(LARGE_INDEX);
    k.extend_from_slice(pk);
    k.extend_from_slice(&(col as u16).to_be_bytes());
    k
}

/// Removes the chunks of row `pk`'s column `col` stored out of line.
pub(crate) fn remove_chunks(txn: &mut dyn KvTxn, t: &Table, pk: &[u8], col: usize) -> Result<()> {
    let p = chunks_prefix(t, pk, col);
    let end = prefix_end(&p).expect("chunk prefix");
    catalog::delete_range(txn, &p, Some(&end))?;
    Ok(())
}

/// Stores `row` at `key`, its large values out of line.  `old` is the row
/// stored at `key` before, if any: its large values that `row` keeps keep
/// their chunks, the others' chunks are removed.
pub fn write(
    txn: &mut dyn KvTxn,
    t: &Table,
    key: &[u8],
    row: &Row,
    old: Option<&Row>,
) -> Result<()> {
    let out = encode(txn, t, key, row, old)?;
    txn.put(key, &out)?;
    Ok(())
}

/// Stores a new row at `key` — unless the key has a row already (`false`),
/// found by the same look at its versions that guards the write.
pub fn insert(txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<bool> {
    let out = encode(txn, t, key, row, None)?;
    Ok(txn.put_new(key, &out)?)
}

/// A new row's stored value when none of its values is long enough to go
/// out of line (`None` when one is): what [`insert`] stores, made without
/// the transaction — on any thread (a load's workers).
pub fn encode_inline(row: &Row) -> Option<Vec<u8>> {
    let mut w = RowWriter::new(row.len());
    for v in row {
        let at = w.value(v);
        if out_of_line_value(v, w.since(at).len()) {
            return None;
        }
    }
    Some(w.finish())
}

/// The row's stored value, its long values written out of line.
fn encode(
    txn: &mut dyn KvTxn,
    t: &Table,
    key: &[u8],
    row: &Row,
    old: Option<&Row>,
) -> Result<Vec<u8>> {
    let pk = pk_suffix(t, key);
    let stored = match old {
        Some(_) => match txn.get(key)? {
            Some(b) => out_of_line(&b)?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    let mut w = RowWriter::new(row.len());
    for (c, v) in row.iter().enumerate() {
        // Encoded in place; moved out when too long to stay.
        let at = w.value(v);
        let was = stored.iter().any(|&(sc, _)| sc == c);
        if !out_of_line_value(v, w.since(at).len()) {
            if was {
                remove_chunks(txn, t, pk, c)?;
            }
            continue;
        }
        let enc = w.take_last(at);
        let same = was
            && old.and_then(|o| o.get(c)).is_some_and(|o| {
                let mut before = Vec::new();
                encode_row_value(&mut before, o);
                before == enc
            });
        if !same {
            if was {
                remove_chunks(txn, t, pk, c)?;
            }
            let prefix = chunks_prefix(t, pk, c);
            for (n, chunk) in enc.chunks(CHUNK).enumerate() {
                let mut k = prefix.clone();
                k.extend_from_slice(&(n as u32).to_be_bytes());
                txn.put(&k, chunk)?;
            }
        }
        w.out_of_line(enc.len() as u32);
    }
    for &(c, _) in &stored {
        if c >= row.len() {
            remove_chunks(txn, t, pk, c)?;
        }
    }
    Ok(w.finish())
}

/// Deletes the row at `key`, whose values are `row`: when none is long
/// enough to have been stored out of line, without reading it again.
pub fn remove_row(txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<()> {
    let mut enc = Vec::new();
    let short = row.iter().all(|v| {
        enc.clear();
        encode_row_value(&mut enc, v);
        !out_of_line_value(v, enc.len())
    });
    if short {
        txn.delete(key)?;
        return Ok(());
    }
    remove(txn, t, key)
}

/// Deletes the row at `key` and its values stored out of line.
pub fn remove(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
    if let Some(b) = txn.get(key)? {
        let pk = pk_suffix(t, key);
        for (c, _) in out_of_line(&b)? {
            remove_chunks(txn, t, pk, c)?;
        }
    }
    txn.delete(key)?;
    Ok(())
}

/// The value of row `pk`'s column `col` stored out of line, `len` bytes of
/// encoding.
pub fn fetch(txn: &dyn KvTxn, t: &Table, pk: &[u8], col: usize, len: u32) -> Result<Value> {
    let p = chunks_prefix(t, pk, col);
    let end = prefix_end(&p).expect("chunk prefix");
    let mut enc = Vec::with_capacity(len as usize);
    for e in txn.scan(Bound::Included(&p), Bound::Excluded(&end))? {
        enc.extend_from_slice(&e?.1);
    }
    if enc.len() != len as usize {
        return Err(Error::internal(format!(
            "{}: a value stored out of line has {} of its {len} bytes",
            t.name,
            enc.len()
        )));
    }
    decode_value(&mut Reader::new(&enc))
}

/// Values stored out of line read in the order of their rows, for a scan
/// (§M14.12 V2): one scan of the chunks from the first row holding one to
/// the scan's end, walked beside the rows' own, in place of a scan of
/// each value's chunks — a seek a value (a 768-float embedding is one)
/// becomes a step.  Opened at the first value met; the rows must come in
/// key order, the columns of a row in theirs.
pub struct Chunks<'a> {
    txn: &'a dyn KvTxn,
    validated: bool,
    /// The chunks' key the scan ends before (`None`: the table's).
    end: Option<Vec<u8>>,
    entries: Option<crate::exec::Entries<'a>>,
}

impl<'a> Chunks<'a> {
    /// For a scan of `t`'s rows ending before row key `end` (`None`: the
    /// table's end).
    pub fn new(txn: &'a dyn KvTxn, t: &Table, validated: bool, end: Option<&[u8]>) -> Self {
        Self {
            txn,
            validated,
            end: Some(match end {
                // A row's key: the chunks of the rows before it.
                Some(e) if e.starts_with(&t.index_prefix(0)) => {
                    let mut k = t.index_prefix(LARGE_INDEX);
                    k.extend_from_slice(pk_suffix(t, e));
                    k
                }
                // Past the rows (the primary index's end).
                _ => prefix_end(&t.index_prefix(LARGE_INDEX)).expect("chunk prefix"),
            }),
            entries: None,
        }
    }

    /// The value of row `pk`'s column `col`, `len` bytes of encoding: as
    /// [`fetch`].  Its chunks are read until they hold `len` bytes, so
    /// nothing past them is read; a value of one chunk (a vector's) is
    /// decoded from the chunk as read.
    pub fn fetch(&mut self, t: &Table, pk: &[u8], col: usize, len: u32) -> Result<Value> {
        let p = chunks_prefix(t, pk, col);
        if self.entries.is_none() {
            let end = self.end.take().expect("opened once");
            self.entries = Some(crate::exec::Entries::over(
                self.txn,
                self.validated,
                Bound::Included(&p),
                Bound::Excluded(&end),
            )?);
        }
        let entries = self.entries.as_mut().expect("opened above");
        let len = len as usize;
        let mut enc = Vec::new();
        while enc.len() < len {
            let Some((k, v)) = entries.next()? else { break };
            if !k.starts_with(&p) {
                if k > p.as_slice() {
                    break; // past it: its chunks are missing
                }
                // Chunks of rows or columns not read, or of rows deleted.
                continue;
            }
            if enc.is_empty() && v.len() == len {
                return decode_value(&mut Reader::new(v));
            }
            enc.extend_from_slice(v);
        }
        if enc.len() != len {
            return Err(Error::internal(format!(
                "{}: a value stored out of line has {} of its {len} bytes",
                t.name,
                enc.len()
            )));
        }
        decode_value(&mut Reader::new(&enc))
    }
}

/// Decodes the row stored at `key`, reading its values stored out of line
/// (and its enums' labels).
pub fn read_row(txn: &dyn KvTxn, t: &Table, key: &[u8], bytes: &[u8]) -> Result<Row> {
    read_row_with(txn, t, key, bytes, &mut Labels::default())
}

/// [`read_row`], with the enums a scan has looked up so far.
pub fn read_row_with(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    bytes: &[u8],
    labels: &mut Labels,
) -> Result<Row> {
    read_some(txn, t, key, bytes, labels, None)
}

/// [`read_row_with`], only the columns `wanted` marks (the others NULL).
pub fn read_some(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    bytes: &[u8],
    labels: &mut Labels,
    wanted: Option<&[bool]>,
) -> Result<Row> {
    let mut row = crate::encoding::decode_some(bytes, t.columns.len(), wanted, &mut |c, len| {
        fetch(txn, t, pk_suffix(t, key), c, len)
    })?;
    for (v, c) in row.iter_mut().zip(&t.columns) {
        if matches!(c.ty, DataType::Enum(_)) {
            labels.fill(txn, c.ty, v)?;
        }
    }
    Ok(row)
}
