//! The containment index (PLAN.md §M14.15 J4b): `CREATE INDEX … USING
//! json (doc)` on a `json` column, for a filter's `doc @> q`, `doc ? k`,
//! `doc ?| ks` and `doc ?& ks` with the right side fixed for the scan.
//!
//! The text index's storage ([`super::store`]): a document's terms are
//! hashes, kept transactional and crash-safe as a text's words are.  A
//! *leaf's* term hashes its path — the object keys from the root to it, an
//! array's elements on their array's path — and its value (PostgreSQL's
//! `jsonb_path_ops`); a *key path's* hashes the path to and through an
//! object's key, which `jsonb_path_ops` has not: with it the index serves
//! `?` too, and `@>` of an empty object or array under a key.  A document
//! is weighted, each term 1, so no positions are kept.
//!
//! `doc @> q` needs every leaf and key path of `q` in `doc`; `doc ? k` a
//! top-level key `k`, or `k` as a string at the top (an array's element,
//! or the document itself).  The index is lossy: two values may hash
//! alike, and the terms tell paths, not which element of an array holds
//! what (`{"a": [{"b": 1}, {"c": 2}]}` has the terms of `{"a": [{"b": 1,
//! "c": 2}]}`), so the condition is checked again on each row it finds
//! ([`IndexMethod::exact`]).  A `q` without a leaf or a key (`{}`, `[]`)
//! is not served: every row is read.

use nucleus_kv::KvTxn;

