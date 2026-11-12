//! Chunk tables (PLAN.md §M14.22 E7): `CREATE TABLE c CHUNKS OF docs
//! (body) BY markdown SIZE 200 [OVERLAP 20] [USING m]` — a table of the
//! chunks of a column's texts, kept current with its table.
//!
//! The chunk table's columns: the documents' primary key (its columns'
//! names and types), `position`, `chunk`, `start`, `length`, `headings`,
//! `size` — what `chunks()` returns ([`crate::chunks`]); its primary key is
//! the documents' and `position`; a foreign key to the documents with
//! `ON DELETE CASCADE ON UPDATE CASCADE` is its lineage, so a document
//! deleted takes its chunks, a key changed moves them, and the foreign
//! keys' machinery does both.  Columns may be added — an embedding column
//! over `chunk` (§M14.22 E4) makes the chunks' vectors.
//!
//! A document written is chunked again in the writer's transaction
//! ([`rechunk`]: chunking is local, no call leaves the process) and its
//! chunks compared with those stored, by text: a chunk whose text is
//! unchanged keeps its row — and so its vector — its position and
//! offsets updated if they moved; the others are deleted and inserted.
//! An edit to one paragraph embeds the chunks it changed, not the
//! document.

use nucleus_kv::KvTxn;

use crate::catalog::{self, ChunkSpec, ForeignKey, Table};
use crate::chunks::{self, By};
use crate::dml::{self, Writer};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::types::{Row, Value};

/// The columns a chunk table derives, after the documents' key.
pub const COLUMNS: [&str; 6] = ["position", "chunk", "start", "length", "headings", "size"];

/// A chunk table of a table being written: its definition, writer and
/// lineage.
pub(crate) struct Chunked {
    writer: Box<Writer>,
    spec: ChunkSpec,
    fk: ForeignKey,
    /// The derived columns' positions, in [`COLUMNS`]' order.
    derived: [usize; 6],
}

/// The chunk tables of `t` (for its writer).
pub(crate) fn of(txn: &dyn KvTxn, reg: &Registry, t: &Table) -> Result<Vec<Chunked>> {
    let mut out = Vec::new();
    for (child, fk_name) in &t.referenced_by {
        if *child == t.name {
            continue;
        }
        let c = catalog::table(txn, child)?;
        let Some(spec) = c.chunks.clone().filter(|s| s.fk == *fk_name) else {
            continue;
        };
        let fk = c
            .foreign_keys
            .iter()
            .find(|f| f.name == *fk_name)
            .cloned()
            .ok_or_else(|| Error::internal("a chunk table without its foreign key"))?;
        let derived = derived(&c)?;
        out.push(Chunked {
            writer: Box::new(Writer::new(txn, reg, c)?),
            spec,
            fk,
            derived,
        });
    }
    Ok(out)
}

fn derived(t: &Table) -> Result<[usize; 6]> {
    let mut d = [0; 6];
    for (i, name) in COLUMNS.iter().enumerate() {
        d[i] = t
            .column(name)
            .ok_or_else(|| Error::internal(format!("chunk table without {name}")))?;
    }
    Ok(d)
}

/// Whether column `c` of chunk table `t` is one it derives (its
/// documents' key or a chunk's).
pub(crate) fn derives(t: &Table, c: usize) -> bool {
    let Some(spec) = &t.chunks else {
        return false;
    };
    COLUMNS.contains(&t.columns[c].name.as_str())
        || t.foreign_keys
            .iter()
            .any(|f| f.name == spec.fk && f.columns.contains(&c))
}

/// `42809` for a statement that would write a chunk table's rows.
pub(crate) fn refuse(t: &Table, what: &str) -> Result<()> {
    if t.chunks.is_some() {
        return Err(Error::wrong_object_type(format!(
            "cannot {what} chunk table \"{}\": its rows follow its documents' table",
            t.name
        )));
    }
    Ok(())
}

/// The chunks of `text` by `spec`, as rows of the derived columns.
fn derive(txn: &dyn KvTxn, spec: &ChunkSpec, text: Option<&str>) -> Result<Vec<[Value; 6]>> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    let by = By::parse(&spec.by)?;
    let (size, overlap) = (spec.size as usize, spec.overlap as usize);
    let cs = match &spec.model {
        None => chunks::chunk(text, by, size, overlap, &mut chunks::Words)?,
        Some(m) => chunks::chunk(
            text,
            by,
            size,
            overlap,
            &mut chunks::ModelTokens { txn, model: m },
        )?,
    };
    Ok(chunks::rows(text, cs))
}

fn text_of(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        _ => None,
    }
}

