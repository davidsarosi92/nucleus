//! Documents by MongoDB's rules (PLAN.md §M16 Q3, D22): what a document
//! API's `find` and `update` need beyond the filter ([`super::path::filter`]),
//! as SQL functions over `json`, so each request is one SQL statement:
//!
//! - `json_update(doc, update)`: the document an update document makes —
//!   its operators (`$set $unset $inc $mul $min $max $rename $push
//!   $addToSet $pop $pull $pullAll`; `$setOnInsert` only on an insert)
//!   applied, or a replacement (no operators) put in its place, the `_id`
//!   kept;
//! - `json_upsert_document(filter, update)`: the document an upsert
//!   inserts when nothing matches — the filter's equalities, then the
//!   update (`$setOnInsert` too);
//! - `json_project(doc, projection)`: the fields a projection keeps (or
//!   drops), `$slice` of an array;
//! - `json_sort_key(doc, sort)`: a `bytea` whose order is MongoDB's sort
//!   by a sort document (`{"age": -1, "name": 1}`): a field's array by
//!   its least element ascending, its greatest descending; values of
//!   different types in MongoDB's order (an empty array, `null` or none,
//!   numbers, strings, objects, arrays, booleans).
//!
//! Paths are dotted; a number in an update's path is an array position.
//! JSON has one number type: arithmetic is exact (`numeric`'s).  Keys are
//! unordered (§M15 item 3): `$set` adds no field "at the end", and
//! objects compare as their sorted keys.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::form::{self, Node};
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::types::Value;

// ------------------------------------------------------------- the tree

/// A document, as a tree to change.
#[derive(Clone, Debug)]
pub(crate) enum T {
    Null,
    Bool(bool),
    /// A number: its json bytes.
    Num(Vec<u8>),
    Str(String),
    Arr(Vec<T>),
    Obj(BTreeMap<String, T>),
}

impl T {
    pub(crate) fn from_bytes(b: &[u8]) -> Result<T> {
        Ok(match form::root(b)? {
            Node::Null => T::Null,
            Node::Bool(v) => T::Bool(v),
            Node::Int(_) | Node::Dec(_) => T::Num(b.to_vec()),
            Node::Str(s) => T::Str(form::text_of(s)?.to_string()),
            Node::Array(a) => T::Arr(
                (0..a.len())
                    .map(|i| T::from_bytes(a.bytes(i)?))
                    .collect::<Result<_>>()?,
            ),
            Node::Object(o) => T::Obj(
                (0..o.len())
                    .map(|i| Ok((o.key(i)?.to_string(), T::from_bytes(o.value_bytes(i)?)?)))
                    .collect::<Result<_>>()?,
            ),
        })
    }

    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(match self {
            T::Null => {
                let mut b = Vec::new();
                form::put_null(&mut b);
                b
            }
            T::Bool(v) => {
                let mut b = Vec::new();
                form::put_bool(&mut b, *v);
                b
            }
            T::Num(b) => b.clone(),
            T::Str(s) => super::ops::string(s),
            T::Arr(a) => {
                let items = a.iter().map(T::to_bytes).collect::<Result<Vec<_>>>()?;
                super::ops::array_of(&items)?
            }
            T::Obj(o) => {
                let items = o
                    .iter()
                    .map(|(k, v)| Ok((k.as_str(), v.to_bytes()?)))
                    .collect::<Result<Vec<_>>>()?;
                super::ops::object_of(items)?
            }
        })
    }

    fn number(&self) -> Option<Result<Decimal>> {
        match self {
            T::Num(b) => Some(super::to_text(b).and_then(|t| Decimal::parse(&t))),
            _ => None,
        }
    }

    fn of_decimal(d: Decimal) -> Result<T> {
        Ok(T::Num(super::ops::from_value(&Value::Numeric(d))?))
    }

    fn type_name(&self) -> &'static str {
        match self {
            T::Null => "null",
            T::Bool(_) => "bool",
            T::Num(_) => "number",
            T::Str(_) => "string",
            T::Arr(_) => "array",
            T::Obj(_) => "object",
        }
    }
}

/// MongoDB's order of types: null, numbers, strings, objects, arrays,
/// booleans (and, as a sort key, an empty array before them all).
fn rank(t: &T) -> u8 {
    match t {
        T::Null => 1,
        T::Num(_) => 2,
        T::Str(_) => 3,
        T::Obj(_) => 4,
        T::Arr(_) => 5,
        T::Bool(_) => 8,
    }
}

