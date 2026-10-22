//! SQL data types and values.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use std::sync::Arc;

use crate::datetime::{self, Interval};
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::ext::TypeRef;

/// A column type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// `BOOLEAN`.
    Bool,
    /// `SMALLINT` (`INT2`): 16-bit.
    SmallInt,
    /// `INTEGER` (`INT`, `INT4`): 32-bit.
    Integer,
    /// `BIGINT` (`INT8`): 64-bit.
    BigInt,
    /// `REAL` (`FLOAT4`): 32-bit IEEE 754.
    Real,
    /// `DOUBLE PRECISION` (`FLOAT`, `FLOAT8`): 64-bit IEEE 754.
    Float,
    /// `TEXT` / `VARCHAR`.
    Text,
    /// `BYTEA`: a binary string.
    Bytea,
    /// `CHAR(n)` / `CHARACTER(n)`: blank-padded text.
    Char,
    /// `NUMERIC` / `DECIMAL`: exact.
    Numeric,
    /// `DATE`.
    Date,
    /// `TIME [WITHOUT TIME ZONE]`: a time of day.
    Time,
    /// `TIMESTAMP [WITHOUT TIME ZONE]`.
    Timestamp,
    /// `TIMESTAMPTZ` / `TIMESTAMP WITH TIME ZONE`.
    TimestampTz,
    /// `INTERVAL`.
    Interval,
    /// `INET`: an IPv4 or IPv6 address with an optional mask.
    Inet,
    /// `CIDR`: an IPv4 or IPv6 network.
    Cidr,
    /// `type[]`: a one-dimensional array of the element type.
    Array(&'static DataType),
    /// A registered type (a module's, or `uuid`): see [`crate::ext::Type`].
    Ext(TypeRef),
    /// An enum of the database's ([`crate::enums`]).
    Enum(crate::enums::EnumRef),
}

impl DataType {
    /// The array type of a scalar type (`None` for an array).
    pub fn array_of(self) -> Option<DataType> {
        Some(DataType::Array(match self {
            DataType::Bool => &DataType::Bool,
            DataType::SmallInt => &DataType::SmallInt,
            DataType::Integer => &DataType::Integer,
            DataType::BigInt => &DataType::BigInt,
            DataType::Real => &DataType::Real,
            DataType::Float => &DataType::Float,
            DataType::Text => &DataType::Text,
            DataType::Bytea => &DataType::Bytea,
            DataType::Char => &DataType::Char,
            DataType::Numeric => &DataType::Numeric,
            DataType::Date => &DataType::Date,
            DataType::Time => &DataType::Time,
            DataType::Timestamp => &DataType::Timestamp,
            DataType::TimestampTz => &DataType::TimestampTz,
            DataType::Interval => &DataType::Interval,
            DataType::Inet => &DataType::Inet,
            DataType::Cidr => &DataType::Cidr,
            // Arrays of registered types and enums: not yet.
            DataType::Array(_) | DataType::Ext(_) | DataType::Enum(_) => return None,
        }))
    }

    /// The element type of an array type.
    pub fn element(self) -> Option<DataType> {
        match self {
            DataType::Array(e) => Some(*e),
            _ => None,
        }
    }