/// The document whose key is `key` (the values of the referenced columns)
/// now has `text`: its chunks made again and set beside those stored.
pub(crate) fn rechunk(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    c: &Chunked,
    key: &[Value],
    text: &Value,
) -> Result<()> {
    let t = &c.writer.table;
    let [pos, chunk, ..] = c.derived;
    let new = derive(&*txn, &c.spec, text_of(text))?;
    let old = crate::fk::referencing_rows(&*txn, reg, t, &c.fk, key)?;
    // Each new chunk takes a stored one of the same text — at its own
    // position first.
    let mut by_text: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for (j, (_, r)) in old.iter().enumerate() {
        by_text
            .entry(text_of(&r[chunk]).unwrap_or(""))
            .or_default()
            .push(j);
    }
    let mut taken = vec![false; old.len()];
    let mut matched: Vec<Option<usize>> = Vec::with_capacity(new.len());
    for n in &new {
        let m = by_text
            .get_mut(text_of(&n[1]).unwrap_or(""))
            .and_then(|js| {
                let k = js.iter().position(|&j| old[j].1[pos] == n[0]).unwrap_or(0);
                (!js.is_empty()).then(|| js.remove(k))
            });
        if let Some(j) = m {
            taken[j] = true;
        }
        matched.push(m);
    }
    // Gone: deleted (their jobs with them).
    let gone: Vec<(Vec<u8>, Row)> = old
        .iter()
        .zip(&taken)
        .filter(|(_, t)| !**t)
        .map(|(r, _)| r.clone())
        .collect();
    if !gone.is_empty() {
        dml::delete_rows(txn, reg, t, &gone)?;
    }
    // Kept: their derived values set; those that move go through
    // positions below zero first, so no two meet on the way.
    let set = |row: &Row, n: &[Value; 6], position: Value| -> Row {
        let mut r = row.clone();
        for (k, &col) in c.derived.iter().enumerate() {
            r[col] = if k == 0 {
                position.clone()
            } else {
                n[k].clone()
            };
        }
        r
    };
    let ctx = crate::expr::Ctx::new(&*txn, reg, &[]);
    let mut first = Vec::new();
    let mut second = Vec::new();
    for (n, m) in new.iter().zip(&matched) {
        let Some(j) = *m else { continue };
        let (key, row) = &old[j];
        if row[pos] == n[0] {
            let mut next = set(row, n, n[0].clone());
            if next != *row {
                c.writer.complete(&mut next, &ctx)?;
                second.push((key.clone(), row.clone(), next));
            }
        } else {
            let Value::Int(p) = n[0] else {
                return Err(Error::internal("a chunk's position"));
            };
            let mut aside = set(row, n, Value::Int(-p));
            c.writer.complete(&mut aside, &ctx)?;
            let aside_key = crate::exec::row_key_of(t, &aside, None);
            let mut next = set(row, n, n[0].clone());
            c.writer.complete(&mut next, &ctx)?;
            first.push((key.clone(), row.clone(), aside.clone()));
            second.push((aside_key, aside, next));
        }
    }
    drop(ctx);
    if !first.is_empty() {
        dml::update_rows(txn, reg, &c.writer, &first)?;
    }
    if !second.is_empty() {
        dml::update_rows(txn, reg, &c.writer, &second)?;
    }
    // New: inserted.
    let fresh: Vec<&[Value; 6]> = new
        .iter()
        .zip(&matched)
        .filter(|(_, m)| m.is_none())
        .map(|(n, _)| n)
        .collect();
    let rows = new_rows(&*txn, reg, c, key, &fresh)?;
    if !rows.is_empty() {
        dml::insert_rows(txn, &c.writer, rows)?;
    }
    Ok(())
}

/// Chunk rows of the document under `key`, with the other columns'
/// defaults (an embedding column's job is put when they are inserted).
fn new_rows(
    txn: &dyn KvTxn,
    reg: &Registry,
    c: &Chunked,
    key: &[Value],
    chunks: &[&[Value; 6]],
) -> Result<Vec<Row>> {
    let t = &c.writer.table;
    let ctx = crate::expr::Ctx::new(txn, reg, &[]);
    let mut rows = Vec::with_capacity(chunks.len());
    for n in chunks {
        let mut row: Row = vec![Value::Null; t.columns.len()];
        for (i, slot) in row.iter_mut().enumerate() {
            if let Some(k) = c.derived.iter().position(|&d| d == i) {
                *slot = n[k].clone();
            } else if let Some(k) = c.fk.columns.iter().position(|&f| f == i) {
                *slot = key[k].clone();
            } else if t.columns[i].default.is_some() {
                *slot = c.writer.default_of(i, &ctx)?;
            }
        }
        c.writer.complete(&mut row, &ctx)?;
        rows.push(row);
    }
    Ok(rows)
}