/// MongoDB's comparison of two values (`$min`, `$max`, equality in
/// `$addToSet` and `$pull`).
pub(crate) fn compare(a: &T, b: &T) -> Ordering {
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (T::Num(x), T::Num(y)) => key(x).cmp(&key(y)),
        (T::Str(x), T::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (T::Bool(x), T::Bool(y)) => x.cmp(y),
        (T::Arr(x), T::Arr(y)) => {
            for (p, q) in x.iter().zip(y) {
                let o = compare(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (T::Obj(x), T::Obj(y)) => {
            for ((kp, p), (kq, q)) in x.iter().zip(y) {
                let o = rank(p)
                    .cmp(&rank(q))
                    .then_with(|| kp.as_bytes().cmp(kq.as_bytes()))
                    .then_with(|| compare(p, q));
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => Ordering::Equal,
    }
}

fn key(b: &[u8]) -> Vec<u8> {
    super::key::of(b).unwrap_or_default()
}

fn equal(a: &T, b: &T) -> bool {
    compare(a, b) == Ordering::Equal
}

fn bad(msg: impl Into<String>) -> Error {
    Error::invalid_parameter(msg.into())
}

fn unsupported(what: &str) -> Error {
    Error::feature(format!("{what} is not supported in a document update"))
}

fn object(b: &[u8], what: &str) -> Result<BTreeMap<String, T>> {
    match T::from_bytes(b)? {
        T::Obj(o) => Ok(o),
        _ => Err(bad(format!("{what} must be a JSON object"))),
    }
}

/// A dotted path's parts.
fn parts(path: &str) -> Result<Vec<String>> {
    let p: Vec<String> = path.split('.').map(str::to_string).collect();
    for k in &p {
        if k.is_empty() {
            return Err(bad(format!("the path \"{path}\" has an empty part")));
        }
        if k.starts_with('$') {
            return Err(unsupported(&format!(
                "the positional part {k} (in \"{path}\")"
            )));
        }
    }
    Ok(p)
}

fn position(k: &str) -> Option<usize> {
    (!k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        .then(|| k.parse().ok())
        .flatten()
}

// ------------------------------------------------------------- reaching

/// The value at `path`, if there is one (positions index arrays).
fn get<'t>(t: &'t T, path: &[String]) -> Option<&'t T> {
    let Some((k, rest)) = path.split_first() else {
        return Some(t);
    };
    match t {
        T::Obj(o) => get(o.get(k)?, rest),
        T::Arr(a) => get(a.get(position(k)?)?, rest),
        _ => None,
    }
}

/// Sets `path` to `v`, making the way (objects; an array padded with
/// `null` to a position).
fn set(t: &mut T, path: &[String], v: T, whole: &str) -> Result<()> {
    let Some((last, parent)) = path.split_last() else {
        *t = v;
        return Ok(());
    };
    make_way(t, parent, whole)?;
    let mut at = t;
    for k in parent {
        at = match at {
            T::Obj(o) => o.get_mut(k).ok_or_else(|| Error::internal("a path made"))?,
            T::Arr(a) => &mut a[position(k).unwrap_or(0)],
            _ => return Err(Error::internal("a path made")),
        };
    }
    match at {
        T::Obj(o) => {
            o.insert(last.clone(), v);
        }
        T::Arr(a) => {
            let i = position(last).ok_or_else(|| {
                bad(format!(
                    "cannot create field '{last}' in an array (in \"{whole}\")"
                ))
            })?;
            if i >= a.len() {
                a.resize(i + 1, T::Null);
            }
            a[i] = v;
        }
        _ => return Err(Error::internal("a path made")),
    }
    Ok(())
}

/// The objects `path` goes through, made where missing; an error where a
/// value on the way is no object or array.
fn make_way(t: &mut T, path: &[String], whole: &str) -> Result<()> {
    let mut at = t;
    for k in path {
        at = match at {
            T::Obj(o) => o
                .entry(k.clone())
                .or_insert_with(|| T::Obj(BTreeMap::new())),
            T::Arr(a) => {
                let i = position(k).ok_or_else(|| {
                    bad(format!(
                        "cannot create field '{k}' in an array (in \"{whole}\")"
                    ))
                })?;
                if i >= a.len() {
                    a.resize(i, T::Null);
                    a.push(T::Obj(BTreeMap::new()));
                }
                &mut a[i]
            }
            other => {
                return Err(bad(format!(
                    "cannot create field '{k}' in a {} (in \"{whole}\")",
                    other.type_name()
                )));
            }
        };
    }
    if !matches!(at, T::Obj(_) | T::Arr(_)) {
        return Err(bad(format!(
            "cannot create a field in a {} (in \"{whole}\")",
            at.type_name()
        )));
    }
    Ok(())
}

/// Removes `path` (an array's element becomes `null`, as MongoDB's
/// `$unset`).
fn unset(t: &mut T, path: &[String]) -> Option<T> {
    let (last, parent) = path.split_last()?;
    let mut at = t;
    for k in parent {
        at = match at {
            T::Obj(o) => o.get_mut(k)?,
            T::Arr(a) => a.get_mut(position(k)?)?,
            _ => return None,
        };
    }
    match at {
        T::Obj(o) => o.remove(last),
        T::Arr(a) => {
            let i = position(last)?;
            a.get_mut(i).map(|v| std::mem::replace(v, T::Null))
        }
        _ => None,
    }
}

// ------------------------------------------------------------- update

const OPERATORS: [&str; 13] = [
    "$set",
    "$unset",
    "$inc",
    "$mul",
    "$min",
    "$max",
    "$rename",
    "$push",
    "$addToSet",
    "$pop",
    "$pull",
    "$pullAll",
    "$setOnInsert",
];

/// An update document's operators, or `None` for a replacement.
fn operators(u: &BTreeMap<String, T>) -> Result<Option<Vec<(&str, &BTreeMap<String, T>)>>> {
    let ops = u.keys().filter(|k| k.starts_with('$')).count();
    if ops == 0 {
        return Ok(None);
    }
    if ops != u.len() {
        return Err(bad("an update document mixes operators ($…) and fields"));
    }
    let mut out = Vec::new();
    for (k, v) in u {
        if !OPERATORS.contains(&k.as_str()) {
            return Err(match k.as_str() {
                "$currentDate" | "$bit" => unsupported(k),
                _ => bad(format!("unknown update operator: {k}")),
            });
        }
        let T::Obj(fields) = v else {
            return Err(bad(format!("{k} takes an object of fields")));
        };
        out.push((k.as_str(), fields));
    }
    Ok(Some(out))
}

/// Every path an update touches must be apart from the others (MongoDB's
/// "would create a conflict").
fn conflicts(ops: &[(&str, &BTreeMap<String, T>)], insert: bool) -> Result<()> {
    let mut paths: Vec<String> = Vec::new();
    for (op, fields) in ops {
        if *op == "$setOnInsert" && !insert {
            continue;
        }
        for (f, v) in fields.iter() {
            paths.push(f.clone());
            if *op == "$rename"
                && let T::Str(to) = v
            {
                paths.push(to.clone());
            }
        }
    }
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            let under = |x: &str, y: &str| x == y || y.starts_with(&format!("{x}."));
            if under(a, b) || under(b, a) {
                return Err(bad(format!(
                    "updating the path '{b}' would create a conflict at '{a}'"
                )));
            }
        }
    }
    Ok(())
}

/// `doc` with update `u` applied (`insert`: `$setOnInsert` too).
fn apply(doc: &mut T, u: &BTreeMap<String, T>, insert: bool) -> Result<()> {
    let Some(ops) = operators(u)? else {
        // A replacement: its fields, the `_id` kept.
        let id = match doc {
            T::Obj(o) => o.get("_id").cloned(),
            _ => None,
        };
        let mut new = u.clone();
        if let Some(id) = id {
            match new.get("_id") {
                Some(n) if !equal(n, &id) => {
                    return Err(bad("a replacement may not change the document's _id"));
                }
                _ => {
                    new.insert("_id".into(), id);
                }
            }
        }
        *doc = T::Obj(new);
        return Ok(());
    };
    conflicts(&ops, insert)?;
    let id = get(doc, &["_id".to_string()]).cloned();
    for (op, fields) in ops {
        for (field, v) in fields {
            let p = parts(field)?;
            operator(doc, op, &p, field, v, insert)?;
        }
    }
    if let Some(id) = id
        && !get(doc, &["_id".to_string()]).is_some_and(|n| equal(n, &id))
    {
        return Err(bad("an update may not change the document's _id"));
    }
    Ok(())
}

fn number_arg(op: &str, v: &T) -> Result<Decimal> {
    v.number()
        .ok_or_else(|| bad(format!("{op} takes a number, not a {}", v.type_name())))?
}

fn operator(doc: &mut T, op: &str, p: &[String], field: &str, v: &T, insert: bool) -> Result<()> {
    match op {
        "$set" => set(doc, p, v.clone(), field),
        "$setOnInsert" if insert => set(doc, p, v.clone(), field),
        "$setOnInsert" => Ok(()),
        "$unset" => {
            unset(doc, p);
            Ok(())
        }
        "$inc" | "$mul" => {
            let n = number_arg(op, v)?;
            let new = match get(doc, p) {
                None => {
                    if op == "$inc" {
                        n
                    } else {
                        Decimal::from_i64(0)
                    }
                }
                Some(cur) => {
                    let c = cur.number().ok_or_else(|| {
                        bad(format!(
                            "cannot apply {op} to a value of type {} (\"{field}\")",
                            cur.type_name()
                        ))
                    })??;
                    if op == "$inc" {
                        c.checked_add(n)?
                    } else {
                        c.checked_mul(n)?
                    }
                }
            };
            set(doc, p, T::of_decimal(new)?, field)
        }
        "$min" | "$max" => {
            let replace = match get(doc, p) {
                None => true,
                Some(cur) => {
                    let o = compare(v, cur);
                    if op == "$min" {
                        o == Ordering::Less
                    } else {
                        o == Ordering::Greater
                    }
                }
            };
            if replace {
                set(doc, p, v.clone(), field)?;
            }
            Ok(())
        }
        "$rename" => {
            let T::Str(to) = v else {
                return Err(bad("$rename takes the new name as a string"));
            };
            let q = parts(to)?;
            traversable(doc, p, field)?;
            if get(doc, p).is_none() {
                // Nothing to rename: the target is not looked at.
                return Ok(());
            }
            if q == p {
                return Err(bad(format!(
                    "$rename's source and target are the same: {field}"
                )));
            }
            let through_array = |path: &[String]| {
                (1..path.len()).any(|n| matches!(get(doc, &path[..n]), Some(T::Arr(_))))
            };
            if through_array(p) || through_array(&q) {
                return Err(bad(format!("$rename cannot go through an array: {field}")));
            }
            if let Some(val) = unset(doc, p) {
                set(doc, &q, val, to)?;
            }
            Ok(())
        }
        "$push" | "$addToSet" => {
            let (each, modifiers) = match v {
                T::Obj(o) if o.contains_key("$each") => {
                    let T::Arr(items) = &o["$each"] else {
                        return Err(bad(format!("{op}'s $each takes an array")));
                    };
                    (items.clone(), Some(o))
                }
                _ => (vec![v.clone()], None),
            };
            let mut arr = match get(doc, p) {
                None => Vec::new(),
                Some(T::Arr(a)) => a.clone(),
                Some(other) => {
                    return Err(bad(format!(
                        "{op} needs an array, \"{field}\" is a {}",
                        other.type_name()
                    )));
                }
            };
            if op == "$addToSet" {
                if let Some(m) = modifiers
                    && m.len() > 1
                {
                    return Err(bad("$addToSet takes $each alone"));
                }
                for item in each {
                    if !arr.iter().any(|a| equal(a, &item)) {
                        arr.push(item);
                    }
                }
            } else {
                push(&mut arr, each, modifiers)?;
            }
            set(doc, p, T::Arr(arr), field)
        }
        "$pop" => {
            let n = number_arg(op, v)?;
            traversable(doc, p, field)?;
            match slot_if(doc, p) {
                None => Ok(()),
                Some(T::Arr(a)) => {
                    if !a.is_empty() {
                        if n < Decimal::from_i64(0) {
                            a.remove(0);
                        } else {
                            a.pop();
                        }
                    }
                    Ok(())
                }
                Some(other) => Err(bad(format!(
                    "$pop needs an array, \"{field}\" is a {}",
                    other.type_name()
                ))),
            }
        }
        "$pull" | "$pullAll" => {
            let matches: Box<dyn Fn(&T) -> Result<bool>> = if op == "$pullAll" {
                let T::Arr(values) = v else {
                    return Err(bad("$pullAll takes an array"));
                };
                let values = values.clone();
                Box::new(move |e| Ok(values.iter().any(|x| equal(x, e))))
            } else {
                pull_condition(v)?
            };
            traversable(doc, p, field)?;
            match slot_if(doc, p) {
                None => Ok(()),
                Some(T::Arr(a)) => {
                    let mut kept = Vec::with_capacity(a.len());
                    for e in a.drain(..) {
                        if !matches(&e)? {
                            kept.push(e);
                        }
                    }
                    *a = kept;
                    Ok(())
                }
                Some(other) => Err(bad(format!(
                    "{op} needs an array, \"{field}\" is a {}",
                    other.type_name()
                ))),
            }
        }
        _ => Err(bad(format!("unknown update operator: {op}"))),
    }
}

/// MongoDB's check before `$pop`, `$pull`, `$pullAll` and `$rename`'s
/// source: the path may end where a field is missing, but not go through
/// a scalar or by a name through an array.
fn traversable(t: &T, path: &[String], whole: &str) -> Result<()> {
    let mut at = t;
    for k in path {
        at = match at {
            T::Obj(o) => match o.get(k) {
                Some(v) => v,
                None => return Ok(()),
            },
            T::Arr(a) => match position(k) {
                Some(i) => match a.get(i) {
                    Some(v) => v,
                    None => return Ok(()),
                },
                None => {
                    return Err(bad(format!(
                        "cannot use the part '{k}' of \"{whole}\" to go through an array"
                    )));
                }
            },
            other => {
                return Err(bad(format!(
                    "cannot use the part '{k}' of \"{whole}\" to go through a {}",
                    other.type_name()
                )));
            }
        };
    }
    Ok(())
}

/// The value at `path` to change, if there is one.
fn slot_if<'t>(t: &'t mut T, path: &[String]) -> Option<&'t mut T> {
    let Some((k, rest)) = path.split_first() else {
        return Some(t);
    };
    match t {
        T::Obj(o) => slot_if(o.get_mut(k)?, rest),
        T::Arr(a) => slot_if(a.get_mut(position(k)?)?, rest),
        _ => None,
    }
}

