//! The catalog — table definitions — and the keyspace layout.  Both live in
//! the KV store (PLAN.md principle 8), so DDL is transactional like any
//! other write.
//!
//! **Keyspace:**
//!
//! | Key | Value |
//! |-----|-------|
//! | `00 01 <table name>` | the table definition |
//! | `00 02 <sequence name>` | a `u64` counter (`table_id`) |
//! | `00 03 <table id u32 BE>` | next hidden row id of a table without a primary key |
//! | `00 04 <table id u32 BE>` | table statistics (§M13) |
//! | `01 <table id u32 BE> <index id u32 BE> <key>` | table and index entries |
//!
//! Index 0 of a table is its primary index: key = the primary key columns
//! (or a hidden row id), value = the encoded row.  Secondary index `n`: key =
//! the index columns followed by the primary key, value empty — so entries
//! are unique even for duplicate index values, and the row is one more
//! lookup away.  A `UNIQUE` index checks for an existing entry with the same
//! non-NULL index columns before inserting.

use std::ops::Bound;

use nucleus_kv::{KvTxn, prefix_end};

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::types::{DataType, Value};

const CATALOG: &[u8] = &[0x00, 0x01];
const SEQUENCE: &[u8] = &[0x00, 0x02];
const ROWID: &[u8] = &[0x00, 0x03];
const STATS: &[u8] = &[0x00, 0x04];
const DATA: u8 = 0x01;
/// Version 2 added `NUMERIC(p, s)` modifiers; version 1 still reads.
const TABLE_VERSION: u8 = 2;

/// A column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// Name (lowercase unless quoted).
    pub name: String,
    /// Type.
    pub ty: DataType,
    /// Whether `NULL` is allowed.
    pub nullable: bool,
    /// The `DEFAULT` expression, as SQL text.
    pub default: Option<String>,
    /// `VARCHAR(n)`: the longest value allowed, in characters.
    pub max_len: Option<u32>,
    /// `NUMERIC(precision, scale)`.
    pub numeric: Option<(u32, u32)>,
}

/// An index.
#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    /// Id within its table (≥ 1; 0 is the primary index).
    pub id: u32,
    /// Name, unique in the database.
    pub name: String,
    /// The indexed columns, by position.
    pub columns: Vec<usize>,
    /// Whether two rows may not share non-NULL index values.
    pub unique: bool,
}

/// A table definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// Id, unique in the database, never reused.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Columns in order.
    pub columns: Vec<Column>,
    /// The primary key columns by position; empty: a hidden row id.
    pub primary_key: Vec<usize>,
    /// Secondary indexes.
    pub indexes: Vec<Index>,
    /// `CHECK` constraints: (name, SQL text of the expression).
    pub checks: Vec<(String, String)>,
    /// The id the next index gets.
    pub next_index_id: u32,
}

impl Table {
    /// The position of column `name`.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The key prefix of index `index_id` (0 = primary).
    pub fn index_prefix(&self, index_id: u32) -> Vec<u8> {
        let mut k = vec![DATA];
        k.extend_from_slice(&self.id.to_be_bytes());
        k.extend_from_slice(&index_id.to_be_bytes());
        k
    }