    /// The SQL name.
    pub fn name(self) -> &'static str {
        match self {
            DataType::Bool => "boolean",
            DataType::SmallInt => "smallint",
            DataType::Integer => "integer",
            DataType::BigInt => "bigint",
            DataType::Real => "real",
            DataType::Float => "double precision",
            DataType::Text => "text",
            DataType::Bytea => "bytea",
            DataType::Char => "character",
            DataType::Numeric => "numeric",
            DataType::Date => "date",
            DataType::Time => "time without time zone",
            DataType::Timestamp => "timestamp without time zone",
            DataType::TimestampTz => "timestamp with time zone",
            DataType::Interval => "interval",
            DataType::Inet => "inet",
            DataType::Cidr => "cidr",
            DataType::Array(e) => match e {
                DataType::Bool => "boolean[]",
                DataType::SmallInt => "smallint[]",
                DataType::Integer => "integer[]",
                DataType::BigInt => "bigint[]",
                DataType::Real => "real[]",
                DataType::Float => "double precision[]",
                DataType::Text => "text[]",
                DataType::Bytea => "bytea[]",
                DataType::Char => "character[]",
                DataType::Numeric => "numeric[]",
                DataType::Date => "date[]",
                DataType::Time => "time without time zone[]",
                DataType::Timestamp => "timestamp without time zone[]",
                DataType::TimestampTz => "timestamp with time zone[]",
                DataType::Interval => "interval[]",
                DataType::Inet => "inet[]",
                DataType::Cidr => "cidr[]",
                DataType::Array(_) | DataType::Ext(_) | DataType::Enum(_) => "array",
            },
            DataType::Ext(t) => t.name(),
            DataType::Enum(e) => e.name(),
        }
    }

    /// Whether this is an integer, `double precision` or `numeric`.
    pub fn is_number(self) -> bool {
        self.is_integer() || matches!(self, DataType::Real | DataType::Float | DataType::Numeric)
    }

    /// Whether this is `inet` or `cidr`.
    pub fn is_net(self) -> bool {
        matches!(self, DataType::Inet | DataType::Cidr)
    }

    /// Whether this is `text` or `char(n)`.
    pub fn is_text(self) -> bool {
        matches!(self, DataType::Text | DataType::Char)
    }

    /// Whether this is `smallint`, `integer` or `bigint`.
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            DataType::SmallInt | DataType::Integer | DataType::BigInt
        )
    }

    /// An integer type's range (`None` for other types).
    pub fn int_range(self) -> Option<(i64, i64)> {
        Some(match self {
            DataType::SmallInt => (i16::MIN.into(), i16::MAX.into()),
            DataType::Integer => (i32::MIN.into(), i32::MAX.into()),
            DataType::BigInt => (i64::MIN, i64::MAX),
            _ => return None,
        })
    }

    /// The type of an integer literal: `integer` if it fits, else `bigint`
    /// (PostgreSQL's rule; a larger one is `numeric`).
    pub fn of_int_literal(i: i64) -> DataType {
        if i32::try_from(i).is_ok() {
            DataType::Integer
        } else {
            DataType::BigInt
        }
    }

    /// Whether this is `date`, `timestamp` or `timestamptz`.
    pub fn is_datetime(self) -> bool {
        matches!(
            self,
            DataType::Date | DataType::Timestamp | DataType::TimestampTz
        )
    }

    /// Whether `words` begin a type name (for parsing multi-word names).
    pub fn is_prefix(words: &[&str]) -> bool {
        Self::parse(words).is_some()
            || matches!(
                words,
                ["double"]
                    | ["character"]
                    | ["timestamp", "with"]
                    | ["timestamp", "with", "time"]
                    | ["timestamp", "without"]
                    | ["timestamp", "without", "time"]
            )
    }

    /// Parses a type name (lowercase words, e.g. `["double", "precision"]`).
    pub fn parse(words: &[&str]) -> Option<Self> {
        Some(match words {
            ["boolean"] | ["bool"] => DataType::Bool,
            ["smallint"] | ["int2"] => DataType::SmallInt,
            ["integer"] | ["int"] | ["int4"] => DataType::Integer,
            ["bigint"] | ["int8"] => DataType::BigInt,
            ["real"] | ["float4"] => DataType::Real,
            ["double", "precision"] | ["float"] | ["float8"] | ["double"] => DataType::Float,
            ["bytea"] => DataType::Bytea,
            ["text"] | ["varchar"] | ["character", "varying"] | ["string"] => DataType::Text,
            ["char"] | ["character"] => DataType::Char,
            ["numeric"] | ["decimal"] | ["dec"] => DataType::Numeric,
            ["date"] => DataType::Date,
            ["time"] => DataType::Time,
            ["timestamp"] | ["timestamp", "without", "time", "zone"] => DataType::Timestamp,
            ["timestamptz"] | ["timestamp", "with", "time", "zone"] => DataType::TimestampTz,
            ["interval"] => DataType::Interval,
            ["inet"] => DataType::Inet,
            ["cidr"] => DataType::Cidr,
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
    /// A 32-bit float (`real`).
    Real(f32),
    /// A 64-bit float.
    Float(f64),
    /// A string.
    Text(String),
    /// A binary string (`bytea`).
    Bytea(Arc<[u8]>),
    /// A `char(n)` value, as stored: blank-padded (trailing blanks do not
    /// count in comparisons).
    Char(String),
    /// An exact decimal.
    Numeric(Decimal),
    /// Days since 2000-01-01.
    Date(i32),
    /// Microseconds since midnight (`24:00:00` included).
    Time(i64),
    /// Microseconds since 2000-01-01 00:00.
    Timestamp(i64),
    /// Microseconds since 2000-01-01 00:00 UTC.
    TimestampTz(i64),
    /// An interval.
    Interval(Interval),
    /// An `inet`.
    Inet(crate::inet::Net),
    /// A `cidr` (its host bits zero).
    Cidr(crate::inet::Net),
    /// An array (one dimension; elements of one type or NULL).
    Array(Vec<Value>),
    /// A value of a registered type: the type and the value's canonical
    /// bytes (equal values have equal bytes).
    Ext(TypeRef, Arc<[u8]>),
    /// An enum's value.
    Enum(crate::enums::EnumValue),
}

impl Value {
    /// A value of the registered type `ty` from its canonical bytes.
    pub fn ext(ty: TypeRef, bytes: impl Into<Arc<[u8]>>) -> Value {
        Value::Ext(ty, bytes.into())
    }

    /// A registered type's value's canonical bytes (`None` for others).
    pub fn ext_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Ext(_, b) => Some(b),
            _ => None,
        }
    }

    /// The value's type (`None` for `NULL`).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(DataType::Bool),
            Value::Int(_) => Some(DataType::BigInt),
            Value::Real(_) => Some(DataType::Real),
            Value::Float(_) => Some(DataType::Float),
            Value::Text(_) => Some(DataType::Text),
            Value::Bytea(_) => Some(DataType::Bytea),
            Value::Char(_) => Some(DataType::Char),
            Value::Numeric(_) => Some(DataType::Numeric),
            Value::Date(_) => Some(DataType::Date),
            Value::Time(_) => Some(DataType::Time),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::TimestampTz(_) => Some(DataType::TimestampTz),
            Value::Interval(_) => Some(DataType::Interval),
            Value::Inet(_) => Some(DataType::Inet),
            Value::Cidr(_) => Some(DataType::Cidr),
            Value::Array(items) => items
                .iter()
                .find_map(Value::data_type)
                .unwrap_or(DataType::Text)
                .array_of()
                .or(Some(DataType::Array(&DataType::Text))),
            Value::Ext(t, _) => Some(DataType::Ext(*t)),
            Value::Enum(e) => e.ty.map(DataType::Enum),
        }
    }

    /// A date or timestamp as microseconds since 2000-01-01 (a date at
    /// midnight), for comparing them with each other.
    pub fn as_timestamp(&self) -> Option<i64> {
        match self {
            Value::Date(d) => Some(datetime::date_to_ts(*d)),
            Value::Timestamp(t) | Value::TimestampTz(t) => Some(*t),
            _ => None,
        }
    }

    /// Whether this is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// SQL comparison: `None` if either side is `NULL`; numbers compare
    /// across `bigint`, `double precision` and `numeric` (numeric against
    /// float as floats), dates and timestamps across each other (a date as
    /// its midnight); other mixed types are an error.
    pub fn sql_cmp(&self, other: &Value) -> Result<Option<Ordering>> {
        Ok(Some(match (self, other) {
            (Value::Null, _) | (_, Value::Null) => return Ok(None),
            (Value::Real(a), Value::Real(b)) => total_cmp(f64::from(*a), f64::from(*b)),
            // A real against another number: as a double (it widens exactly).
            (Value::Real(a), b) => return Value::Float(f64::from(*a)).sql_cmp(b),
            (a, Value::Real(b)) => return a.sql_cmp(&Value::Float(f64::from(*b))),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => total_cmp(*a, *b),
            (Value::Int(a), Value::Float(b)) => total_cmp(*a as f64, *b),
            (Value::Float(a), Value::Int(b)) => total_cmp(*a, *b as f64),
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            // char(n) compares without its trailing blanks — with another
            // char, and as text with text (PostgreSQL's cast).
            (Value::Char(a) | Value::Text(a), Value::Char(b))
            | (Value::Char(a), Value::Text(b)) => {
                let side = |v: &Value, s: &str| -> usize {
                    if matches!(v, Value::Char(_)) {
                        unpadded(s).len()
                    } else {
                        s.len()
                    }
                };
                a.as_bytes()[..side(self, a)].cmp(&b.as_bytes()[..side(other, b)])
            }
            (Value::Bytea(a), Value::Bytea(b)) => a.cmp(b),
            (Value::Time(a), Value::Time(b)) => a.cmp(b),
            // By declaration (the sort keys), within one enum.
            (Value::Enum(a), Value::Enum(b))
                if a.ty == b.ty || a.ty.is_none() || b.ty.is_none() =>
            {
                a.key.cmp(&b.key)
            }
            (Value::Inet(a) | Value::Cidr(a), Value::Inet(b) | Value::Cidr(b)) => a.cmp_net(*b),
            (Value::Numeric(a), Value::Numeric(b)) => a.cmp(b),
            (Value::Numeric(a), Value::Int(b)) => a.cmp(&Decimal::from_i64(*b)),
            (Value::Int(a), Value::Numeric(b)) => Decimal::from_i64(*a).cmp(b),
            (Value::Numeric(a), Value::Float(b)) => total_cmp(a.to_f64(), *b),
            (Value::Float(a), Value::Numeric(b)) => total_cmp(*a, b.to_f64()),
            (Value::Interval(a), Value::Interval(b)) => a.cmp(b),
            (Value::Array(a), Value::Array(b)) => {
                // Element by element (a NULL element after the others),
                // then the shorter first: PostgreSQL's array order.
                for (x, y) in a.iter().zip(b) {
                    let o = match (x.is_null(), y.is_null()) {
                        (true, true) => Ordering::Equal,
                        (true, false) => Ordering::Greater,
                        (false, true) => Ordering::Less,
                        _ => x.sql_cmp(y)?.expect("not null"),
                    };
                    if o != Ordering::Equal {
                        return Ok(Some(o));
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) if a.as_timestamp().is_some() && b.as_timestamp().is_some() => {
                a.as_timestamp().cmp(&b.as_timestamp())
            }
            (Value::Ext(t, x), Value::Ext(u, y)) if t == u => match t.imp().ordering() {
                Some(o) => o.compare(self, other)?,
                None if x == y => Ordering::Equal,
                None => {
                    return Err(Error::undefined_function(format!(
                        "type {} has no ordering",
                        t.name()
                    )));
                }
            },
            (a, b) => {
                return Err(Error::type_mismatch(format!(
                    "cannot compare {} with {}",
                    type_name(a),
                    type_name(b)
                )));
            }
        }))
    }

    /// SQL equality: `None` if either side is `NULL`.  A registered type
    /// needs no order for it (its equality is on canonical bytes).
    pub fn sql_eq(&self, other: &Value) -> Result<Option<bool>> {
        match (self, other) {
            (Value::Ext(t, x), Value::Ext(u, y)) if t == u => Ok(Some(x == y)),
            _ => Ok(self.sql_cmp(other)?.map(Ordering::is_eq)),
        }
    }

    /// The order used by `ORDER BY`, `DISTINCT` and grouping: `NULL` sorts
    /// after every value (PostgreSQL's default for ascending order).  A
    /// registered type without an ordering sorts by its bytes — never
    /// visible: `ORDER BY` refuses such a type.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            (Value::Ext(t, x), Value::Ext(u, y)) if t == u && t.imp().ordering().is_none() => {
                x.cmp(y)
            }
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
            (Value::Enum(e), DataType::Enum(t)) if e.ty == Some(t) => Value::Enum(e),
            (Value::Enum(e), DataType::Text) => Value::Text(e.label.to_string()),
            // Text to an enum needs the catalog: crate::enums::cast.
            (Value::Text(s), DataType::Enum(t)) => {
                return Err(Error::internal(format!(
                    "the enum {} label {s:?} was converted without the catalog",
                    t.name()
                )));
            }
            (v @ Value::Char(_), DataType::Char) => v,
            (Value::Text(s), DataType::Char) => Value::Char(s),
            (Value::Char(s), DataType::Text) => Value::Text(unpadded(&s).to_string()),
            (Value::Char(s), t) => Value::Text(unpadded(&s).to_string()).cast(t)?,
            (v, DataType::Char) if !matches!(v, Value::Array(_)) => {
                let Value::Text(s) = v.cast(DataType::Text)? else {
                    unreachable!("text")
                };
                Value::Char(s)
            }
            (v @ Value::Real(_), DataType::Real) => v,
            // As PostgreSQL: a real becomes numeric through its 6 significant
            // digits (FLT_DIG), as a double through 15.
            (Value::Real(f), DataType::Numeric) => Value::Numeric(Decimal::from_f32(f)?),
            (Value::Real(f), DataType::Text) => Value::Text(render_real(f)),
            (Value::Real(f), t) => Value::Float(f64::from(f)).cast(t)?,
            (Value::Text(s), DataType::Real) => Value::Real(parse_real(&s)?),
            (v, DataType::Real) if v.data_type().is_some_and(DataType::is_number) => {
                let Value::Float(f) = v.cast(DataType::Float)? else {
                    unreachable!("a double")
                };
                Value::Real(narrow_float(f)?)
            }
            (v @ Value::Ext(..), DataType::Ext(t)) if v.data_type() == Some(DataType::Ext(t)) => v,
            (Value::Text(s), DataType::Ext(t)) => t.imp().input(&s)?,
            (Value::Ext(t, b), DataType::Text) => Value::Text(t.imp().output(&Value::Ext(t, b))),
            (v @ Value::Bool(_), DataType::Bool)
            | (v @ Value::Int(_), DataType::BigInt)
            | (v @ Value::Float(_), DataType::Float)
            | (v @ Value::Text(_), DataType::Text)
            | (v @ Value::Bytea(_), DataType::Bytea)
            | (v @ Value::Numeric(_), DataType::Numeric)
            | (v @ Value::Date(_), DataType::Date)
            | (v @ Value::Time(_), DataType::Time)
            | (v @ Value::Inet(_), DataType::Inet)
            | (v @ Value::Cidr(_), DataType::Cidr)
            | (v @ Value::Timestamp(_), DataType::Timestamp)
            | (v @ Value::TimestampTz(_), DataType::TimestampTz)
            | (v @ Value::Interval(_), DataType::Interval) => v,
            (Value::Array(items), DataType::Array(e)) => Value::Array(
                items
                    .into_iter()
                    .map(|v| v.cast(*e))
                    .collect::<Result<_>>()?,
            ),
            (Value::Text(s), DataType::Array(e)) => Value::Array(
                parse_array(&s)?
                    .into_iter()
                    .map(|v| v.cast(*e))
                    .collect::<Result<_>>()?,
            ),
            (Value::Int(i), DataType::Numeric) => Value::Numeric(Decimal::from_i64(i)),
            (Value::Float(f), DataType::Numeric) => Value::Numeric(Decimal::from_f64(f)?),
            (Value::Int(i), t @ (DataType::SmallInt | DataType::Integer)) => {
                Value::Int(fit_int(i, t)?)
            }
            (Value::Numeric(d), t) if t.is_integer() => {
                let i = d.to_i64().map_err(|_| int_overflow(t))?;
                Value::Int(fit_int(i, t)?)
            }
            (Value::Numeric(d), DataType::Float) => Value::Float(d.to_f64()),
            (Value::Text(s), DataType::Numeric) => Value::Numeric(Decimal::parse(&s)?),
            (Value::Text(s), DataType::Date) => Value::Date(datetime::parse_date(&s)?),
            (Value::Text(s), DataType::Time) => Value::Time(datetime::parse_time_of_day(&s)?),
            (Value::Text(s), DataType::Inet) => Value::Inet(crate::inet::parse_inet(&s)?),
            (Value::Text(s), DataType::Cidr) => Value::Cidr(crate::inet::parse_cidr(&s)?),
            (Value::Inet(n), DataType::Cidr) => Value::Cidr(n.network()),
            (Value::Cidr(n), DataType::Inet) => Value::Inet(n),
            (Value::Timestamp(t) | Value::TimestampTz(t), DataType::Time) => {
                Value::Time(datetime::time_of_ts(t))
            }
            (Value::Interval(i), DataType::Time) => Value::Time(datetime::time_add(0, i)),
            (Value::Time(t), DataType::Interval) => Value::Interval(Interval {
                months: 0,
                days: 0,
                micros: t,
            }),
            (Value::Text(s), DataType::Timestamp) => {
                Value::Timestamp(datetime::parse_timestamp(&s, false)?)
            }
            (Value::Text(s), DataType::TimestampTz) => {
                Value::TimestampTz(datetime::parse_timestamp(&s, true)?)
            }
            (Value::Text(s), DataType::Interval) => Value::Interval(datetime::parse_interval(&s)?),
            (Value::Timestamp(t) | Value::TimestampTz(t), DataType::Date) => {
                Value::Date(datetime::ts_to_date(t))
            }
            (Value::Date(d), DataType::Timestamp) => Value::Timestamp(datetime::date_to_ts(d)),
            (Value::Date(d), DataType::TimestampTz) => Value::TimestampTz(datetime::date_to_ts(d)),
            (Value::Timestamp(t), DataType::TimestampTz) => Value::TimestampTz(t),
            (Value::TimestampTz(t), DataType::Timestamp) => Value::Timestamp(t),
            (Value::Int(i), DataType::Float) => Value::Float(i as f64),
            (Value::Float(f), t) if t.is_integer() => {
                // Rounded half to even, as PostgreSQL's `rint`.
                let r = f.round_ties_even();
                if !r.is_finite() || r < i64::MIN as f64 || r >= i64::MAX as f64 {
                    return Err(int_overflow(t));
                }
                Value::Int(fit_int(r as i64, t)?)
            }
            (Value::Int(i), DataType::Bool) => Value::Bool(i != 0),
            (Value::Bool(b), DataType::Integer) => Value::Int(i64::from(b)),
            // As PostgreSQL: a boolean cast to text is 'true'/'false' (the
            // wire format's 't'/'f' is only how it is sent).
            (Value::Bool(b), DataType::Text) => {
                Value::Text(if b { "true" } else { "false" }.into())
            }
            (v, DataType::Text) => Value::Text(v.render()),
            (Value::Text(s), t) if t.is_integer() => Value::Int(parse_int(&s, t)?),
            (Value::Text(s), DataType::Bytea) => Value::Bytea(crate::bytea::parse(&s)?.into()),
            (Value::Text(s), DataType::Float) => Value::Float(parse_double(&s)?),
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
            Value::Real(f) => render_real(*f),
            Value::Float(f) => render_float(*f),
            Value::Text(s) => s.clone(),
            Value::Bytea(b) => crate::bytea::format_hex(b),
            Value::Char(s) => s.clone(),
            Value::Numeric(d) => d.to_string(),
            Value::Date(d) => datetime::format_date(*d),
            Value::Time(t) => datetime::format_time(*t),
            Value::Enum(e) => e.label.to_string(),
            Value::Inet(n) => n.inet_text(),
            Value::Cidr(n) => n.cidr_text(),
            Value::Timestamp(t) => datetime::format_timestamp(*t, false),
            Value::TimestampTz(t) => datetime::format_timestamp(*t, true),
            Value::Interval(i) => i.to_string(),
            Value::Ext(t, _) => t.imp().output(self),
            Value::Array(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|v| match v {
                        Value::Null => "NULL".into(),
                        v => quote_element(&v.render()),
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
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
        Value::Int(_) | Value::Real(_) | Value::Float(_) | Value::Numeric(_) => 1,
        Value::Text(_) | Value::Char(_) => 2,
        Value::Bytea(_) => 8,
        Value::Time(_) => 9,
        Value::Inet(_) | Value::Cidr(_) => 10,
        Value::Enum(_) => 11,
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => 3,
        Value::Interval(_) => 4,
        Value::Array(_) => 6,
        Value::Ext(..) => 7,
    }
}

/// An array element as PostgreSQL prints it: quoted when it is empty,
/// `NULL`, or holds a delimiter, brace, quote, backslash or space.
fn quote_element(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.eq_ignore_ascii_case("null")
        && !s
            .chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if plain {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Parses an array literal `{a,"b c",NULL}` into text elements (`NULL`
/// unquoted is a NULL).
pub fn parse_array(s: &str) -> Result<Vec<Value>> {
    let bad = || Error::invalid_text(format!("malformed array literal: \"{s}\""));
    let t = s.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .ok_or_else(bad)?;
    let mut out = Vec::new();
    if inner.trim().is_empty() {
        return Ok(out);
    }
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut item = String::new();
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
            loop {
                match chars.next().ok_or_else(bad)? {
                    '"' => break,
                    '\\' => item.push(chars.next().ok_or_else(bad)?),
                    c => item.push(c),
                }
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                if matches!(c, '{' | '}' | '"') {
                    return Err(bad());
                }
                item.push(if c == '\\' {
                    chars.next();
                    chars.next().ok_or_else(bad)?
                } else {
                    c
                });
                if c != '\\' {
                    chars.next();
                }
            }
            item = item.trim_end().to_string();
            if item.is_empty() {
                return Err(bad());
            }
        }
        out.push(if !quoted && item.eq_ignore_ascii_case("null") {
            Value::Null
        } else {
            Value::Text(item)
        });
        match chars.next() {
            None => return Ok(out),
            Some(',') => {}
            Some(_) => return Err(bad()),
        }
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
    pg_float_text(f.is_nan(), f.is_infinite(), f < 0.0, &format!("{f:e}"), 15)
}

fn render_real(f: f32) -> String {
    pg_float_text(f.is_nan(), f.is_infinite(), f < 0.0, &format!("{f:e}"), 6)
}

/// A float as PostgreSQL 12+ prints it: the shortest digits that read back
/// to the same value, in fixed notation when the leading digit's power of
/// ten is in `-4 .. limit` (15 for a double, 6 for a real), else as
/// `d.ddde±XX`.  `sci` is Rust's shortest `{:e}` form (`-1.25e-7`).
fn pg_float_text(nan: bool, inf: bool, neg: bool, sci: &str, limit: i32) -> String {
    if nan {
        return "NaN".into();
    }
    if inf {
        return if neg { "-Infinity" } else { "Infinity" }.into();
    }
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("an integer exponent");
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    if digits == "0" {
        return format!("{sign}0");
    }
    if !(-4..limit).contains(&exp) {
        let m = mantissa.trim_start_matches('-');
        let e_sign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{m}e{e_sign}{:02}", exp.unsigned_abs());
    }
    if exp < 0 {
        return format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize));
    }
    let int_len = exp as usize + 1;
    if digits.len() <= int_len {
        format!("{sign}{digits}{}", "0".repeat(int_len - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..int_len], &digits[int_len..])
    }
}

/// `22003` for a float result outside its type's range.
pub fn float_overflow() -> Error {
    Error::out_of_range("value out of range: overflow")
}

/// `22003` for a nonzero float result too small for its type.
pub fn float_underflow() -> Error {
    Error::out_of_range("value out of range: underflow")
}

/// A double narrowed to a real: `22003` if it is finite but too large, or
/// nonzero but too small (as PostgreSQL's `float8` → `float4`).
pub fn narrow_float(f: f64) -> Result<f32> {
    let r = f as f32;
    if r.is_infinite() && f.is_finite() {
        Err(float_overflow())
    } else if r == 0.0 && f != 0.0 {
        Err(float_underflow())
    } else {
        Ok(r)
    }
}

/// A float's text form (`1.5`, `-2e10`, `Infinity`, `NaN`, spaces around),
/// parsed by `parse`; out of range — beyond the largest, or nonzero digits
/// that round to zero — is `22003`.
fn parse_float<F: std::str::FromStr + Copy>(
    s: &str,
    name: &str,
    parse: impl Fn(&str) -> Option<F>,
    infinite: impl Fn(F) -> bool,
    zero: impl Fn(F) -> bool,
) -> Result<F> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let bad = || Error::invalid_text(format!("invalid input syntax for type {name}: {s:?}"));
    // Rust's parser takes "inf" and "nan" (any case) as PostgreSQL does,
    // but also "infinity" spelled with a sign it rejects: none are digits.
    let f = parse(t).ok_or_else(bad)?;
    let word = t
        .chars()
        .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'));
    let mantissa = t.split(['e', 'E']).next().unwrap_or("");
    let nonzero = mantissa.chars().any(|c| ('1'..='9').contains(&c));
    if (!word && infinite(f)) || (nonzero && zero(f)) {
        return Err(Error::out_of_range(format!(
            "\"{t}\" is out of range for type {name}"
        )));
    }
    Ok(f)
}

