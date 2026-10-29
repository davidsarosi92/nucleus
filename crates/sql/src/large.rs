//! Large values stored out of line (PLAN.md §M14.26), so a column can hold
//! files and model artefacts without every write of its row rewriting
//! them.
//!
//! A value whose row encoding is longer than [`THRESHOLD`] bytes is
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
use crate::encoding::{OUT_OF_LINE, ROW_VERSION, Reader, decode_value, encode_value, out_of_line};
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
    let pk = pk_suffix(t, key);
    let stored = match old {
        Some(_) => match txn.get(key)? {
            Some(b) => out_of_line(&b)?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(3 + row.len() * 9);
    out.push(ROW_VERSION);
    out.extend_from_slice(&(row.len() as u16).to_le_bytes());
    for (c, v) in row.iter().enumerate() {
        let mut enc = Vec::new();
        encode_value(&mut enc, v);
        let was = stored.iter().any(|&(sc, _)| sc == c);
        if enc.len() <= THRESHOLD {
            if was {
                remove_chunks(txn, t, &pk, c)?;
            }
            out.extend_from_slice(&enc);
            continue;
        }
        let same = was
            && old.and_then(|o| o.get(c)).is_some_and(|o| {
                let mut before = Vec::new();
                encode_value(&mut before, o);
                before == enc
            });
        if !same {
            if was {
                remove_chunks(txn, t, &pk, c)?;
            }
            let prefix = chunks_prefix(t, &pk, c);
            for (n, chunk) in enc.chunks(CHUNK).enumerate() {
                let mut k = prefix.clone();
                k.extend_from_slice(&(n as u32).to_be_bytes());
                txn.put(&k, chunk)?;
            }
        }
        out.push(OUT_OF_LINE);
        out.extend_from_slice(&(enc.len() as u32).to_le_bytes());
    }
    for &(c, _) in &stored {
        if c >= row.len() {
            remove_chunks(txn, t, &pk, c)?;
        }
    }
    txn.put(key, &out)?;
    Ok(())
}

/// Deletes the row at `key` and its values stored out of line.
pub fn remove(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
    if let Some(b) = txn.get(key)? {
        let pk = pk_suffix(t, key);
        for (c, _) in out_of_line(&b)? {
            remove_chunks(txn, t, &pk, c)?;
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
        fetch(txn, t, &pk_suffix(t, key), c, len)
    })?;
    for (v, c) in row.iter_mut().zip(&t.columns) {
        if matches!(c.ty, DataType::Enum(_)) {
            labels.fill(txn, c.ty, v)?;
        }
    }
    Ok(row)
}