    /// The prefix of every key of the table.
    pub fn data_prefix(&self) -> Vec<u8> {
        let mut k = vec![DATA];
        k.extend_from_slice(&self.id.to_be_bytes());
        k
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(TABLE_VERSION)
            .u32(self.id)
            .str(&self.name)
            .u32(self.columns.len() as u32);
        for c in &self.columns {
            w.str(&c.name)
                .u8(type_tag(c.ty))
                .u8(u8::from(c.nullable))
                .opt_str(c.default.as_deref())
                .u32(c.max_len.map_or(0, |n| n + 1));
            let (p, s) = c.numeric.map_or((0, 0), |(p, s)| (p + 1, s));
            w.u32(p).u32(s);
        }
        w.u32(self.primary_key.len() as u32);
        for &p in &self.primary_key {
            w.u32(p as u32);
        }
        w.u32(self.indexes.len() as u32);
        for i in &self.indexes {
            w.u32(i.id)
                .str(&i.name)
                .u8(u8::from(i.unique))
                .u32(i.columns.len() as u32);
            for &c in &i.columns {
                w.u32(c as u32);
            }
        }
        w.u32(self.checks.len() as u32);
        for (name, expr) in &self.checks {
            w.str(name).str(expr);
        }
        w.u32(self.next_index_id);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=TABLE_VERSION).contains(&version) {
            return Err(Error::internal("unknown table definition version"));
        }
        let id = r.u32()?;
        let name = r.str()?;
        let n = r.u32()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r.str()?;
            let ty = type_of_tag(r.u8()?)?;
            let nullable = r.u8()? == 1;
            let default = r.opt_str()?;
            let max_len = match r.u32()? {
                0 => None,
                n => Some(n - 1),
            };
            let numeric = if version >= 2 {
                match (r.u32()?, r.u32()?) {
                    (0, _) => None,
                    (p, s) => Some((p - 1, s)),
                }
            } else {
                None
            };
            columns.push(Column {
                name,
                ty,
                nullable,
                default,
                max_len,
                numeric,
            });
        }
        let n = r.u32()? as usize;
        let primary_key = (0..n)
            .map(|_| r.u32().map(|p| p as usize))
            .collect::<Result<_>>()?;
        let n = r.u32()? as usize;
        let mut indexes = Vec::with_capacity(n);
        for _ in 0..n {
            let id = r.u32()?;
            let name = r.str()?;
            let unique = r.u8()? == 1;
            let m = r.u32()? as usize;
            let columns = (0..m)
                .map(|_| r.u32().map(|c| c as usize))
                .collect::<Result<_>>()?;
            indexes.push(Index {
                id,
                name,
                columns,
                unique,
            });
        }
        let n = r.u32()? as usize;
        let checks = (0..n)
            .map(|_| Ok((r.str()?, r.str()?)))
            .collect::<Result<_>>()?;
        let next_index_id = r.u32()?;
        Ok(Self {
            id,
            name,
            columns,
            primary_key,
            indexes,
            checks,
            next_index_id,
        })
    }
}

fn type_tag(t: DataType) -> u8 {
    match t {
        DataType::Bool => 1,
        DataType::Int => 2,
        DataType::Float => 3,
        DataType::Text => 4,
        DataType::Numeric => 5,
        DataType::Date => 6,
        DataType::Timestamp => 7,
        DataType::TimestampTz => 8,
        DataType::Interval => 9,
    }
}

fn type_of_tag(t: u8) -> Result<DataType> {
    Ok(match t {
        1 => DataType::Bool,
        2 => DataType::Int,
        3 => DataType::Float,
        4 => DataType::Text,
        5 => DataType::Numeric,
        6 => DataType::Date,
        7 => DataType::Timestamp,
        8 => DataType::TimestampTz,
        9 => DataType::Interval,
        _ => return Err(Error::internal("unknown column type tag")),
    })
}