/// `$push` with `$each` and its modifiers `$position`, `$sort`, `$slice`.
fn push(arr: &mut Vec<T>, each: Vec<T>, m: Option<&BTreeMap<String, T>>) -> Result<()> {
    let int = |k: &str| -> Result<Option<i64>> {
        match m.and_then(|m| m.get(k)) {
            None => Ok(None),
            Some(v) => {
                let d = v
                    .number()
                    .ok_or_else(|| bad(format!("{k} takes an integer")))??;
                let t = d.to_string();
                t.parse::<i64>()
                    .map(Some)
                    .map_err(|_| bad(format!("{k} takes an integer")))
            }
        }
    };
    if let Some(m) = m {
        for k in m.keys() {
            if !matches!(k.as_str(), "$each" | "$position" | "$slice" | "$sort") {
                return Err(bad(format!("unknown $push modifier: {k}")));
            }
        }
    }
    let at = match int("$position")? {
        None => arr.len(),
        Some(p) if p < 0 => arr.len().saturating_sub(p.unsigned_abs() as usize),
        Some(p) => (p as usize).min(arr.len()),
    };
    arr.splice(at..at, each);
    if let Some(s) = m.and_then(|m| m.get("$sort")) {
        match s {
            T::Num(_) => {
                let desc = s
                    .number()
                    .transpose()?
                    .is_some_and(|d| d < Decimal::from_i64(0));
                arr.sort_by(|a, b| {
                    let o = compare(a, b);
                    if desc { o.reverse() } else { o }
                });
            }
            T::Obj(keys) if keys.len() > 1 => {
                return Err(Error::feature(
                    "a $sort by several fields in $push (a json object's keys are unordered)",
                ));
            }
            T::Obj(keys) => {
                let keys: Vec<(Vec<String>, bool)> = keys
                    .iter()
                    .map(|(k, d)| {
                        Ok((
                            parts(k)?,
                            d.number()
                                .transpose()?
                                .is_some_and(|d| d < Decimal::from_i64(0)),
                        ))
                    })
                    .collect::<Result<_>>()?;
                arr.sort_by(|a, b| {
                    for (k, desc) in &keys {
                        let (x, y) = (get(a, k).unwrap_or(&T::Null), get(b, k).unwrap_or(&T::Null));
                        let o = compare(x, y);
                        let o = if *desc { o.reverse() } else { o };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    Ordering::Equal
                });
            }
            _ => return Err(bad("$sort takes 1, -1 or an object of them")),
        }
    }
    if let Some(n) = int("$slice")? {
        if n >= 0 {
            arr.truncate(n as usize);
        } else {
            let keep = n.unsigned_abs() as usize;
            if arr.len() > keep {
                arr.drain(..arr.len() - keep);
            }
        }
    }
    Ok(())
}

/// What `$pull` removes: elements a condition holds for — operators on
/// the element (`{"$gte": 6}`), a query on an element's fields
/// (`{"qty": {"$lt": 2}}`), or else elements equal to the value.
fn pull_condition(v: &T) -> Result<Box<dyn Fn(&T) -> Result<bool>>> {
    use super::path::eval::{self, Fail, St};
    let filter = match v {
        T::Obj(o) if !o.is_empty() && o.keys().all(|k| k.starts_with('$')) => {
            // On the element itself: as a field `v` of a wrapper.
            let mut w = BTreeMap::new();
            w.insert("v".to_string(), v.clone());
            Some((T::Obj(w), true))
        }
        T::Obj(o) if !o.is_empty() && o.keys().all(|k| !k.starts_with('$')) => {
            Some((v.clone(), false))
        }
        _ => None,
    };
    let Some((filter, wrap)) = filter else {
        let v = v.clone();
        return Ok(Box::new(move |e| Ok(equal(e, &v))));
    };
    let path = super::path::filter::compile(&filter.to_bytes()?)?;
    Ok(Box::new(move |e| {
        let doc = if wrap {
            let mut w = BTreeMap::new();
            w.insert("v".to_string(), e.clone());
            T::Obj(w)
        } else if matches!(e, T::Obj(_)) {
            e.clone()
        } else {
            return Ok(false);
        };
        let b = doc.to_bytes()?;
        match eval::run(&path, &b, None, false, None) {
            Ok(st) => Ok(st == St::Found),
            Err(Fail::Soft(_)) => Ok(false),
            Err(Fail::Hard(e)) => Err(e),
        }
    }))
}

/// `json_update(doc, update)`.
pub fn update(doc: &[u8], u: &[u8]) -> Result<Vec<u8>> {
    let u = object(u, "an update document")?;
    if u.is_empty() {
        return Err(bad("an update document must not be empty"));
    }
    let mut d = T::from_bytes(doc)?;
    apply(&mut d, &u, false)?;
    d.to_bytes()
}

/// Checks an update document whole, before any document: applied to an
/// empty one, where only what it says can fail (an unknown operator, a
/// conflict, an argument of the wrong kind, a condition that is no
/// filter) — as MongoDB refuses it whether or not anything matches.
pub fn check_update(u: &[u8]) -> Result<()> {
    update(&T::Obj(BTreeMap::new()).to_bytes()?, u).map(|_| ())
}

/// Checks a projection whole (on an empty document).
pub fn check_projection(p: &[u8]) -> Result<()> {
    project(&T::Obj(BTreeMap::new()).to_bytes()?, p).map(|_| ())
}

/// `json_upsert_document(filter, update)`: the filter's equalities (its
/// fields with a value or `$eq`, through `$and`), then the update.
pub fn upsert_document(filter: &[u8], u: &[u8]) -> Result<Vec<u8>> {
    let f = object(filter, "a filter")?;
    let u = object(u, "an update document")?;
    let mut d = T::Obj(BTreeMap::new());
    if operators(&u)?.is_some() {
        equalities(&mut d, &f)?;
        apply(&mut d, &u, true)?;
    } else {
        // A replacement: it, with the filter's `_id` when it has none.
        let mut new = u.clone();
        if !new.contains_key("_id")
            && let Some(id) = f.get("_id").filter(|v| !operator_object(v))
        {
            new.insert("_id".into(), id.clone());
        }
        d = T::Obj(new);
    }
    d.to_bytes()
}

fn operator_object(v: &T) -> bool {
    matches!(v, T::Obj(o) if o.keys().next().is_some_and(|k| k.starts_with('$')))
}

fn equalities(d: &mut T, f: &BTreeMap<String, T>) -> Result<()> {
    let mut found = Vec::new();
    collect_equalities(f, &mut found);
    // A path set twice, or within another: MongoDB's "matched twice".
    for (n, (a, _)) in found.iter().enumerate() {
        for (b, _) in &found[n + 1..] {
            let under = |x: &str, y: &str| x == y || y.starts_with(&format!("{x}."));
            if under(a, b) || under(b, a) {
                return Err(bad(format!(
                    "an upsert's filter sets the path '{a}' twice (with '{b}')"
                )));
            }
        }
    }
    for (field, value) in found {
        let p = parts(&field)?;
        set(d, &p, value, &field)?;
    }
    Ok(())
}

fn collect_equalities(f: &BTreeMap<String, T>, out: &mut Vec<(String, T)>) {
    for (k, v) in f {
        match k.as_str() {
            "$and" => {
                if let T::Arr(items) = v {
                    for i in items {
                        if let T::Obj(o) = i {
                            collect_equalities(o, out);
                        }
                    }
                }
            }
            k if k.starts_with('$') => {}
            field => match v {
                T::Obj(o) if operator_object(v) => {
                    if let Some(e) = o.get("$eq") {
                        out.push((field.to_string(), e.clone()));
                    }
                }
                other => out.push((field.to_string(), other.clone())),
            },
        }
    }
}

// ------------------------------------------------------------- projection

/// `json_project(doc, projection)`.
pub fn project(doc: &[u8], proj: &[u8]) -> Result<Vec<u8>> {
    let p = object(proj, "a projection")?;
    let d = T::from_bytes(doc)?;
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    let mut slices = Vec::new();
    let mut id = true;
    let mut id_named = false;
    for (k, v) in &p {
        let path = parts(k)?;
        match v {
            T::Obj(o) if o.len() == 1 && o.contains_key("$slice") => {
                slices.push((path, o["$slice"].clone()));
            }
            T::Obj(o) if o.keys().any(|k| k.starts_with('$')) => {
                return Err(Error::feature(format!(
                    "the projection operator {} is not supported",
                    o.keys().next().map_or("", String::as_str)
                )));
            }
            T::Bool(_) | T::Num(_) => {
                let on = match v {
                    T::Bool(b) => *b,
                    n => n
                        .number()
                        .transpose()?
                        .is_some_and(|d| d != Decimal::from_i64(0)),
                };
                if k == "_id" {
                    id = on;
                    id_named = on;
                } else if on {
                    include.push(path);
                } else {
                    exclude.push(path);
                }
            }
            _ => {
                return Err(Error::feature(
                    "a projection's value other than 0, 1, true, false or $slice",
                ));
            }
        }
    }
    if !include.is_empty() && !exclude.is_empty() {
        return Err(bad(
            "a projection either includes fields or excludes them, not both",
        ));
    }
    // `{"_id": 1}` alone includes it alone.
    let mut out = if include.is_empty() && !(id_named && exclude.is_empty() && slices.is_empty()) {
        let mut out = d.clone();
        for p in &exclude {
            drop_path(&mut out, p);
        }
        if !id {
            drop_path(&mut out, &["_id".to_string()]);
        }
        out
    } else {
        let mut out = T::Obj(BTreeMap::new());
        let mut keep = include.clone();
        keep.extend(slices.iter().map(|(p, _)| p.clone()));
        if id {
            keep.push(vec!["_id".to_string()]);
        }
        keep_paths(&d, &keep, &mut out);
        out
    };
    for (p, s) in &slices {
        slice_path(&mut out, p, s)?;
    }
    out.to_bytes()
}

/// The parts of `d` the paths name, into `out` (through arrays: each
/// object element's, scalars dropped).
fn keep_paths(d: &T, paths: &[Vec<String>], out: &mut T) {
    let T::Obj(src) = d else { return };
    let T::Obj(dst) = out else { return };
    let mut by_first: BTreeMap<&str, Vec<Vec<String>>> = BTreeMap::new();
    for p in paths {
        if let Some((k, rest)) = p.split_first() {
            by_first.entry(k).or_default().push(rest.to_vec());
        }
    }
    for (k, rests) in by_first {
        let Some(v) = src.get(k) else { continue };
        if rests.iter().any(Vec::is_empty) {
            dst.insert(k.to_string(), v.clone());
            continue;
        }
        if let Some(sub) = keep_sub(v, &rests) {
            dst.insert(k.to_string(), sub);
        }
    }
}

fn keep_sub(v: &T, rests: &[Vec<String>]) -> Option<T> {
    match v {
        T::Obj(_) => {
            let mut o = T::Obj(BTreeMap::new());
            keep_paths(v, rests, &mut o);
            Some(o)
        }
        T::Arr(a) => Some(T::Arr(
            a.iter().filter_map(|e| keep_sub(e, rests)).collect(),
        )),
        _ => None,
    }
}

fn drop_path(d: &mut T, p: &[String]) {
    let Some((k, rest)) = p.split_first() else {
        return;
    };
    match d {
        T::Obj(o) => {
            if rest.is_empty() {
                o.remove(k);
            } else if let Some(v) = o.get_mut(k) {
                drop_path(v, rest);
            }
        }
        T::Arr(a) => {
            for e in a {
                drop_path(e, p);
            }
        }
        _ => {}
    }
}

fn slice_path(d: &mut T, p: &[String], s: &T) -> Result<()> {
    let (skip, limit) = match s {
        T::Num(_) => (None, int_of(s)?),
        T::Arr(a) if a.len() == 2 => (Some(int_of(&a[0])?), int_of(&a[1])?),
        _ => return Err(bad("$slice takes n or [skip, n]")),
    };
    if skip.is_some() && limit <= 0 {
        return Err(bad("$slice's n must be positive"));
    }
    slice_at(d, p, skip, limit);
    Ok(())
}

/// `$slice` of the array at `p` — through an array, in each of its
/// objects.
fn slice_at(d: &mut T, p: &[String], skip: Option<i64>, limit: i64) {
    match (d, p.split_first()) {
        (T::Arr(a), None) => {
            let len = a.len() as i64;
            let (from, n) = match skip {
                None if limit >= 0 => (0, limit),
                None => ((len + limit).max(0), -limit),
                Some(sk) => (
                    if sk < 0 {
                        (len + sk).max(0)
                    } else {
                        sk.min(len)
                    },
                    limit,
                ),
            };
            let from = (from as usize).min(a.len());
            let to = (from + n as usize).min(a.len());
            *a = a[from..to].to_vec();
        }
        (T::Obj(o), Some((k, rest))) => {
            if let Some(v) = o.get_mut(k) {
                slice_at(v, rest, skip, limit);
            }
        }
        (T::Arr(a), Some(_)) => {
            for e in a.iter_mut().filter(|e| matches!(e, T::Obj(_))) {
                slice_at(e, p, skip, limit);
            }
        }
        _ => {}
    }
}

fn int_of(t: &T) -> Result<i64> {
    let d = t.number().ok_or_else(|| bad("an integer is expected"))??;
    d.to_string()
        .parse()
        .map_err(|_| bad("an integer is expected"))
}

// ------------------------------------------------------------- sort keys

/// `json_sort_key(doc, sort)`: bytes whose order is MongoDB's sort by
/// `sort` — compared as bytes, documents sort as MongoDB sorts them.
/// The document's keys are generated as a compound multikey index's
/// (`keys`): a tuple of the fields' values, one per element of an array
/// the fields go through; the document's key is the least tuple in the
/// sort's order (each field's bytes inverted when descending).
pub(crate) fn sort_key(doc: &[u8], sort: &[(String, T)]) -> Result<Vec<u8>> {
    let mut fields = Vec::with_capacity(sort.len());
    let mut desc = Vec::with_capacity(sort.len());
    for (i, (field, dir)) in sort.iter().enumerate() {
        desc.push(match dir.number().transpose()? {
            Some(n) if n == Decimal::from_i64(1) => false,
            Some(n) if n == Decimal::from_i64(-1) => true,
            _ => return Err(bad("a sort document's values are 1 or -1")),
        });
        fields.push((i, parts(field)?));
    }
    // Only what the paths reach, not the whole document.
    let wanted: Vec<&[String]> = fields.iter().map(|(_, p)| p.as_slice()).collect();
    let d = pruned(doc, &wanted)?;
    let mut best: Option<Vec<u8>> = None;
    for tuple in keys(&d, &fields, false)? {
        let mut bytes = Vec::new();
        let mut tuple = tuple;
        tuple.sort_by_key(|(i, _)| *i);
        for (i, v) in tuple {
            let mut seg = Vec::new();
            match v {
                None => seg.push(0),
                Some(v) => sort_bytes(&v, &mut seg)?,
            }
            if desc[i] {
                for b in &mut seg {
                    *b = !*b;
                }
            }
            bytes.extend(seg);
        }
        if best.as_ref().is_none_or(|b| bytes < *b) {
            best = Some(bytes);
        }
    }
    best.ok_or_else(|| Error::internal("a sort key without a tuple"))
}

/// A sort document's fields in their order, from its text — a json
/// object's keys are unordered (§M15 item 3), so the text is read here:
/// `{"age": -1, "name": 1}`, or the pairs `[["age", -1], ["name", 1]]`.
pub(crate) fn sort_fields(text: &str) -> Result<Vec<(String, T)>> {
    let shape =
        || bad("a sort document is an object of fields, or an array of [field, 1 or -1] pairs");
    let whole = T::from_bytes(&super::parse::parse(text)?)?;
    let fields = match whole {
        T::Arr(pairs) => pairs
            .into_iter()
            .map(|p| match p {
                T::Arr(mut kv) if kv.len() == 2 => match (kv.remove(0), kv.remove(0)) {
                    (T::Str(k), v) => Ok((k, v)),
                    _ => Err(shape()),
                },
                _ => Err(shape()),
            })
            .collect::<Result<Vec<_>>>()?,
        T::Obj(o) => {
            // The keys in the text's order: each at depth one, a string
            // followed by `:`.
            let b = text.as_bytes();
            let (mut depth, mut i, mut keys) = (0usize, 0usize, Vec::new());
            while i < b.len() {
                match b[i] {
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth = depth.saturating_sub(1),
                    b'"' => {
                        let start = i;
                        i += 1;
                        while i < b.len() && b[i] != b'"' {
                            i += if b[i] == b'\\' { 2 } else { 1 };
                        }
                        let mut j = i + 1;
                        while j < b.len() && b[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if depth == 1 && b.get(j) == Some(&b':') {
                            let quoted = text.get(start..=i).ok_or_else(shape)?;
                            if let T::Str(k) = T::from_bytes(&super::parse::parse(quoted)?)? {
                                keys.push(k);
                            }
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let mut out: Vec<(String, T)> = Vec::new();
            for k in keys {
                // A repeated key: its last value, at its first place.
                if !out.iter().any(|(x, _)| *x == k)
                    && let Some(v) = o.get(&k)
                {
                    out.push((k, v.clone()));
                }
            }
            out
        }
        _ => return Err(shape()),
    };
    if fields.is_empty() {
        return Err(bad("a sort document must not be empty"));
    }
    Ok(fields)
}

/// `t`'s bytes in MongoDB's order of values: its type's rank, then a
/// number by value, a string by its bytes, an array element by element
/// (each after a `1`, a `0` at its end: a shorter one first), an object
/// field by field (the value's rank, the name, the value) — `compare`'s
/// order, as bytes.
fn sort_bytes(t: &T, out: &mut Vec<u8>) -> Result<()> {
    fn text(s: &str, out: &mut Vec<u8>) {
        for &x in s.as_bytes() {
            out.push(x);
            if x == 0 {
                out.push(0xff);
            }
        }
        out.extend_from_slice(&[0x00, 0x01]);
    }
    out.push(rank(t));
    match t {
        T::Null => {}
        T::Bool(b) => out.push(u8::from(*b)),
        T::Num(b) => super::key::encode(form::root(b)?, out)?,
        T::Str(s) => text(s, out),
        T::Arr(a) => {
            for e in a {
                out.push(1);
                sort_bytes(e, out)?;
            }
            out.push(0);
        }
        T::Obj(o) => {
            for (k, v) in o {
                out.push(1);
                out.push(rank(v));
                text(k, out);
                sort_bytes(v, out)?;
            }
            out.push(0);
        }
    }
    Ok(())
}

/// The tree of `b` cut to what `paths` reach: an object's keys they name
/// (an array's elements each, as a path goes through arrays), a value
/// whole where a path ends.
fn pruned(b: &[u8], paths: &[&[String]]) -> Result<T> {
    if paths.iter().any(|p| p.is_empty()) {
        return T::from_bytes(b);
    }
    Ok(match form::root(b)? {
        Node::Object(o) => {
            let mut out = BTreeMap::new();
            for (n, p) in paths.iter().enumerate() {
                let k = &p[0];
                if out.contains_key(k) || paths[..n].iter().any(|q| q[0] == *k) {
                    continue;
                }
                if let Some(v) = o.get_bytes(k)? {
                    let rests: Vec<&[String]> = paths
                        .iter()
                        .filter(|q| q[0] == *k)
                        .map(|q| &q[1..])
                        .collect();
                    out.insert(k.clone(), pruned(v, &rests)?);
                }
            }
            T::Obj(out)
        }
        Node::Array(a) => T::Arr(
            (0..a.len())
                .map(|i| pruned(a.bytes(i)?, paths))
                .collect::<Result<_>>()?,
        ),
        _ => T::from_bytes(b)?,
    })
}

/// A field's value in a key: `None` for a field that is an empty array
/// (below every value).
type Key = (usize, Option<T>);

/// The key tuples of `node` for `fields` (their number, their path below
/// `node`), as a compound multikey index's: a field's path through
/// objects; where it ends at an array, one tuple per element (an empty
/// array: `None`); where it goes through one, one per element, the rest
/// of the path in it (an element that is no object: `null`) — the fields
/// through the same array sharing its elements, through two different
/// ones an error (MongoDB's parallel arrays).  `element`: `node` is an
/// array's element, so an array here is a value, not expanded.
fn keys(node: &T, fields: &[(usize, Vec<String>)], element: bool) -> Result<Vec<Vec<Key>>> {
    let mut fixed: Vec<Key> = Vec::new();
    let mut array: Option<&Vec<T>> = None;
    let mut through: Vec<(usize, Vec<String>)> = Vec::new();
    for (i, p) in fields {
        let mut at = node;
        let mut k = 0;
        loop {
            let top = element && k == 0;
            match at {
                T::Arr(a) if !top => {
                    if array.is_some_and(|b| !std::ptr::eq(a, b)) {
                        return Err(bad(
                            "cannot sort by parallel arrays (two sort fields go through different arrays in a document)",
                        ));
                    }
                    array = Some(a);
                    through.push((*i, p[k..].to_vec()));
                    break;
                }
                _ if k == p.len() => {
                    fixed.push((*i, Some(at.clone())));
                    break;
                }
                T::Obj(o) => match o.get(&p[k]) {
                    Some(v) => {
                        at = v;
                        k += 1;
                    }
                    None => {
                        fixed.push((*i, Some(T::Null)));
                        break;
                    }
                },
                _ => {
                    fixed.push((*i, Some(T::Null)));
                    break;
                }
            }
        }
    }
    let Some(a) = array else {
        return Ok(vec![fixed]);
    };
    if a.is_empty() {
        let mut t = fixed;
        t.extend(
            through
                .into_iter()
                .map(|(i, rest)| (i, if rest.is_empty() { None } else { Some(T::Null) })),
        );
        return Ok(vec![t]);
    }
    let mut out = Vec::new();
    for e in a {
        for sub in keys(e, &through, true)? {
            let mut t = fixed.clone();
            t.extend(sub);
            out.push(t);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Vec<u8> {
        super::super::parse::parse(s).unwrap()
    }

    fn t(b: Result<Vec<u8>>) -> String {
        match b {
            Ok(b) => super::super::to_text(&b).unwrap(),
            Err(e) => format!("ERROR {}", e.code),
        }
    }

    #[test]
    fn updates_apply_mongodbs_operators() {
        let d = r#"{"_id": 1, "n": 2, "a": {"b": [1, 2, 3]}, "s": "x"}"#;
        let u = |x: &str| t(update(&j(d), &j(x)));
        assert_eq!(
            u(r#"{"$set": {"a.c": 5}}"#),
            r#"{"a": {"b": [1, 2, 3], "c": 5}, "n": 2, "s": "x", "_id": 1}"#
        );
        assert_eq!(
            u(r#"{"$inc": {"n": 1.5, "m": 1}}"#),
            r#"{"a": {"b": [1, 2, 3]}, "m": 1, "n": 3.5, "s": "x", "_id": 1}"#
        );
        assert_eq!(
            u(r#"{"$unset": {"a": ""}}"#),
            r#"{"n": 2, "s": "x", "_id": 1}"#
        );
        assert_eq!(
            u(r#"{"$push": {"a.b": {"$each": [9, 0], "$sort": 1, "$slice": -3}}}"#),
            r#"{"a": {"b": [2, 3, 9]}, "n": 2, "s": "x", "_id": 1}"#
        );
        assert_eq!(
            u(r#"{"$pull": {"a.b": {"$gte": 2}}}"#),
            r#"{"a": {"b": [1]}, "n": 2, "s": "x", "_id": 1}"#
        );
        assert_eq!(u(r#"{"$inc": {"s": 1}}"#), "ERROR 22023");
        assert_eq!(u(r#"{"$set": {"a": 1, "a.b": 2}}"#), "ERROR 22023");
        assert_eq!(u(r#"{"$set": {"_id": 2}}"#), "ERROR 22023");
        assert_eq!(u(r#"{"z": 1}"#), r#"{"z": 1, "_id": 1}"#);
    }

    #[test]
    fn mongodbs_answers_in_the_corners() {
        // Each answer MongoDB 8's on the same document.
        let u = |d: &str, x: &str| t(update(&j(d), &j(x)));
        // A string is above a number; a longer array above its prefix.
        assert_eq!(u(r#"{"a": 5}"#, r#"{"$max": {"a": "x"}}"#), r#"{"a": "x"}"#);
        assert_eq!(
            u(r#"{"a": [1]}"#, r#"{"$max": {"a": [1, 2]}}"#),
            r#"{"a": [1, 2]}"#
        );
        // Not through a scalar.
        assert_eq!(
            u(r#"{"a": "x"}"#, r#"{"$rename": {"a.b": "c"}}"#),
            "ERROR 22023"
        );
        assert_eq!(u(r#"{"a": "x"}"#, r#"{"$pop": {"a.b": 1}}"#), "ERROR 22023");
        // A position from the end.
        assert_eq!(
            u(
                r#"{"a": [1, 2, 3]}"#,
                r#"{"$push": {"a": {"$each": [9], "$position": -1}}}"#
            ),
            r#"{"a": [1, 2, 9, 3]}"#
        );
        // A sort by `a.b`: through a scalar, a missing field and an empty
        // array all `null`, below 0.
        let k = |d: &str, s: &str| sort_key(&j(d), &sort_fields(s).unwrap()).unwrap();
        let ab = r#"{"a.b": 1}"#;
        assert_eq!(k(r#"{"a": 5}"#, ab), k("{}", ab));
        assert_eq!(k(r#"{"a": []}"#, ab), k("{}", ab));
        assert!(k("{}", ab) < k(r#"{"a": {"b": 0}}"#, ab));
        // Objects field by field, a value's type before the name.
        assert!(k(r#"{"a": {"b": 1}}"#, r#"{"a": 1}"#) < k(r#"{"a": {"a": "s"}}"#, r#"{"a": 1}"#));
        // `$slice` through an array of objects.
        assert_eq!(
            t(project(
                &j(r#"{"_id": 1, "a": [{"b": [1, 2]}, {"b": [3, 4]}]}"#),
                &j(r#"{"a.b": {"$slice": 1}}"#)
            )),
            r#"{"a": [{"b": [1]}, {"b": [3]}], "_id": 1}"#
        );
        // An upsert's filter naming a path twice.
        assert_eq!(
            t(upsert_document(
                &j(r#"{"a": 1, "$and": [{"a": 1}]}"#),
                &j(r#"{"$set": {"c": 1}}"#)
            )),
            "ERROR 22023"
        );
    }

    #[test]
    fn sort_documents_keep_their_order() {
        let names = |s: &str| {
            sort_fields(s)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(r#"{"b": 1, "_id": -1, "a": {"x": 1}}"#),
            ["b", "_id", "a"]
        );
        assert_eq!(names(r#"{ "b\"c" : 1 , "a":1}"#), ["b\"c", "a"]);
        assert_eq!(names(r#"[["z", 1], ["a", -1]]"#), ["z", "a"]);
        assert!(sort_fields("{}").is_err());
        assert!(sort_fields("[1]").is_err());
    }

    #[test]
    fn sort_keys_order_as_mongodb() {
        let k = |d: &str, s: &str| sort_key(&j(d), &sort_fields(s).unwrap()).unwrap();
        let asc = r#"{"a": 1}"#;
        // [] < null/missing < numbers < strings < objects < booleans
        assert!(k(r#"{"a": []}"#, asc) < k("{}", asc));
        assert!(k("{}", asc) < k(r#"{"a": -5}"#, asc));
        assert!(k(r#"{"a": 7}"#, asc) < k(r#"{"a": "b"}"#, asc));
        assert!(k(r#"{"a": "b"}"#, asc) < k(r#"{"a": {}}"#, asc));
        assert!(k(r#"{"a": {}}"#, asc) < k(r#"{"a": true}"#, asc));
        // an array by its least element ascending, its greatest descending
        assert!(k(r#"{"a": [9, 1]}"#, asc) < k(r#"{"a": 2}"#, asc));
        let desc = r#"{"a": -1}"#;
        assert!(k(r#"{"a": [9, 1]}"#, desc) < k(r#"{"a": 5}"#, desc));
    }
}
