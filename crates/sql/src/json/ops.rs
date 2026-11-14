//! What the operators and functions do to values (PLAN.md §M14.15 J2),
//! on the binary form: reading in place, building new values from the
//! bytes of the old ones' nodes (a node's bytes are a value's: nothing is
//! re-encoded but the containers a change passes through).
//!
//! The semantics are PostgreSQL's `jsonb`'s, error codes and messages
//! included, but for its raw-scalar quirks: a scalar is not a one-element
//! array to `-> 0`.

use std::borrow::Cow;

use super::form::{self, Node};
use crate::error::{Error, Result};
use crate::types::Value;

// ------------------------------------------------------------- reading

/// `j -> key`: an object's field.
pub fn field<'a>(b: &'a [u8], key: &str) -> Result<Option<&'a [u8]>> {
    match form::root(b)? {
        Node::Object(o) => o.get_bytes(key),
        _ => Ok(None),
    }
}

/// `j -> i`: an array's element, from the end when negative.
pub fn element(b: &[u8], i: i64) -> Result<Option<&[u8]>> {
    match form::root(b)? {
        Node::Array(a) => match index(i, a.len()) {
            Some(i) => a.bytes(i).map(Some),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// A position from SQL's index `i` (negative: from the end) in `len`.
fn index(i: i64, len: usize) -> Option<usize> {
    let i = if i < 0 {
        i.checked_add(i64::try_from(len).ok()?)?
    } else {
        i
    };
    usize::try_from(i).ok().filter(|i| *i < len)
}

/// `j #> path`: the value at a path of keys and array positions; `None`
/// where it leads nowhere (or holds a `NULL`).
pub fn at_path<'a>(b: &'a [u8], path: &[Option<String>]) -> Result<Option<&'a [u8]>> {
    let mut cur = b;
    for step in path {
        let Some(step) = step else { return Ok(None) };
        let next = match form::root(cur)? {
            Node::Object(o) => o.get_bytes(step)?,
            Node::Array(a) => match step.trim().parse::<i64>().ok() {
                Some(i) => match index(i, a.len()) {
                    Some(i) => Some(a.bytes(i)?),
                    None => None,
                },
                None => None,
            },
            _ => None,
        };
        match next {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// A value as `->>` gives it: a string's text, `None` for `null`, the
/// JSON text of anything else.
pub fn as_text(b: &[u8]) -> Result<Option<String>> {
    match form::root(b)? {
        Node::Null => Ok(None),
        Node::Str(s) => Ok(Some(form::text_of(s)?.to_string())),
        _ => super::to_text(b).map(Some),
    }
}

/// `j ? key`: a key of an object, a string element of an array, or the
/// string itself.
pub fn exists(b: &[u8], key: &str) -> Result<bool> {
    Ok(match form::root(b)? {
        Node::Object(o) => o.find(key)?.is_some(),
        Node::Array(a) => {
            for i in 0..a.len() {
                if let Node::Str(s) = a.get(i)?
                    && s == key.as_bytes()
                {
                    return Ok(true);
                }
            }
            false
        }
        Node::Str(s) => s == key.as_bytes(),
        _ => false,
    })
}

/// `a @> b`: whether `a` contains `b` — an object every key of `b` with a
/// value containing `b`'s, an array an element containing each of `b`'s;
/// a scalar only itself, but a top-level array its elements.
pub fn contains(a: &[u8], b: &[u8]) -> Result<bool> {
    let (na, nb) = (form::root(a)?, form::root(b)?);
    match (na, nb) {
        (Node::Array(arr), nb) if !is_container(&nb) => {
            for i in 0..arr.len() {
                if arr.bytes(i)? == b {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => deep_contains(a, b, 0),
    }
}

fn is_container(n: &Node<'_>) -> bool {
    matches!(n, Node::Array(_) | Node::Object(_))
}

fn deep_contains(a: &[u8], b: &[u8], depth: usize) -> Result<bool> {
    if depth > super::MAX_DEPTH {
        return Err(super::too_deep());
    }
    match (form::root(a)?, form::root(b)?) {
        (Node::Object(oa), Node::Object(ob)) => {
            if ob.len() > oa.len() {
                return Ok(false);
            }
            for i in 0..ob.len() {
                let Some(av) = oa.get_bytes(ob.key(i)?)? else {
                    return Ok(false);
                };
                let bv = ob.value_bytes(i)?;
                if !same_kind_contains(av, bv, depth)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Node::Array(aa), Node::Array(ab)) => {
            // Each of `b`'s elements in some element of `a`: a scalar
            // equal to one, a container in one of its kind.
            let elems: Vec<&[u8]> = (0..aa.len()).map(|i| aa.bytes(i)).collect::<Result<_>>()?;
            let scalars: std::collections::HashSet<&[u8]> = if ab.len() > 4 {
                elems
                    .iter()
                    .copied()
                    .filter(|e| !is_container_bytes(e))
                    .collect()
            } else {
                Default::default()
            };
            'next: for j in 0..ab.len() {
                let bv = ab.bytes(j)?;
                if !is_container_bytes(bv) {
                    let found = if ab.len() > 4 {
                        scalars.contains(bv)
                    } else {
                        elems.contains(&bv)
                    };
                    if !found {
                        return Ok(false);
                    }
                    continue;
                }
                for av in &elems {
                    if same_kind_contains(av, bv, depth)? {
                        continue 'next;
                    }
                }
                return Ok(false);
            }
            Ok(true)
        }
        (na, nb) if !is_container(&na) && !is_container(&nb) => Ok(a == b),
        _ => Ok(false),
    }
}

fn is_container_bytes(b: &[u8]) -> bool {
    matches!(b.first().map(|h| h >> 4), Some(6 | 7))
}

/// `b` in `a`: equal scalars, or containers of one kind, `a`'s holding.
fn same_kind_contains(a: &[u8], b: &[u8], depth: usize) -> Result<bool> {
    let (ka, kb) = (a.first().map(|h| h >> 4), b.first().map(|h| h >> 4));
    if is_container_bytes(b) {
        Ok(ka == kb && deep_contains(a, b, depth + 1)?)
    } else {
        Ok(a == b)
    }
}

// ------------------------------------------------------------- building

/// An array of these values' bytes.
pub fn array_of<B: AsRef<[u8]>>(items: &[B]) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut ends = Vec::with_capacity(items.len());
    for it in items {
        data.extend_from_slice(it.as_ref());
        ends.push(data.len());
    }
    let mut out = Vec::with_capacity(data.len() + 2 * ends.len() + 4);
    form::put_array(&mut out, &ends, &data)?;
    Ok(out)
}

/// An object of these keys and values' bytes (the last of equal keys
/// kept).
pub fn object_of<K: AsRef<str>, B: AsRef<[u8]>>(items: Vec<(K, B)>) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut pairs = Vec::with_capacity(items.len());
    for (k, v) in items {
        let start = data.len();
        data.extend_from_slice(v.as_ref());
        pairs.push((k, start..data.len()));
    }
    let mut out = Vec::with_capacity(data.len() + 4 * pairs.len() + 4);
    form::put_object(&mut out, &mut pairs, &data)?;
    Ok(out)
}

fn elements(b: &[u8]) -> Result<Vec<&[u8]>> {
    match form::root(b)? {
        Node::Array(a) => (0..a.len()).map(|i| a.bytes(i)).collect(),
        _ => Err(form::malformed()),
    }
}

fn fields(b: &[u8]) -> Result<Vec<(Cow<'_, str>, &[u8])>> {
    match form::root(b)? {
        Node::Object(o) => (0..o.len())
            .map(|i| Ok((Cow::Borrowed(o.key(i)?), o.value_bytes(i)?)))
            .collect(),
        _ => Err(form::malformed()),
    }
}

/// A string value.
pub fn string(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 2);
    form::put_str(&mut out, s);
    out
}

fn kind_of(b: &[u8]) -> Result<u8> {
    b.first().map(|h| h >> 4).ok_or_else(form::malformed)
}

const ARR: u8 = 6;
const OBJ: u8 = 7;

/// `a || b`: objects merged (`b`'s keys win), arrays concatenated, a
/// non-array appended to an array or put before one; two other values
/// make an array of both.
pub fn concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
    match (kind_of(a)?, kind_of(b)?) {
        (OBJ, OBJ) => {
            let mut f = fields(a)?;
            f.extend(fields(b)?);
            object_of(f)
        }
        (ARR, ARR) => {
            let mut e = elements(a)?;
            e.extend(elements(b)?);
            array_of(&e)
        }
        (ARR, _) => {
            let mut e = elements(a)?;
            e.push(b);
            array_of(&e)
        }
        (_, ARR) => {
            let mut e = vec![a];
            e.extend(elements(b)?);
            array_of(&e)
        }
        _ => array_of(&[a, b]),
    }
}

fn from_scalar_error() -> Error {
    Error::invalid_parameter("cannot delete from scalar")
}

/// `j - keys`: an object without these keys, an array without the strings
/// equal to them.
pub fn delete_keys(b: &[u8], keys: &[&str]) -> Result<Vec<u8>> {
    match form::root(b)? {
        Node::Object(o) => {
            let mut kept = Vec::with_capacity(o.len());
            for i in 0..o.len() {
                let k = o.key(i)?;
                if !keys.contains(&k) {
                    kept.push((k, o.value_bytes(i)?));
                }
            }
            if kept.len() == o.len() {
                return Ok(b.to_vec());
            }
            object_of(kept)
        }
        Node::Array(a) => {
            let mut kept = Vec::with_capacity(a.len());
            for i in 0..a.len() {
                let e = a.bytes(i)?;
                if let Node::Str(s) = form::root(e)?
                    && keys.iter().any(|k| k.as_bytes() == s)
                {
                    continue;
                }
                kept.push(e);
            }
            array_of(&kept)
        }
        _ => Err(from_scalar_error()),
    }
}

/// `j - i`: an array without element `i` (from the end when negative).
pub fn delete_index(b: &[u8], i: i64) -> Result<Vec<u8>> {
    match form::root(b)? {
        Node::Array(a) => {
            let Some(i) = index(i, a.len()) else {
                return Ok(b.to_vec());
            };
            let mut e = elements(b)?;
            e.remove(i);
            array_of(&e)
        }
        Node::Object(_) => Err(Error::invalid_parameter(
            "cannot delete from object using integer index",
        )),
        _ => Err(from_scalar_error()),
    }
}

/// A path step as an array position, or the error PostgreSQL gives.
fn step_index(step: &str, level: usize) -> Result<i64> {
    step.trim()
        .parse::<i64>()
        .ok()
        .filter(|i| i32::try_from(*i).is_ok())
        .ok_or_else(|| {
            Error::invalid_text(format!(
                "path element at position {} is not an integer: \"{step}\"",
                level + 1
            ))
        })
}

fn path_step(path: &[Option<String>], level: usize) -> Result<&str> {
    path[level]
        .as_deref()
        .ok_or_else(|| Error::null_value(format!("path element at position {} is null", level + 1)))
}

/// `j #- path`: without the value at the path (unchanged where the path
/// leads nowhere).
pub fn delete_path(b: &[u8], path: &[Option<String>]) -> Result<Vec<u8>> {
    if !is_container_bytes(b) {
        return Err(Error::invalid_parameter("cannot delete path in scalar"));
    }
    if path.is_empty() {
        return Ok(b.to_vec());
    }
    Ok(edit(b, path, 0, &Edit::Delete)?.unwrap_or_else(|| b.to_vec()))
}

/// What [`edit`] does at the path's end.
enum Edit<'a> {
    Delete,
    /// `json_set`: replace, or add when `create`.
    Set {
        new: &'a [u8],
        create: bool,
    },
    /// `json_insert`: add before the position, or after it.
    Insert {
        new: &'a [u8],
        after: bool,
    },
}

/// `b` changed at `path[level..]`: `None` when nothing changed.
fn edit(
    b: &[u8],
    path: &[Option<String>],
    level: usize,
    what: &Edit<'_>,
) -> Result<Option<Vec<u8>>> {
    if level > super::MAX_DEPTH {
        return Err(super::too_deep());
    }
    let last = level + 1 == path.len();
    match form::root(b)? {
        Node::Object(o) => {
            let key = path_step(path, level)?;
            let found = o.find(key)?;
            let mut f = fields(b)?;
            match (found, last, what) {
                (Some(i), false, _) => {
                    let v = o.value_bytes(i)?;
                    // The next step is read as it is reached (a `NULL` one
                    // refused even before a scalar), as PostgreSQL does.
                    path_step(path, level + 1)?;
                    if !is_container_bytes(v) {
                        return Ok(None);
                    }
                    let Some(changed) = edit(v, path, level + 1, what)? else {
                        return Ok(None);
                    };
                    f[i].1 = &[];
                    let mut owned: Vec<(Cow<'_, str>, Vec<u8>)> =
                        f.into_iter().map(|(k, v)| (k, v.to_vec())).collect();
                    owned[i].1 = changed;
                    object_of(owned).map(Some)
                }
                (None, false, _) => Ok(None),
                (Some(i), true, Edit::Delete) => {
                    f.remove(i);
                    object_of(f).map(Some)
                }
                (None, true, Edit::Delete) => Ok(None),
                (Some(i), true, Edit::Set { new, .. }) => {
                    f[i].1 = new;
                    object_of(f).map(Some)
                }
                (None, true, Edit::Set { new, create }) => {
                    if !create {
                        return Ok(None);
                    }
                    f.push((Cow::Owned(key.to_string()), new));
                    object_of(f).map(Some)
                }
                (Some(_), true, Edit::Insert { .. }) => {
                    Err(Error::invalid_parameter("cannot replace existing key")
                        .with_hint("Try using the function jsonb_set to replace key value."))
                }
                (None, true, Edit::Insert { new, .. }) => {
                    f.push((Cow::Owned(key.to_string()), new));
                    object_of(f).map(Some)
                }
            }
        }
        Node::Array(a) => {
            let i = step_index(path_step(path, level)?, level)?;
            let len = a.len();
            let pos = index(i, len);
            let mut e = elements(b)?;
            if !last {
                let Some(p) = pos else { return Ok(None) };
                let v = e[p];
                path_step(path, level + 1)?;
                if !is_container_bytes(v) {
                    return Ok(None);
                }
                let Some(changed) = edit(v, path, level + 1, what)? else {
                    return Ok(None);
                };
                let mut owned: Vec<Vec<u8>> = e.iter().map(|x| x.to_vec()).collect();
                owned[p] = changed;
                return array_of(&owned).map(Some);
            }
            // Out of range: before the first (negative) or past the last.
            let before = i < 0;
            match (pos, what) {
                (Some(p), Edit::Delete) => {
                    e.remove(p);
                    array_of(&e).map(Some)
                }
                (None, Edit::Delete) => Ok(None),
                (Some(p), Edit::Set { new, .. }) => {
                    e[p] = new;
                    array_of(&e).map(Some)
                }
                (None, Edit::Set { new, create }) => {
                    if !create {
                        return Ok(None);
                    }
                    if before {
                        e.insert(0, new);
                    } else {
                        e.push(new);
                    }
                    array_of(&e).map(Some)
                }
                (Some(p), Edit::Insert { new, after }) => {
                    e.insert(if *after { p + 1 } else { p }, new);
                    array_of(&e).map(Some)
                }
                (None, Edit::Insert { new, .. }) => {
                    if before {
                        e.insert(0, new);
                    } else {
                        e.push(new);
                    }
                    array_of(&e).map(Some)
                }
            }
        }
        _ => Ok(None),
    }
}

/// `json_set(j, path, new, create)`.
pub fn set(b: &[u8], path: &[Option<String>], new: &[u8], create: bool) -> Result<Vec<u8>> {
    if !is_container_bytes(b) {
        return Err(Error::invalid_parameter("cannot set path in scalar"));
    }
    if path.is_empty() {
        return Ok(b.to_vec());
    }
    Ok(edit(b, path, 0, &Edit::Set { new, create })?.unwrap_or_else(|| b.to_vec()))
}

/// `json_insert(j, path, new, after)`.
pub fn insert(b: &[u8], path: &[Option<String>], new: &[u8], after: bool) -> Result<Vec<u8>> {
    if !is_container_bytes(b) {
        return Err(Error::invalid_parameter("cannot set path in scalar"));
    }
    if path.is_empty() {
        return Ok(b.to_vec());
    }
    Ok(edit(b, path, 0, &Edit::Insert { new, after })?.unwrap_or_else(|| b.to_vec()))
}

/// `json_strip_nulls(j, in_arrays)`: without object fields that are
/// `null`, at any depth — and array elements that are, if asked.
pub fn strip_nulls(b: &[u8], in_arrays: bool) -> Result<Vec<u8>> {
    strip(b, in_arrays, 0)
}

fn strip(b: &[u8], in_arrays: bool, depth: usize) -> Result<Vec<u8>> {
    if depth > super::MAX_DEPTH {
        return Err(super::too_deep());
    }
    const NULL: u8 = 0;
    match kind_of(b)? {
        OBJ => {
            let mut kept = Vec::new();
            for (k, v) in fields(b)? {
                if kind_of(v)? != NULL {
                    kept.push((k, strip(v, in_arrays, depth + 1)?));
                }
            }
            object_of(kept)
        }
        ARR => {
            let mut kept = Vec::new();
            for v in elements(b)? {
                if !(in_arrays && kind_of(v)? == NULL) {
                    kept.push(strip(v, in_arrays, depth + 1)?);
                }
            }
            array_of(&kept)
        }
        _ => Ok(b.to_vec()),
    }
}

// ------------------------------------------------------------- from SQL

/// A SQL value as JSON (`to_json`): numbers as numbers (a float's
/// non-finite values as strings, as PostgreSQL's), booleans, `NULL` as
/// `null`, arrays as arrays, a `json` value as itself, times in ISO 8601
/// (`2024-01-01T10:00:00`), anything else as the string of its text.
pub fn from_value(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    put_value(v, &mut out, 0)?;
    Ok(out)
}

fn put_value(v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > super::MAX_DEPTH {
        return Err(super::too_deep());
    }
    match v {
        Value::Null => form::put_null(out),
        Value::Bool(b) => form::put_bool(out, *b),
        Value::Int(i) => form::put_int(out, *i),
        Value::Float(f) => put_float(out, *f, format!("{f:e}"))?,
        Value::Real(f) => put_float(out, f64::from(*f), format!("{f:e}"))?,
        Value::Numeric(_) => out.extend_from_slice(&super::parse::parse(&v.render())?),
        Value::Array(items) => {
            let mut data = Vec::new();
            let mut ends = Vec::with_capacity(items.len());
            for it in items {
                put_value(it, &mut data, depth + 1)?;
                ends.push(data.len());
            }
            form::put_array(out, &ends, &data)?;
        }
        Value::Ext(t, b) if t.name() == "json" => out.extend_from_slice(b),
        Value::Timestamp(_) => form::put_str(out, &v.render().replacen(' ', "T", 1)),
        Value::TimestampTz(_) => {
            let mut s = v.render().replacen(' ', "T", 1);
            // `+00` → `+00:00`, as ISO 8601 writes an offset.
            let n = s.len();
            if n >= 3 && matches!(&s.as_bytes()[n - 3], b'+' | b'-') {
                s.push_str(":00");
            }
            form::put_str(out, &s);
        }
        _ => form::put_str(out, &v.render()),
    }
    Ok(())
}

fn put_float(out: &mut Vec<u8>, f: f64, shortest: String) -> Result<()> {
    if f.is_nan() {
        form::put_str(out, "NaN");
    } else if f.is_infinite() {
        form::put_str(out, if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        out.extend_from_slice(&super::parse::parse(&shortest)?);
    }
    Ok(())
}

/// A key of `json_build_object` / `json_object_agg`: a scalar's text (a
/// string's own, another's JSON text); arrays and `json` refused.
pub fn key_text(v: &Value) -> Result<String> {
    if matches!(v, Value::Array(_)) || matches!(v, Value::Ext(t, _) if t.name() == "json") {
        return Err(Error::invalid_parameter(
            "key value must be scalar, not array, composite, or json",
        ));
    }
    let b = from_value(v)?;
    match form::root(&b)? {
        Node::Str(s) => Ok(form::text_of(s)?.to_string()),
        _ => super::to_text(&b),
    }
}