fn table_key(name: &str) -> Vec<u8> {
    let mut k = CATALOG.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn id_key(prefix: &[u8], id: u32) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// The table named `name`, if it exists.
pub fn get_table(txn: &dyn KvTxn, name: &str) -> Result<Option<Table>> {
    txn.get(&table_key(name))?
        .map(|b| Table::decode(&b))
        .transpose()
}

/// The table named `name`, or `42P01`.
pub fn table(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    get_table(txn, name)?
        .ok_or_else(|| Error::undefined_table(format!("relation \"{name}\" does not exist")))
}

/// Every table, by name.
pub fn tables(txn: &dyn KvTxn) -> Result<Vec<Table>> {
    let end = prefix_end(CATALOG).expect("catalog prefix");
    let mut out = Vec::new();
    for e in txn.scan(Bound::Included(CATALOG), Bound::Excluded(&end))? {
        out.push(Table::decode(&e?.1)?);
    }
    Ok(out)
}

/// The next value of sequence `name`.
pub fn next_sequence(txn: &mut dyn KvTxn, name: &str) -> Result<u64> {
    let mut key = SEQUENCE.to_vec();
    key.extend_from_slice(name.as_bytes());
    let next = txn
        .get(&key)?
        .map_or(1, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    txn.put(&key, &(next + 1).to_le_bytes())?;
    Ok(next)
}

/// The next hidden row id of `table`.
pub fn next_rowid(txn: &mut dyn KvTxn, table: &Table) -> Result<i64> {
    let key = id_key(ROWID, table.id);
    let next = txn
        .get(&key)?
        .map_or(1, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    txn.put(&key, &(next + 1).to_le_bytes())?;
    Ok(next as i64)
}

/// Stores a new table, giving it an id; fails if the name is taken.
pub fn create_table(txn: &mut dyn KvTxn, mut table: Table) -> Result<Table> {
    if get_table(txn, &table.name)?.is_some() {
        return Err(Error::duplicate_table(format!(
            "relation \"{}\" already exists",
            table.name
        )));
    }
    table.id = u32::try_from(next_sequence(txn, "table_id")?)
        .map_err(|_| Error::out_of_range("too many tables"))?;
    txn.put(&table_key(&table.name), &table.encode())?;
    Ok(table)
}

/// Replaces a table's definition (same name).
pub fn update_table(txn: &mut dyn KvTxn, table: &Table) -> Result<()> {
    txn.put(&table_key(&table.name), &table.encode())?;
    Ok(())
}

/// Deletes every entry in `[start, end)`; returns how many.
pub fn delete_range(txn: &mut dyn KvTxn, start: &[u8], end: Option<&[u8]>) -> Result<u64> {
    let end_bound = end.map_or(Bound::Unbounded, Bound::Excluded);
    let keys: Vec<Vec<u8>> = txn
        .scan(Bound::Included(start), end_bound)?
        .map(|e| e.map(|(k, _)| k))
        .collect::<nucleus_kv::Result<_>>()?;
    for k in &keys {
        txn.delete(k)?;
    }
    Ok(keys.len() as u64)
}

/// Drops a table and all its data.
pub fn drop_table(txn: &mut dyn KvTxn, table: &Table) -> Result<()> {
    let prefix = table.data_prefix();
    delete_range(txn, &prefix, prefix_end(&prefix).as_deref())?;
    txn.delete(&table_key(&table.name))?;
    txn.delete(&id_key(ROWID, table.id))?;
    txn.delete(&id_key(STATS, table.id))?;
    Ok(())
}

/// The table holding index `name`, if any.
pub fn find_index(txn: &dyn KvTxn, name: &str) -> Result<Option<(Table, usize)>> {
    for t in tables(txn)? {
        if let Some(i) = t.indexes.iter().position(|i| i.name == name) {
            return Ok(Some((t, i)));
        }
    }
    Ok(None)
}

/// The statistics key of a table.
pub fn stats_key(table: &Table) -> Vec<u8> {
    id_key(STATS, table.id)
}

/// A row's primary index key.
pub fn row_key(table: &Table, pk: &[Value]) -> Vec<u8> {
    let mut k = table.index_prefix(0);
    k.extend_from_slice(&crate::encoding::encode_key(pk));
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_roundtrip() {
        let t = Table {
            id: 7,
            name: "people".into(),
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: DataType::Int,
                    nullable: false,
                    default: None,
                    max_len: None,
                    numeric: None,
                },
                Column {
                    name: "name".into(),
                    ty: DataType::Text,
                    nullable: true,
                    default: Some("'anon'".into()),
                    max_len: Some(20),
                    numeric: None,
                },
                Column {
                    name: "price".into(),
                    ty: DataType::Numeric,
                    nullable: true,
                    default: None,
                    max_len: None,
                    numeric: Some((10, 2)),
                },
            ],
            primary_key: vec![0],
            indexes: vec![Index {
                id: 1,
                name: "people_name".into(),
                columns: vec![1],
                unique: true,
            }],
            checks: vec![("id_positive".into(), "id > 0".into())],
            next_index_id: 2,
        };
        assert_eq!(Table::decode(&t.encode()).unwrap(), t);
        assert!(t.index_prefix(0) < t.index_prefix(1));
        assert!(t.data_prefix().len() == 5 && t.index_prefix(1).starts_with(&t.data_prefix()));
    }
}