/// The documents' rows `rows` were written: their chunk tables' rows made
/// to follow — new rows (`old` `None`), or rows whose text changed.
pub(crate) fn written<'r>(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    chunked: &[Chunked],
    rows: impl Iterator<Item = (Option<&'r Row>, &'r Row)>,
) -> Result<()> {
    let rows: Vec<_> = rows.collect();
    for c in chunked {
        // New documents have no chunks yet (their key was no one's): all
        // their chunks inserted at once.
        let mut inserted = Vec::new();
        for &(old, new) in &rows {
            let key: Vec<Value> = c.fk.ref_columns.iter().map(|&k| new[k].clone()).collect();
            match old {
                None => {
                    let derived = derive(&*txn, &c.spec, text_of(&new[c.spec.column]))?;
                    let refs: Vec<&[Value; 6]> = derived.iter().collect();
                    inserted.extend(new_rows(&*txn, reg, c, &key, &refs)?);
                }
                // The text is the same: a key change cascaded already.
                Some(o) if o[c.spec.column] == new[c.spec.column] => {}
                Some(_) => rechunk(txn, reg, c, &key, &new[c.spec.column])?,
            }
        }
        if !inserted.is_empty() {
            dml::insert_rows(txn, &c.writer, inserted)?;
        }
    }
    Ok(())
}

/// `CREATE TABLE name CHUNKS OF source (column) BY by SIZE size [OVERLAP
/// overlap] [USING model]`: the table made and filled.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    name: &str,
    source: &str,
    column: &str,
    by: &str,
    size: u32,
    overlap: u32,
    model: Option<&str>,
) -> Result<()> {
    let src = catalog::table(&*txn, source)?;
    if src.partition_by.is_some() || src.partition_of.is_some() {
        return Err(Error::feature(
            "a chunk table of a partitioned table or a partition is not supported yet",
        ));
    }
    if src.chunks.is_some() {
        return Err(Error::feature(
            "a chunk table of a chunk table is not supported",
        ));
    }
    let col = src.column(column).ok_or_else(|| {
        Error::undefined_column(format!(
            "column \"{column}\" of relation \"{source}\" does not exist"
        ))
    })?;
    if src.columns[col].ty != crate::types::DataType::Text {
        return Err(Error::type_mismatch(format!(
            "column \"{column}\" is of type {}: chunks are cut from text",
            crate::ncatalog::type_text(&src.columns[col])
        )));
    }
    if src.primary_key.is_empty() {
        return Err(Error::invalid_object_definition(format!(
            "table \"{source}\" needs a primary key for a chunk table: a chunk names its document by it"
        )));
    }
    // The settings, checked as chunks() checks them.
    By::parse(by)?;
    chunks::chunk(
        "",
        By::parse(by)?,
        size as usize,
        overlap as usize,
        &mut chunks::Words,
    )?;
    if let Some(m) = model {
        crate::models::count_tokens(&*txn, m, &["a"])?;
    }
    // The table, as SQL: the documents' key, the chunk's columns.
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut defs = Vec::new();
    let mut keys = Vec::new();
    for &k in &src.primary_key {
        let c = &src.columns[k];
        if COLUMNS.contains(&c.name.as_str()) {
            return Err(Error::duplicate_column(format!(
                "the key column \"{}\" of \"{source}\" has the name of a chunk's column",
                c.name
            )));
        }
        defs.push(format!(
            "{} {} NOT NULL",
            q(&c.name),
            crate::ncatalog::type_text(c)
        ));
        keys.push(q(&c.name));
    }
    for (n, ty) in COLUMNS
        .iter()
        .zip(["integer", "text", "integer", "integer", "text[]", "integer"])
    {
        defs.push(format!("{} {ty} NOT NULL", q(n)));
    }
    let fk = format!("{name}_document_fkey");
    let key = keys.join(", ");
    let sql = format!(
        "CREATE TABLE {} ({}, PRIMARY KEY ({key}, position), CONSTRAINT {} FOREIGN KEY ({key}) REFERENCES {} ({}) ON DELETE CASCADE ON UPDATE CASCADE)",
        q(name),
        defs.join(", "),
        q(&fk),
        q(source),
        src.primary_key
            .iter()
            .map(|&k| q(&src.columns[k].name))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let crate::ast::Statement::CreateTable {
        columns,
        constraints,
        ..
    } = crate::parser::parse(&sql)?.remove(0)
    else {
        return Err(Error::internal("a chunk table's definition"));
    };
    crate::session::create_table(
        txn,
        reg,
        name,
        &columns,
        &constraints,
        crate::storage::ROW,
        None,
    )?;
    let mut t = catalog::table(&*txn, name)?;
    t.chunks = Some(ChunkSpec {
        fk,
        column: col,
        by: by.to_ascii_lowercase(),
        size,
        overlap,
        model: model.map(str::to_string),
    });
    catalog::update_table(txn, &t)?;
    // Every document's chunks.
    let src = catalog::table(&*txn, source)?;
    let chunked = of(&*txn, reg, &src)?;
    let docs: Vec<Row> = {
        let ctx = crate::expr::Ctx::new(&*txn, reg, &[]);
        crate::exec::matching(&src, &crate::plan::Access::Full, None, &ctx)?
            .map(|r| r.map(|(_, row)| row))
            .collect::<Result<_>>()?
    };
    let i = chunked
        .iter()
        .position(|c| c.writer.table.name == name)
        .ok_or_else(|| Error::internal("the chunk table just made"))?;
    written(txn, reg, &chunked[i..=i], docs.iter().map(|r| (None, r)))
}
