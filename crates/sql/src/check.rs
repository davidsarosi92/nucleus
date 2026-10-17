//! A consistency check of the SQL layer's data (PLAN.md principle 5): the
//! catalog, the rows and the index entries agree.  The crash tests run it
//! after every recovery.
//!
//! Checked, for every table: each row decodes, sits under its primary key
//! and keeps `NOT NULL`; each secondary index holds exactly the entries its
//! rows imply, and a unique index no duplicates; every foreign key finds
//! its referenced row; and no keys exist for a table or index the catalog
//! does not have.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use nucleus_kv::{KvTxn, prefix_end};

use crate::catalog::{self, Table};
use crate::encoding::{decode_row, encode_key};
use crate::error::Result;
use crate::exec::{pk_suffix, row_key_of};
use crate::types::{Row, Value};

/// Every inconsistency found, described; empty when the data is consistent.
pub fn check(txn: &dyn KvTxn, reg: &crate::ext::Registry) -> Result<Vec<String>> {
    let tables = catalog::tables(txn)?;
    let mut problems = Vec::new();
    let mut rows_of: BTreeMap<&str, Vec<Row>> = BTreeMap::new();
    for t in &tables {
        rows_of.insert(&t.name, check_table(txn, reg, t, &mut problems)?);
    }
    for t in &tables {
        for fk in &t.foreign_keys {
            let Some(parent) = rows_of.get(fk.table.as_str()) else {
                problems.push(format!(
                    "{}: foreign key {} references missing table {}",
                    t.name, fk.name, fk.table
                ));
                continue;
            };
            let keys: BTreeSet<Vec<u8>> = parent
                .iter()
                .map(|r| encode_key(&pick(r, &fk.ref_columns)))
                .collect();
            for row in &rows_of[t.name.as_str()] {
                let vals = pick(row, &fk.columns);
                if !vals.iter().any(Value::is_null) && !keys.contains(&encode_key(&vals)) {
                    problems.push(format!(
                        "{}: row {row:?} breaks foreign key {}",
                        t.name, fk.name
                    ));
                }
            }
        }
    }
    // Keys of tables and indexes the catalog does not know.
    let known: BTreeMap<u32, BTreeSet<u32>> = tables
        .iter()
        .map(|t| {
            let ids = std::iter::once(0).chain(t.indexes.iter().map(|i| i.id));
            (t.id, ids.collect())
        })
        .collect();
    let data = [0x01u8];
    let end = prefix_end(&data).expect("data prefix");
    let mut seen = BTreeSet::new();
    for e in txn.scan(Bound::Included(&data), Bound::Excluded(&end))? {
        let (k, _) = e?;
        let id = |at: usize| {
            k.get(at..at + 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        };
        let (Some(table), Some(index)) = (id(1), id(5)) else {
            problems.push(format!("malformed data key {k:?}"));
            continue;
        };
        if !known.get(&table).is_some_and(|ix| ix.contains(&index)) && seen.insert((table, index)) {
            problems.push(format!(
                "keys of table id {table} index id {index}, which the catalog does not have"
            ));
        }
    }
    Ok(problems)
}

fn pick(row: &Row, columns: &[usize]) -> Vec<Value> {
    columns.iter().map(|&c| row[c].clone()).collect()
}

/// Checks one table's rows and indexes; returns its rows.
fn check_table(
    txn: &dyn KvTxn,
    reg: &crate::ext::Registry,
    t: &Table,
    problems: &mut Vec<String>,
) -> Result<Vec<Row>> {
    let keys = crate::dml::IndexKeys::new(txn, reg, t)?;
    let scan = |prefix: &[u8]| -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let end = prefix_end(prefix).expect("table prefix");
        txn.scan(Bound::Included(prefix), Bound::Excluded(&end))?
            .map(|e| e.map_err(Into::into))
            .collect()
    };
    let mut rows = Vec::new();
    let mut expected: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new(); t.indexes.len()];
    for (key, value) in scan(&t.index_prefix(0))? {
        let row = match decode_row(&value, t.columns.len()) {
            Ok(row) => row,
            Err(e) => {
                problems.push(format!(
                    "{}: row under {key:?} does not decode: {e}",
                    t.name
                ));
                continue;
            }
        };
        if !t.primary_key.is_empty() && row_key_of(t, &row, None) != key {
            problems.push(format!(
                "{}: row {row:?} is not under its primary key",
                t.name
            ));
        }
        for (c, col) in t.columns.iter().enumerate() {
            if !col.nullable && row[c].is_null() {
                problems.push(format!("{}: row {row:?} has NULL in {}", t.name, col.name));
            }
        }
        let pk = pk_suffix(t, &key);
        for (i, (want, (k, _))) in expected
            .iter_mut()
            .zip(keys.entries(txn, t, &row, &pk)?)
            .enumerate()
        {
            if want.insert(k, pk.clone()).is_some() {
                problems.push(format!(
                    "{}: row {row:?} duplicates another in unique index {}",
                    t.name, t.indexes[i].name
                ));
            }
        }
        rows.push(row);
    }
    for (i, want) in expected.iter().enumerate() {
        let got: BTreeMap<Vec<u8>, Vec<u8>> = scan(&t.index_prefix(t.indexes[i].id))?
            .into_iter()
            .collect();
        if &got != want {
            let missing = want.keys().filter(|k| !got.contains_key(*k)).count();
            let extra = got.keys().filter(|k| !want.contains_key(*k)).count();
            let wrong = want
                .iter()
                .filter(|(k, v)| got.get(*k).is_some_and(|g| g != *v))
                .count();
            problems.push(format!(
                "{}: index {} has {missing} missing, {extra} extra and {wrong} wrong entries",
                t.name, t.indexes[i].name
            ));
        }
    }
    Ok(rows)
}