use super::segment::{Doc, weight_bits};
use super::store;
use crate::catalog::{Index as IndexDef, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::index_method::{IndexMethod, RowRef};
use crate::json::form::{self, Node};
use crate::text::query::{Leaf, Node as QNode, Query};
use crate::types::{Row, Value};

/// The method's name.
pub const NAME: &str = "json";

/// The functions it serves: the operators', as bound.
const CONTAINS: &str = "@>(json,json)";
const HAS: &str = "?(json,text)";
const HAS_ANY: &str = "?|(json,text[])";
const HAS_ALL: &str = "?&(json,text[])";

/// The hashes' kinds ([`crate::object_cache::object_id`]'s, distinct from
/// the text index's 13 and the sparse index's 14): a path, a leaf, a
/// document (what a tombstone names the version of a row by).
const PATH: u64 = 15;
const LEAF: u64 = 16;
const DOC: u64 = 17;

/// The containment index method.
pub struct JsonIndex;

/// A key path's term.
fn key_term(path: &[u8]) -> String {
    format!("k{:016x}", crate::object_cache::object_id(PATH, path))
}

/// A leaf's term: its path's hash and its value's bytes (canonical: equal
/// values, equal bytes), hashed.
fn leaf_term(path: &[u8], value: &[u8]) -> String {
    let p = crate::object_cache::object_id(PATH, path);
    let mut b = Vec::with_capacity(8 + value.len());
    b.extend_from_slice(&p.to_le_bytes());
    b.extend_from_slice(value);
    format!("v{:016x}", crate::object_cache::object_id(LEAF, &b))
}

/// The terms of the node `n` (its bytes `raw`) on `path` — each key on it
/// as its length (a varint) and its bytes — into `out`; a key path's only
/// above an empty object or array where `needed` ([`needed`]).
fn walk(
    n: Node,
    raw: &[u8],
    path: &mut Vec<u8>,
    out: &mut Vec<String>,
    needed: bool,
) -> Result<()> {
    match n {
        Node::Array(a) => {
            for i in 0..a.len() {
                walk(a.get(i)?, a.bytes(i)?, path, out, needed)?;
            }
        }
        Node::Object(o) => {
            for i in 0..o.len() {
                let key = o.key_bytes(i)?;
                let mark = path.len();
                form::put_varint(path, key.len() as u64);
                path.extend_from_slice(key);
                let value = o.value(i)?;
                let empty = match value {
                    Node::Array(a) => a.is_empty(),
                    Node::Object(o) => o.is_empty(),
                    _ => false,
                };
                if !needed || empty {
                    out.push(key_term(path));
                }
                walk(value, o.value_bytes(i)?, path, out, needed)?;
                path.truncate(mark);
            }
        }
        _ => out.push(leaf_term(path, raw)),
    }
    Ok(())
}

/// The terms of a document (its binary form), sorted, each once.
fn terms(doc: &[u8]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(form::root(doc)?, doc, &mut Vec::new(), &mut out, false)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The terms a document containing `q` (its binary form) must have,
/// sorted, each once: its leaves', and a key path's only where the key
/// holds an empty object or array — a leaf below a key implies the key,
/// and a key path's postings are long (`{"user": {"name": "u42"}}` needs
/// one term, not three).
fn needed(q: &[u8]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(form::root(q)?, q, &mut Vec::new(), &mut out, true)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// A document as the index keeps it.
fn doc_of(terms: Vec<String>) -> Doc {
    let one = weight_bits(1.0);
    Doc {
        len: terms.len() as u32,
        weights: vec![one; terms.len()],
        terms: terms.into_iter().map(|t| (t, Vec::new())).collect(),
    }
}

/// The value of `row` the index holds, its hash and its document (`None`:
/// a `NULL`, not indexed).
fn indexed(t: &Table, i: usize, row: &Row) -> Result<Option<(u64, Doc)>> {
    match &row[t.indexes[i].columns[0]] {
        Value::Null => Ok(None),
        v @ Value::Ext(..) => {
            let b = crate::json::bytes(v)?;
            Ok(Some((
                crate::object_cache::object_id(DOC, b),
                doc_of(terms(b)?),
            )))
        }
        _ => Err(Error::internal("a json index of a column not json")),
    }
}

/// A query's term for a string `k` at the top: the key `k`, or `k` itself
/// (an element of a top-level array, or the document).
fn has(k: &str) -> [String; 2] {
    let mut path = Vec::new();
    form::put_varint(&mut path, k.len() as u64);
    path.extend_from_slice(k.as_bytes());
    let mut s = Vec::new();
    form::put_str(&mut s, k);
    [key_term(&path), leaf_term(&[], &s)]
}

/// The query for `function` with `args` — the leaves its rows must hold
/// (`must`, each a choice among terms) — or `None` where the index cannot
/// narrow the rows (the rows are then read).
fn query_of(function: &str, args: &[Value]) -> Result<Option<Vec<Vec<String>>>> {
    let texts = |v: &Value| -> Option<Vec<String>> {
        match v {
            Value::Array(xs) => xs
                .iter()
                .map(|x| match x {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    };
    Ok(match (function, args) {
        (CONTAINS, [q @ Value::Ext(..)]) => {
            let ts = needed(crate::json::bytes(q)?)?;
            (!ts.is_empty()).then(|| ts.into_iter().map(|t| vec![t]).collect())
        }
        (HAS, [Value::Text(k)]) => Some(vec![has(k).to_vec()]),
        (HAS_ANY, [v]) => texts(v)
            .filter(|ks| !ks.is_empty())
            .map(|ks| vec![ks.iter().flat_map(|k| has(k)).collect()]),
        (HAS_ALL, [v]) => texts(v)
            .filter(|ks| !ks.is_empty())
            .map(|ks| ks.iter().map(|k| has(k).to_vec()).collect()),
        _ => None,
    })
}

impl IndexMethod for JsonIndex {
    fn name(&self) -> &str {
        NAME
    }

    fn scrub(
        &self,
        kv: &std::sync::Arc<dyn nucleus_kv::Kv>,
        _: &Registry,
        t: &Table,
        i: usize,
    ) -> Result<()> {
        super::scrub(kv, t, i)
    }

    fn define(&self, t: &Table, ix: &mut IndexDef) -> Result<()> {
        if ix.columns.len() != 1 {
            return Err(Error::feature("a json index has one column"));
        }
        let col = &t.columns[ix.columns[0]];
        if col.ty != crate::json::data_type() {
            return Err(Error::feature(format!(
                "a json index needs a json column, not {}",
                col.ty.name()
            )));
        }
        match ix.opclasses[0].as_deref() {
            None | Some("json_ops") => {}
            Some(o) => {
                return Err(Error::undefined_object(format!(
                    "operator class \"{o}\" does not exist for access method \"json\""
                )));
            }
        }
        if let Some((k, _)) = ix.options.first() {
            return Err(Error::invalid_parameter(format!(
                "unrecognized parameter \"{k}\""
            )));
        }
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
            Some((pk, row)) => indexed(t, i, row)?.map(|d| (pk, d)),
            None => None,
        };
        let new = match new {
            Some((pk, row)) => indexed(t, i, row)?.map(|d| (pk, d)),
            None => None,
        };
        if old.is_some() && old == new {
            return Ok(());
        }
        let s = super::store(t, i);
        if let Some((pk, (hash, doc))) = old {
            let key = s.keys.tombstone(pk, hash, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_gone(&doc))?;
        }
        if let Some((pk, (hash, doc))) = new {
            let key = s.keys.delta(pk, crate::hnsw::store::nonce());
            txn.put(&key, &store::encode_insert(hash, &doc))?;
        }
        super::pending().note(super::Id {
            instance: txn.instance(),
            table: t.id,
            index: t.indexes[i].id,
        });
        Ok(())
    }

    /// The rows' documents read, made segments of [`super::BUILD_BATCH`],
    /// merged into one where they fit.
    fn build(&self, txn: &mut dyn KvTxn, _: &Registry, t: &Table, i: usize) -> Result<()> {
        let col = t.indexes[i].columns[0];
        let mut wanted = vec![false; t.columns.len()];
        wanted[col] = true;
        let s = super::store(t, i);
        let mut docs: Vec<(Vec<u8>, u64, Doc)> = Vec::new();
        let mut made = Vec::new();
        for r in crate::index_method::rows(&*txn, t, Some(&wanted))? {
            let (pk, row) = r?;
            if let Some((hash, doc)) = indexed(t, i, &row)? {
                docs.push((pk, hash, doc));
            }
            if docs.len() == super::BUILD_BATCH {
                made.push((super::segment::of_docs(&docs)?, docs.len() as u32));
                docs.clear();
            }
        }
        if !docs.is_empty() {
            made.push((super::segment::of_docs(&docs)?, docs.len() as u32));
        }
        let bytes: usize = made.iter().map(|(b, _)| b.len()).sum();
        s.append_segments(txn, made)?;
        let workers = crate::budget::workers();
        if bytes <= super::ONE_SEGMENT {
            s.merge_all(txn, workers)
        } else {
            s.merge_due(txn, workers)
        }
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
        let view = super::store(t, i).view(txn)?;
        let mut want = Vec::new();
        for (pk, row) in rows {
            if let Some((hash, _)) = indexed(t, i, row)? {
                want.push((pk.clone(), hash));
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
                "json index \"{name}\": {missing} rows missing, {extra} it should not have"
            ));
        }
        Ok(())
    }

    fn serves(&self, _: &IndexDef, function: &str) -> bool {
        matches!(function, CONTAINS | HAS | HAS_ANY | HAS_ALL)
    }

    /// Lossy: the condition is checked again on the rows it finds.
    fn exact(&self, _: &IndexDef, _: &str) -> bool {
        false
    }

    fn rows(&self, txn: &dyn KvTxn, t: &Table, i: usize) -> Result<Option<usize>> {
        Ok(Some(super::store(t, i).view(txn)?.stats(&[]).docs as usize))
    }

    /// The rows holding every leaf of the query's: a superset of those the
    /// condition holds for, sorted.
    fn rows_where(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        i: usize,
        function: &str,
        args: &[Value],
    ) -> Result<Option<Vec<Vec<u8>>>> {
        if args.iter().any(Value::is_null) {
            return Ok(Some(Vec::new()));
        }
        let Some(must) = query_of(function, args)? else {
            return Ok(None);
        };
        let mut leaves: Vec<String> = must.iter().flatten().cloned().collect();
        leaves.sort_unstable();
        leaves.dedup();
        let at = |t: &String| leaves.binary_search(t).expect("a leaf of the query");
        let root = QNode::Bool {
            must: must
                .iter()
                .map(|choice| match choice.as_slice() {
                    [one] => QNode::Leaf(at(one)),
                    many => QNode::Bool {
                        must: Vec::new(),
                        should: many.iter().map(|t| QNode::Leaf(at(t))).collect(),
                        not: Vec::new(),
                    },
                })
                .collect(),
            should: Vec::new(),
            not: Vec::new(),
        };
        let q = Query {
            leaves: leaves.into_iter().map(Leaf::Exact).collect(),
            root: Some(root),
        };
        Ok(Some(super::store(t, i).view(txn)?.matching(&q)?))
    }
}

/// Registers the method (the built-in module does, with the text index).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_index_method(std::sync::Arc::new(JsonIndex))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Vec<u8> {
        crate::json::parse::parse(text).expect("json")
    }

    /// Whether `q`'s leaves are among `d`'s: what the index finds.
    fn found(d: &str, q: &str) -> bool {
        let have = terms(&doc(d)).expect("terms");
        let want = needed(&doc(q)).expect("terms");
        want.iter().all(|t| have.binary_search(t).is_ok())
    }

    #[test]
    fn containment_finds_every_contained_document() {
        // Every pair where `d @> q` holds is found (no false negative).
        let cases = [
            (
                r#"{"a": 1, "b": [1, 2, {"c": "x"}]}"#,
                r#"{"b": [{"c": "x"}]}"#,
            ),
            (r#"{"a": 1, "b": [1, 2]}"#, r#"{"b": [2, 1, 2]}"#),
            (r#"[1, [2, 3]]"#, r#"[[3]]"#),
            (r#"["foo", "bar"]"#, r#""foo""#),
            (r#"{"a": {"b": {}}}"#, r#"{"a": {"b": {}}}"#),
            (r#"{"a": 1.50}"#, r#"{"a": 1.5}"#),
            (r#"{"a": [{"b": 1}, {"c": 2}]}"#, r#"{"a": [{"b": 1}]}"#),
        ];
        for (d, q) in cases {
            assert!(
                crate::json::ops::contains(&doc(d), &doc(q)).unwrap(),
                "{d} @> {q}"
            );
            assert!(found(d, q), "{d} @> {q}: not found");
        }
    }

    #[test]
    fn terms_tell_paths_and_values_apart() {
        assert!(!found(r#"{"a": 1}"#, r#"{"a": 2}"#));
        assert!(!found(r#"{"a": 1}"#, r#"{"b": 1}"#));
        assert!(!found(r#"{"a": {"b": 1}}"#, r#"{"b": 1}"#));
        assert!(!found(r#"{"a": "1"}"#, r#"{"a": 1}"#));
        assert!(!found(r#"{"a": {}}"#, r#"{"a": {"b": {}}}"#));
        // A path, not an element: found, and the recheck says no.
        assert!(found(
            r#"{"a": [{"b": 1}, {"c": 2}]}"#,
            r#"{"a": [{"b": 1, "c": 2}]}"#
        ));
    }

    #[test]
    fn a_top_level_string_is_a_key_or_an_element() {
        let d = terms(&doc(r#"{"k": 1, "arr": ["x"]}"#)).unwrap();
        let [key, elem] = has("k");
        assert!(d.binary_search(&key).is_ok());
        assert!(d.binary_search(&elem).is_err());
        let d = terms(&doc(r#"["k", "j"]"#)).unwrap();
        assert!(d.binary_search(&elem).is_ok());
        let d = terms(&doc(r#""k""#)).unwrap();
        assert!(d.binary_search(&elem).is_ok());
    }

    #[test]
    fn nothing_to_look_for_is_not_served() {
        let _ = crate::ext::Registry::shared_builtin(); // registers json
        assert!(
            query_of(CONTAINS, &[crate::json::from_text("{}").unwrap()])
                .unwrap()
                .is_none()
        );
        assert!(
            query_of(HAS_ANY, &[Value::Array(Vec::new())])
                .unwrap()
                .is_none()
        );
    }
}
