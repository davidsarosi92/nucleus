//! SQL data types and values.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::error::{Error, Result};

/// A column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// `BOOLEAN`.
    Bool,
    /// `BIGINT` (`INTEGER`, `INT`, `SMALLINT` are the same 64-bit type).
    Int,
    /// `DOUBLE PRECISION` (`REAL`, `FLOAT`).
    Float,
    /// `TEXT` / `VARCHAR`.
    Text,
}

impl DataType {
    /// The SQL name.
    pub fn name(self) -> &'static str {
        match self {
            DataType::Bool => "boolean",
            DataType::Int => "bigint",
            DataType::Float => "double precision",
            DataType::Text => "text",
        }
    }

    /// Parses a type name (lowercase words, e.g. `["double", "precision"]`).
    pub fn parse(words: &[&str]) -> Option<Self> {
        Some(match words {
            ["boolean"] | ["bool"] => DataType::Bool,
            ["bigint"] | ["int"] | ["integer"] | ["smallint"] | ["int8"] | ["int4"] | ["int2"] => {
                DataType::Int
            }
            ["double", "precision"]
            | ["float"]
            | ["real"]
            | ["float8"]
            | ["float4"]
            | ["double"] => DataType::Float,
            ["text"]
            | ["varchar"]
            | ["character", "varying"]
            | ["char"]
            | ["character"]
            | ["string"] => DataType::Text,
            _ => return None,
        })
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A value.
#[derive(Clone, Debug)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// A string.
    Text(String),
}

impl Value {
    /// The value's type (`None` for `NULL`).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(DataType::Bool),
            Value::Int(_) => Some(DataType::Int),
            Value::Float(_) => Some(DataType::Float),
            Value::Text(_) => Some(DataType::Text),
        }
    }

    /// Whether this is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// SQL comparison: `None` if either side is `NULL`; numbers compare
    /// across `Int` and `Float`; other mixed types are an error.
    pub fn sql_cmp(&self, other: &Value) -> Result<Option<Ordering>> {
        Ok(Some(match (self, other) {
            (Value::Null, _) | (_, Value::Null) => return Ok(None),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => total_cmp(*a, *b),
            (Value::Int(a), Value::Float(b)) => total_cmp(*a as f64, *b),
            (Value::Float(a), Value::Int(b)) => total_cmp(*a, *b as f64),
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (a, b) => {
                return Err(Error::type_mismatch(format!(
                    "cannot compare {} with {}",
                    type_name(a),
                    type_name(b)
                )));
            }
        }))
    }

    /// The order used by `ORDER BY`, `DISTINCT` and grouping: `NULL` sorts
    /// after every value (PostgreSQL's default for ascending order).
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            _ => self
                .sql_cmp(other)
                .ok()
                .flatten()
                .unwrap_or_else(|| rank(self).cmp(&rank(other))),
        }
    }

    /// Converts to `ty` (a SQL `CAST`).
    pub fn cast(self, ty: DataType) -> Result<Value> {
        let bad = |v: &Value| Error::invalid_text(format!("cannot cast {} to {ty}", v.render()));
        Ok(match (self, ty) {
            (Value::Null, _) => Value::Null,
            (v @ Value::Bool(_), DataType::Bool)
            | (v @ Value::Int(_), DataType::Int)
            | (v @ Value::Float(_), DataType::Float)
            | (v @ Value::Text(_), DataType::Text) => v,
            (Value::Int(i), DataType::Float) => Value::Float(i as f64),
            (Value::Float(f), DataType::Int) => {
                let r = f.round();
                if !r.is_finite() || r < i64::MIN as f64 || r >= i64::MAX as f64 {
                    return Err(Error::out_of_range("bigint out of range"));
                }
                Value::Int(r as i64)
            }
            (Value::Int(i), DataType::Bool) => Value::Bool(i != 0),
            (Value::Bool(b), DataType::Int) => Value::Int(i64::from(b)),
            // As PostgreSQL: a boolean cast to text is 'true'/'false' (the
            // wire format's 't'/'f' is only how it is sent).
            (Value::Bool(b), DataType::Text) => {
                Value::Text(if b { "true" } else { "false" }.into())
            }
            (v, DataType::Text) => Value::Text(v.render()),
            (Value::Text(s), DataType::Int) => Value::Int(s.trim().parse().map_err(|_| {
                Error::invalid_text(format!("invalid input syntax for type bigint: {s:?}"))
            })?),
            (Value::Text(s), DataType::Float) => Value::Float(s.trim().parse().map_err(|_| {
                Error::invalid_text(format!(
                    "invalid input syntax for type double precision: {s:?}"
                ))
            })?),
            (Value::Text(s), DataType::Bool) => match s.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "yes" | "y" | "on" | "1" => Value::Bool(true),
                "f" | "false" | "no" | "n" | "off" | "0" => Value::Bool(false),
                _ => {
                    return Err(Error::invalid_text(format!(
                        "invalid input syntax for type boolean: {s:?}"
                    )));
                }
            },
            (v, _) => return Err(bad(&v)),
        })
    }

    /// The value as text, the way PostgreSQL prints it (`NULL` as `NULL`).
    pub fn render(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bool(b) => if *b { "t" } else { "f" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => render_float(*f),
            Value::Text(s) => s.clone(),
        }
    }

    /// `true`, `false` or `NULL` as a condition; errors for non-booleans.
    pub fn truth(&self) -> Result<Option<bool>> {
        match self {
            Value::Null => Ok(None),
            Value::Bool(b) => Ok(Some(*b)),
            v => Err(Error::type_mismatch(format!(
                "argument must be boolean, not {}",
                type_name(v)
            ))),
        }
    }
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 5,
        Value::Bool(_) => 0,
        Value::Int(_) | Value::Float(_) => 1,
        Value::Text(_) => 2,
    }
}