/// A `double precision` from text.
pub fn parse_double(s: &str) -> Result<f64> {
    parse_float(
        s,
        "double precision",
        |t| t.parse().ok(),
        f64::is_infinite,
        |f| f == 0.0,
    )
}

/// A `real` from text.
pub fn parse_real(s: &str) -> Result<f32> {
    parse_float(
        s,
        "real",
        |t| t.parse().ok(),
        f32::is_infinite,
        |f| f == 0.0,
    )
}

/// A `char(n)` value without its trailing blanks.
pub fn unpadded(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// `s` as a `char(n)`: padded with blanks to `n` characters.  Longer is
/// `22001` unless the excess is blanks — or the cast is explicit, which
/// truncates.
pub fn fit_char(s: &str, n: u32, explicit: bool) -> Result<String> {
    let n = n as usize;
    let len = s.chars().count();
    if len > n {
        let cut = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
        if !explicit && !s[cut..].chars().all(|c| c == ' ') {
            return Err(Error::string_too_long(format!(
                "value too long for type character({n})"
            )));
        }
        return Ok(s[..cut].to_string());
    }
    let mut out = String::with_capacity(s.len() + n - len);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', n - len));
    Ok(out)
}

/// `22003` for a value outside integer type `t`.
pub fn int_overflow(t: DataType) -> Error {
    Error::out_of_range(format!("{t} out of range"))
}

/// `i` if it is in integer type `t`'s range, else `22003`.
pub fn fit_int(i: i64, t: DataType) -> Result<i64> {
    let (lo, hi) = t.int_range().expect("an integer type");
    if (lo..=hi).contains(&i) {
        Ok(i)
    } else {
        Err(int_overflow(t))
    }
}

/// Parses integer type `t`'s text form: an optional sign, then decimal
/// digits — or, as PostgreSQL 16, `0x`, `0o` or `0b` digits — with `_`
/// allowed between digits; spaces around.
pub fn parse_int(s: &str, t: DataType) -> Result<i64> {
    let bad = || Error::invalid_text(format!("invalid input syntax for type {t}: {s:?}"));
    let body = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let (neg, digits) = match body.as_bytes().first() {
        Some(b'-') => (true, &body[1..]),
        Some(b'+') => (false, &body[1..]),
        _ => (false, body),
    };
    let (radix, digits) = match digits.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => (16, &digits[2..]),
        Some("0o") => (8, &digits[2..]),
        Some("0b") => (2, &digits[2..]),
        _ => (10, digits),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return Err(bad());
    }
    let mut v: i128 = 0;
    for c in digits.chars().filter(|&c| c != '_') {
        let d = c.to_digit(radix).ok_or_else(bad)?;
        v = v * i128::from(radix) + i128::from(d);
        if v > i128::from(u64::MAX) {
            break;
        }
    }
    let v = if neg { -v } else { v };
    let (lo, hi) = t.int_range().expect("an integer type");
    if v < i128::from(lo) || v > i128::from(hi) {
        return Err(Error::out_of_range(format!(
            "value \"{s}\" is out of range for type {t}"
        )));
    }
    Ok(v as i64)
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
            Value::Real(f) => hash_number(f64::from(*f), state),
            Value::Float(f) => hash_number(*f, state),
            Value::Text(s) => s.hash(state),
            // As the text it equals.
            Value::Char(s) => unpadded(s).hash(state),
            Value::Bytea(b) => {
                8u8.hash(state);
                b.hash(state);
            }
            Value::Time(t) => {
                9u8.hash(state);
                t.hash(state);
            }
            // Equal whatever their type: an inet and a cidr can be equal.
            Value::Inet(n) | Value::Cidr(n) => {
                10u8.hash(state);
                n.hash(state);
            }
            Value::Enum(e) => {
                11u8.hash(state);
                e.key.hash(state);
            }
            Value::Numeric(d) => hash_number(d.to_f64(), state),
            Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
                3u8.hash(state);
                self.as_timestamp().hash(state);
            }
            Value::Interval(i) => {
                4u8.hash(state);
                i.span().hash(state);
            }
            Value::Array(items) => {
                6u8.hash(state);
                items.hash(state);
            }
            Value::Ext(t, b) => {
                7u8.hash(state);
                t.hash(state);
                b.hash(state);
            }
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
            Value::Text(" 42 ".into()).cast(DataType::BigInt).unwrap(),
            Value::Int(42)
        );
        assert_eq!(
            Value::Float(2.5).cast(DataType::BigInt).unwrap(),
            Value::Int(2)
        );
        assert_eq!(
            Value::Float(3.5).cast(DataType::Integer).unwrap(),
            Value::Int(4)
        );
        assert_eq!(
            Value::Int(40_000)
                .cast(DataType::SmallInt)
                .unwrap_err()
                .code,
            "22003"
        );
        assert_eq!(
            Value::Text("40000".into())
                .cast(DataType::SmallInt)
                .unwrap_err()
                .message,
            "value \"40000\" is out of range for type smallint"
        );
        assert!(Value::Text("x".into()).cast(DataType::BigInt).is_err());
        let err = |v: Value, t| v.cast(t).unwrap_err().message;
        assert_eq!(
            err(Value::Text("1e40".into()), DataType::Real),
            "\"1e40\" is out of range for type real"
        );
        assert_eq!(
            err(Value::Text("1e-400".into()), DataType::Float),
            "\"1e-400\" is out of range for type double precision"
        );
        assert_eq!(
            err(Value::Float(1e40), DataType::Real),
            "value out of range: overflow"
        );
        assert_eq!(
            err(Value::Float(1e-50), DataType::Real),
            "value out of range: underflow"
        );
        assert_eq!(
            Value::Text(" -Infinity ".into())
                .cast(DataType::Real)
                .unwrap(),
            Value::Real(f32::NEG_INFINITY)
        );
        assert_eq!(
            Value::Real(0.1).cast(DataType::Numeric).unwrap().render(),
            "0.1"
        );
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
        // PostgreSQL 18's output, for each.
        for (f, want) in [
            (-0.0, "-0"),
            (0.0, "0"),
            (1e300, "1e+300"),
            (-1.5e300, "-1.5e+300"),
            (2.5, "2.5"),
            (1e16, "1e+16"),
            (1e15, "1e+15"),
            (123456789012345.0, "123456789012345"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e-46, "1e-46"),
            (100.0, "100"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(Value::Float(f).render(), want);
        }
        for (f, want) in [
            (0.1f32, "0.1"),
            (-0.0, "-0"),
            (3.4e38, "3.4e+38"),
            (1e-45, "1e-45"),
            (123456789.0, "1.2345679e+08"),
            (12345678.0, "1.2345678e+07"),
            (1e10, "1e+10"),
            (1e-5, "1e-05"),
            (1e-6, "1e-06"),
            (123456.7, "123456.7"),
            (1.0 / 3.0, "0.33333334"),
        ] {
            assert_eq!(Value::Real(f).render(), want);
        }
        assert_eq!(Value::Bool(true).render(), "t");
    }
}
