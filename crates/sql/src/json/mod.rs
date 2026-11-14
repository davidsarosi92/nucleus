//! `json` (PLAN.md §M14.15): JSON values, validated and kept in a binary
//! form ([`form`]) read in place — one type, `jsonb` another name for it.
//!
//! A value is [`Value::Ext`] with its canonical bytes: equal documents
//! (whatever their spacing, key order or duplicate keys) have equal bytes.
//! Numbers keep their value exactly, at any size within `numeric`'s
//! bounds, but not how they were written: `1.50` reads back `1.5`, `1e2`
//! `100` (PostgreSQL's `jsonb` keeps the scale).  Its order is
//! PostgreSQL's ([`key`]).  To drivers it travels as text — every language
//! parses JSON — and they hand out their own values (a `dict`, an object).

pub mod form;
mod funcs;
pub mod key;
pub mod ops;
pub mod parse;
pub mod path;
pub(crate) mod sqljson;
pub mod text;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    CanonicalBytes, Equality, Hashing, KeyEncoding, Registry, TotalOrder, Type, TypeRef,
};
use crate::types::{DataType, Value};

pub use form::Node;

/// How deeply values may nest: the parser, the output and the key
/// encoding recurse once a level (a browser's stack is the smallest).
pub const MAX_DEPTH: usize = if cfg!(target_family = "wasm") {
    256
} else {
    1000
};

pub(crate) fn too_deep() -> Error {
    Error::statement_too_complex(format!(
        "a json value nests too deeply (more than {MAX_DEPTH} levels)"
    ))
}

/// The `json` type.
pub struct Json;

/// The interned `json` type (the built-in module registers it).
pub fn json_type() -> TypeRef {
    TypeRef::named("json").expect("the built-in module registers json")
}

/// The `json` column type.
pub fn data_type() -> DataType {
    DataType::Ext(json_type())
}

/// A value from its binary form (canonical: [`form`]'s writers made it).
pub fn value(bytes: Vec<u8>) -> Value {
    Value::ext(json_type(), bytes)
}

/// A json value's binary form.
pub fn bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Ext(t, b) if t.name() == "json" => Ok(b),
        _ => Err(Error::internal("not a json value")),
    }
}

/// A value from JSON text.
pub fn from_text(text: &str) -> Result<Value> {
    Ok(value(parse::parse(text)?))
}

/// A value's text (PostgreSQL's layout: `{"a": 1, "b": [1, 2]}`).
pub fn to_text(b: &[u8]) -> Result<String> {
    let mut s = Vec::with_capacity(b.len() + b.len() / 2);
    text::write_bytes(form::root(b)?, &mut s)?;
    String::from_utf8(s).map_err(|_| form::malformed())
}

/// A value's text, indented.
pub fn to_pretty(b: &[u8]) -> Result<String> {
    let mut s = String::with_capacity(2 * b.len());
    text::pretty(form::root(b)?, &mut s, 0)?;
    Ok(s)
}

struct Order;

impl TotalOrder for Order {
    fn compare(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        key::compare(bytes(a)?, bytes(b)?)
    }
}

struct Key;

impl KeyEncoding for Key {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        if let Ok(n) = bytes(v).and_then(form::root) {
            // A stored value is well formed; a malformed one encodes as
            // null rather than panicking.
            if key::encode(n, out).is_ok() {
                return;
            }
        }
        out.push(0x01);
    }

    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        let (v, n) = key::decode(b)?;
        Ok((value(v), n))
    }
}

/// The name of a node's kind, as `json_typeof` says it.
pub fn kind(n: &Node<'_>) -> &'static str {
    match n {
        Node::Null => "null",
        Node::Bool(_) => "boolean",
        Node::Int(_) | Node::Dec(_) => "number",
        Node::Str(_) => "string",
        Node::Array(_) => "array",
        Node::Object(_) => "object",
    }
}

/// A number node as `numeric` (an error past its 38 digits).
pub fn to_numeric(n: &Node<'_>) -> Option<Result<crate::decimal::Decimal>> {
    match n {
        Node::Int(i) => Some(Ok(crate::decimal::Decimal::from_i64(*i))),
        Node::Dec(d) => {
            let digits = d.digits();
            Some(crate::decimal::Decimal::from_digits(
                d.negative,
                &digits,
                d.exp + d.len as i64 - 1,
            ))
        }
        _ => None,
    }
}

/// A number node as a double.
pub fn to_f64(n: &Node<'_>) -> Option<f64> {
    match n {
        Node::Int(i) => Some(*i as f64),
        Node::Dec(d) => {
            let mut s = String::new();
            text::write_dec(*d, &mut s);
            s.parse().ok()
        }
        _ => None,
    }
}

fn cast_to(v: &Value, to: DataType) -> Option<Result<Value>> {
    let n = match bytes(v).and_then(form::root) {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    let refuse = |n: &Node<'_>| {
        Err(Error::invalid_parameter(format!(
            "cannot cast json {} to type {}",
            kind(n),
            to.name()
        )))
    };
    Some(match to {
        DataType::Bool => match n {
            Node::Bool(b) => Ok(Value::Bool(b)),
            other => refuse(&other),
        },
        DataType::SmallInt | DataType::Integer | DataType::BigInt | DataType::Numeric => match n {
            Node::Int(i) => Value::Int(i).cast(to),
            Node::Dec(_) => to_numeric(&n)
                .expect("a number")
                .and_then(|d| Value::Numeric(d).cast(to)),
            other => refuse(&other),
        },
        DataType::Real | DataType::Float => match to_f64(&n) {
            Some(f) => Value::Float(f).cast(to),
            None => refuse(&n),
        },
        _ => return None,
    })
}

impl Type for Json {
    fn name(&self) -> &str {
        "json"
    }
    fn aliases(&self) -> &[&str] {
        &["jsonb"]
    }
    fn category(&self) -> &str {
        "json"
    }
    fn input(&self, text: &str) -> Result<Value> {
        from_text(text)
    }
    fn output(&self, v: &Value) -> String {
        bytes(v).and_then(to_text).unwrap_or_else(|_| "?".into())
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&Order)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Key)
    }
    fn cast_to(&self, v: &Value, to: DataType) -> Option<Result<Value>> {
        cast_to(v, to)
    }
    fn arithmetic(&self) -> Option<&dyn crate::ext::Arithmetic> {
        Some(&funcs::Arith)
    }
    fn wire_output(&self, v: &Value) -> Option<Vec<u8>> {
        Some(self.output(v).into_bytes())
    }
    fn wire_input(&self, b: &[u8]) -> Option<Result<Value>> {
        Some(match std::str::from_utf8(b) {
            Ok(s) => from_text(s),
            Err(_) => Err(Error::invalid_text(
                "invalid input syntax for type json: not UTF-8",
            )),
        })
    }
    fn examples(&self) -> Vec<Value> {
        [
            "null",
            "true",
            "false",
            "0",
            "-1",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "1.5",
            "-0.001",
            "1e300",
            "\"\"",
            "\"a\"",
            "\"ab\"",
            "[]",
            "[1, 2]",
            "[2]",
            "{}",
            "{\"a\": 1}",
            "{\"a\": 1, \"b\": [true, null, {\"c\": \"d\"}]}",
        ]
        .iter()
        .map(|s| from_text(s).expect("valid"))
        .collect()
    }
}

/// Registers the type and its functions.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(Json))?;
    funcs::register(r)?;
    path::register(r)?;
    sqljson::register(r)
}

#[cfg(test)]
mod tests;