/// A total order on floats with `NaN` greatest and `-0 == 0`
/// (PostgreSQL's).
pub fn total_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).expect("not NaN"),
    }
}

fn render_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if f == 0.0 {
        "0".into() // -0 prints as 0
    } else {
        // Shortest representation that round-trips, like PostgreSQL 12+.
        let s = format!("{f}");
        if s.contains('e') || s.len() < 18 {
            s
        } else {
            format!("{f:e}")
        }
    }
}

/// The type name of a value, for messages.
pub fn type_name(v: &Value) -> &'static str {
    v.data_type().map_or("unknown", DataType::name)
}

impl PartialEq for Value {
    /// Equality for grouping and `DISTINCT`: `NULL` equals `NULL`, `1 = 1.0`.
    fn eq(&self, other: &Self) -> bool {
        self.sort_cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Consistent with `eq`: numbers that compare equal hash equally.
        match self {
            Value::Null => 0u8.hash(state),
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => hash_number(*i as f64, state),
            Value::Float(f) => hash_number(*f, state),
            Value::Text(s) => s.hash(state),
        }
    }
}

fn hash_number<H: Hasher>(f: f64, state: &mut H) {
    1u8.hash(state);
    let f = if f == 0.0 { 0.0 } else { f }; // -0 == 0
    if f.is_nan() {
        u64::MAX.hash(state)
    } else {
        f.to_bits().hash(state)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// A row: one value per column.
pub type Row = Vec<Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons() {
        assert_eq!(
            Value::Int(1).sql_cmp(&Value::Float(1.5)).unwrap(),
            Some(Ordering::Less)
        );
        assert_eq!(Value::Null.sql_cmp(&Value::Int(1)).unwrap(), None);
        assert!(Value::Int(1).sql_cmp(&Value::Text("1".into())).is_err());
        assert_eq!(Value::Null.sort_cmp(&Value::Int(1)), Ordering::Greater);
        assert_eq!(Value::Int(1), Value::Float(1.0));
    }

    #[test]
    fn casts() {
        assert_eq!(
            Value::Text(" 42 ".into()).cast(DataType::Int).unwrap(),
            Value::Int(42)
        );
        assert_eq!(
            Value::Float(2.5).cast(DataType::Int).unwrap(),
            Value::Int(3)
        );
        assert!(Value::Text("x".into()).cast(DataType::Int).is_err());
        assert_eq!(
            Value::Text("on".into()).cast(DataType::Bool).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            Value::Float(0.1).cast(DataType::Text).unwrap(),
            Value::Text("0.1".into())
        );
    }

    #[test]
    fn rendering() {
        assert_eq!(Value::Float(-0.0).render(), "0");
        assert_eq!(Value::Float(1e300).render(), "1e300");
        assert_eq!(Value::Float(2.5).render(), "2.5");
        assert_eq!(Value::Bool(true).render(), "t");
    }
}
