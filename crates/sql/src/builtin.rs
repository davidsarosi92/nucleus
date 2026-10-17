//! The built-in module: every built-in scalar function, aggregate and table
//! function, the `public` and `nc_catalog` schemas and the `nc_catalog`
//! views — registered through the extension API ([`crate::ext`]) like any
//! other module.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use crate::ast::BinaryOp;
use crate::binder::arith_type;
use crate::error::{Error, Result};
use crate::expr::Func;
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, Equality, FnRef, Hashing, KeyEncoding,
    Module, Registry, Returns, ScalarFunction, Signature, TableFunction, TableSignature,
    TotalOrder, Type, common_type,
};
use crate::types::{DataType, Row, Value};

/// The built-in module.
pub struct Builtin;

/// The built-in functions: names (the first is the one `EXPLAIN` shows
/// for most) and argument counts.
const FUNCTIONS: &[(Func, &[&str], RangeInclusive<usize>)] = &[
    (Func::Lower, &["lower"], 1..=1),
    (Func::Upper, &["upper"], 1..=1),
    (
        Func::Length,
        &["length", "char_length", "character_length"],
        1..=1,
    ),
    (Func::Abs, &["abs"], 1..=1),
    (Func::Coalesce, &["coalesce"], 1..=usize::MAX),
    (Func::Nullif, &["nullif"], 2..=2),
    (Func::Substr, &["substr", "substring"], 2..=3),
    (Func::Round, &["round"], 1..=2),
    (Func::Floor, &["floor"], 1..=1),
    (Func::Ceil, &["ceil", "ceiling"], 1..=1),
    (Func::Trim, &["trim", "btrim"], 1..=1),
    (Func::Concat, &["concat"], 1..=usize::MAX),
    (Func::Greatest, &["greatest"], 1..=usize::MAX),
    (Func::Least, &["least"], 1..=usize::MAX),
    (Func::IsDistinctFrom, &["__is_distinct_from"], 2..=2),
    (Func::Replace, &["replace"], 3..=3),
    (Func::Strpos, &["strpos"], 2..=2),
    (Func::Sqrt, &["sqrt"], 1..=1),
    (Func::Power, &["power", "pow"], 2..=2),
    (Func::Version, &["version"], 0..=0),
    (Func::Trunc, &["trunc"], 1..=2),
    (Func::Sign, &["sign"], 1..=1),
    (Func::Mod, &["mod"], 2..=2),
    (
        Func::Now,
        &[
            "now",
            "current_timestamp",
            "transaction_timestamp",
            "statement_timestamp",
        ],
        0..=0,
    ),
    (Func::ClockTimestamp, &["clock_timestamp"], 0..=0),
    (Func::CurrentDate, &["current_date"], 0..=0),
    (Func::LocalTimestamp, &["localtimestamp"], 0..=0),
    (Func::Extract, &["extract"], 2..=2),
    (Func::DatePart, &["date_part"], 2..=2),
    (Func::DateTrunc, &["date_trunc"], 2..=2),
    (Func::MakeDate, &["make_date"], 3..=3),
    (Func::Age, &["age"], 1..=2),
    (Func::JustifyDays, &["justify_days"], 1..=1),
    (Func::JustifyHours, &["justify_hours"], 1..=1),
    (Func::JustifyInterval, &["justify_interval"], 1..=1),
    (Func::ToTimestamp, &["to_timestamp"], 1..=1),
    (Func::NumericFit, &["__numeric_fit"], 3..=3),
    (
        Func::CurrentUser,
        &["current_user", "session_user", "user", "current_role"],
        0..=0,
    ),
    (
        Func::CurrentDatabase,
        &["current_database", "current_catalog"],
        0..=0,
    ),
    (Func::CurrentSchema, &["current_schema"], 0..=0),
    (Func::ArrayToString, &["array_to_string"], 2..=3),
    (Func::ArrayUpper, &["array_upper"], 2..=2),
    (Func::ArrayLower, &["array_lower"], 2..=2),
    (Func::ArrayLength, &["array_length"], 2..=2),
    (Func::Cardinality, &["cardinality"], 1..=1),
    (Func::ArrayAppend, &["array_append"], 2..=2),
    (Func::RegexpReplace, &["regexp_replace"], 3..=4),
];

impl Module for Builtin {
    fn name(&self) -> &str {
        "builtin"
    }

    fn register(&self, r: &mut Registry) -> Result<()> {
        for f in builtins().values() {
            let names = FUNCTIONS
                .iter()
                .find(|(g, ..)| *g == f.func)
                .map(|(_, names, _)| *names)
                .expect("listed");
            r.add_function(names, f.clone())?;
        }
        for (a, names) in AGGREGATES {
            r.add_aggregate(names, Arc::new(BuiltinAgg(*a)))?;
        }
        r.add_table_function(Arc::new(GenerateSeries))?;
        for (ty, names, _) in TYPES {
            r.add_builtin_type(names, *ty);
        }
        r.add_table_function(Arc::new(Unnest))?;
        r.add_schema("public", "user tables", false)?;
        r.add_schema("nc_catalog", "the nucleus system catalog", false)?;
        for v in crate::ncatalog::views() {
            r.add_view(v)?;
        }
        Ok(())
    }
}

/// A built-in function (they share one implementation, by [`Func`]).
struct BuiltinFn {
    func: Func,
    arity: RangeInclusive<usize>,
}

fn builtins() -> &'static HashMap<Func, Arc<BuiltinFn>> {
    static B: OnceLock<HashMap<Func, Arc<BuiltinFn>>> = OnceLock::new();
    B.get_or_init(|| {
        FUNCTIONS
            .iter()
            .map(|(f, _, arity)| {
                (
                    *f,
                    Arc::new(BuiltinFn {
                        func: *f,
                        arity: arity.clone(),
                    }),
                )
            })
            .collect()
    })
}

/// The built-in function `f`, for calls the binder makes itself
/// (`coalesce` for `USING` joins, the length and precision checks of an
/// assignment).
pub fn func(f: Func) -> FnRef {
    FnRef::new(builtins()[&f].clone())
}

impl ScalarFunction for BuiltinFn {
    fn name(&self) -> &str {
        self.func.name()
    }

    fn arity(&self) -> RangeInclusive<usize> {
        self.arity.clone()
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        signature(self.func, args)
    }

    /// The built-ins handle `NULL`s themselves.
    fn strict(&self) -> bool {
        false
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        match self.func {
            Func::ArrayToString
            | Func::ArrayUpper
            | Func::ArrayLower
            | Func::ArrayLength
            | Func::Cardinality
            | Func::ArrayAppend
            | Func::RegexpReplace => arrays_and_regex(self.func, args),
            f => crate::expr::func(f, args.to_vec()),
        }
    }
}

/// The array functions and `regexp_replace`, which take `NULL`s their
/// own way.
fn arrays_and_regex(f: Func, args: &[Value]) -> Result<Value> {
    let as_int = |i: usize| match args.get(i) {
        Some(Value::Int(n)) => Some(*n),
        Some(Value::Numeric(d)) => d.to_i64().ok(),
        Some(Value::Text(s)) => s.trim().parse().ok(),
        _ => None,
    };
    let array = |i: usize| match args.get(i) {
        Some(Value::Array(items)) => Some(items),
        _ => None,
    };
    Ok(match f {
        Func::ArrayToString => {
            let Some(items) = array(0) else {
                return Ok(Value::Null);
            };
            let Some(Value::Text(sep)) = args.get(1) else {
                return Ok(Value::Null);
            };
            let null_as = match args.get(2) {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            };
            let parts: Vec<String> = items
                .iter()
                .filter_map(|v| match v {
                    Value::Null => null_as.clone(),
                    v => Some(v.render()),
                })
                .collect();
            Value::Text(parts.join(sep))
        }
        Func::ArrayUpper | Func::ArrayLength | Func::ArrayLower | Func::Cardinality => {
            let Some(items) = array(0) else {
                return Ok(Value::Null);
            };
            if f != Func::Cardinality && as_int(1) != Some(1) || items.is_empty() {
                return Ok(if f == Func::Cardinality {
                    Value::Int(0)
                } else {
                    Value::Null
                });
            }
            Value::Int(if f == Func::ArrayLower {
                1
            } else {
                items.len() as i64
            })
        }
        Func::ArrayAppend => {
            let mut items = array(0).cloned().unwrap_or_default();
            items.push(args.get(1).cloned().unwrap_or(Value::Null));
            Value::Array(items)
        }
        Func::RegexpReplace => {
            let (Some(Value::Text(s)), Some(Value::Text(p)), Some(Value::Text(rep))) =
                (args.first(), args.get(1), args.get(2))
            else {
                return Ok(Value::Null);
            };
            let flags = match args.get(3) {
                Some(Value::Text(f)) => f.clone(),
                _ => String::new(),
            };
            // PostgreSQL writes \1 for a group; the regex crate ${1}.
            let mut rep2 = String::new();
            let mut chars = rep.chars().peekable();
            while let Some(c) = chars.next() {
                match (c, chars.peek()) {
                    ('\\', Some(d)) if d.is_ascii_digit() => {
                        rep2.push_str(&format!("${{{d}}}"));
                        chars.next();
                    }
                    ('\\', Some('&')) => {
                        rep2.push_str("${0}");
                        chars.next();
                    }
                    ('$', _) => rep2.push_str("$$"),
                    (c, _) => rep2.push(c),
                }
            }
            crate::expr::with_regex(p, flags.contains('i'), |re| {
                if flags.contains('g') {
                    re.replace_all(s, rep2.as_str()).into_owned()
                } else {
                    re.replace(s, rep2.as_str()).into_owned()
                }
            })
            .map(Value::Text)?
        }
        _ => unreachable!("{} is not an array function", f.name()),
    })
}

fn type_name(t: Option<DataType>) -> &'static str {
    t.map_or("unknown", DataType::name)
}

fn signature(f: Func, args: &[ArgInfo]) -> Result<Signature> {
    use DataType::*;
    let n = args.len();
    let t = |i: usize| args[i].ty;
    let keep = |ty: Option<DataType>| Ok(Signature::keep(n, ty));
    match f {
        Func::Lower
        | Func::Upper
        | Func::Trim
        | Func::Substr
        | Func::Concat
        | Func::Replace
        | Func::Version
        | Func::CurrentUser
        | Func::CurrentDatabase
        | Func::CurrentSchema
        | Func::ArrayToString
        | Func::RegexpReplace => keep(Some(Text)),
        Func::Length | Func::Strpos => keep(Some(Int)),
        Func::ArrayUpper | Func::ArrayLower | Func::ArrayLength | Func::Cardinality => {
            keep(Some(Int))
        }
        Func::IsDistinctFrom => keep(Some(Bool)),
        Func::Sqrt | Func::Power => keep(Some(Float)),
        // round(int, n) and trunc(int) are numeric.
        Func::Round | Func::Trunc if t(0) == Some(Int) && (f == Func::Trunc || n == 2) => {
            let mut a = vec![Arg::Keep; n];
            a[0] = Arg::Convert(Numeric);
            Ok(Signature::new(a, Returns::Type(Some(Numeric))))
        }
        Func::Abs
        | Func::Round
        | Func::Trunc
        | Func::Floor
        | Func::Ceil
        | Func::Sign
        | Func::Nullif => keep(t(0)),
        Func::Mod => keep(arith_type(BinaryOp::Mod, t(0), t(1)).flatten()),
        Func::Now | Func::ClockTimestamp | Func::ToTimestamp => Ok(Signature::new(
            vec![Arg::Untyped(Float); n],
            Returns::Type(Some(TimestampTz)),
        )),
        Func::CurrentDate | Func::MakeDate => keep(Some(Date)),
        Func::LocalTimestamp => keep(Some(Timestamp)),
        Func::Extract | Func::DatePart | Func::DateTrunc => {
            if let Some(t0) = t(0).filter(|t| *t != Text) {
                return Err(Error::undefined_function(format!(
                    "operator does not exist: {t0} {} text",
                    f.name()
                )));
            }
            let (a1, t1) = if args[1].text_literal {
                (Arg::Untyped(Timestamp), Some(Timestamp))
            } else {
                (Arg::Keep, t(1))
            };
            if t1.is_some_and(|t| !t.is_datetime() && t != Interval) {
                return Err(Error::undefined_function(format!(
                    "function {}(text, {}) does not exist",
                    f.name(),
                    type_name(t1)
                )));
            }
            let returns = match f {
                Func::Extract => Some(Numeric),
                Func::DatePart => Some(Float),
                _ => match t1 {
                    Some(Date) => Some(TimestampTz),
                    t => t,
                },
            };
            Ok(Signature::new(
                vec![Arg::Untyped(Text), a1],
                Returns::Type(returns),
            ))
        }
        Func::Age => Ok(Signature::new(
            args.iter()
                .map(|a| {
                    if a.text_literal {
                        Arg::Untyped(Timestamp)
                    } else {
                        Arg::Keep
                    }
                })
                .collect(),
            Returns::Type(Some(Interval)),
        )),
        Func::JustifyDays | Func::JustifyHours | Func::JustifyInterval => Ok(Signature::new(
            vec![Arg::Untyped(Interval)],
            Returns::Type(Some(Interval)),
        )),
        Func::NumericFit => keep(Some(Numeric)),
        Func::ArrayAppend => {
            let et = t(0).and_then(DataType::element);
            let untyped = args[1].ty.is_none() || args[1].text_literal;
            let t1 = match et {
                Some(e) if untyped => Some(e),
                _ => t(1),
            };
            Ok(Signature::new(
                vec![Arg::Keep, et.map_or(Arg::Keep, Arg::Untyped)],
                Returns::Type(t(0).or_else(|| t1.and_then(DataType::array_of))),
            ))
        }
        Func::Coalesce | Func::Greatest | Func::Least => {
            Ok(Signature::new(vec![Arg::Common; n], Returns::Common))
        }
    }
}

// ------------------------------------------------------------ aggregates

/// The built-in aggregates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StringAgg,
    ArrayAgg,
    BoolAnd,
    BoolOr,
}

const AGGREGATES: &[(AggFunc, &[&str])] = &[
    (AggFunc::Count, &["count"]),
    (AggFunc::Sum, &["sum"]),
    (AggFunc::Avg, &["avg"]),
    (AggFunc::Min, &["min"]),
    (AggFunc::Max, &["max"]),
    (AggFunc::StringAgg, &["string_agg"]),
    (AggFunc::ArrayAgg, &["array_agg"]),
    (AggFunc::BoolAnd, &["bool_and", "every"]),
    (AggFunc::BoolOr, &["bool_or"]),
];

struct BuiltinAgg(AggFunc);

impl Aggregate for BuiltinAgg {
    fn name(&self) -> &str {
        AGGREGATES
            .iter()
            .find(|(f, _)| *f == self.0)
            .expect("listed")
            .1[0]
    }

    fn arity(&self) -> RangeInclusive<usize> {
        if self.0 == AggFunc::StringAgg {
            2..=2
        } else {
            1..=1
        }
    }

    fn accepts_star(&self) -> bool {
        self.0 == AggFunc::Count
    }

    /// `array_agg` keeps `NULL`s.
    fn keeps_nulls(&self) -> bool {
        self.0 == AggFunc::ArrayAgg
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::*;
        let name = self.name();
        let at = args.first().and_then(|a| a.ty);
        let keep = |t| Ok(Signature::keep(args.len(), t));
        if matches!(self.0, AggFunc::Sum | AggFunc::Avg)
            && at.is_some_and(|t| !t.is_number() && t != Interval)
        {
            return Err(Error::undefined_function(format!(
                "function {name}({}) does not exist",
                at.unwrap()
            )));
        }
        match self.0 {
            AggFunc::Count => keep(Some(Int)),
            // As PostgreSQL: the average of integers is numeric.
            AggFunc::Avg => keep(match at {
                Some(Int) | None => Some(Numeric),
                t => t,
            }),
            AggFunc::Sum => keep(at),
            AggFunc::Min | AggFunc::Max => {
                crate::binder::require(at, &[crate::binder::Need::Ordering], name)?;
                keep(at)
            }
            AggFunc::StringAgg => {
                if let Some(t) = args.iter().find_map(|a| a.ty.filter(|t| *t != Text)) {
                    return Err(Error::undefined_function(format!(
                        "operator does not exist: {t} string_agg text"
                    )));
                }
                Ok(Signature::new(
                    vec![Arg::Untyped(Text); 2],
                    Returns::Type(Some(Text)),
                ))
            }
            AggFunc::ArrayAgg => {
                keep(Some(at.unwrap_or(Text).array_of().ok_or_else(|| {
                    Error::feature("arrays of arrays are not supported")
                })?))
            }
            AggFunc::BoolAnd | AggFunc::BoolOr => match at {
                Some(t) if t != Bool => Err(Error::type_mismatch(format!(
                    "argument of {name} must be type boolean, not type {t}"
                ))),
                _ => keep(Some(Bool)),
            },
        }
    }

    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(Acc {
            func: self.0,
            count: 0,
            value: Value::Null,
            text: None,
            items: None,
        })
    }
}

/// A built-in aggregate's state.
struct Acc {
    func: AggFunc,
    /// Rows seen (`count`, `avg`).
    count: i64,
    /// The running sum, minimum, maximum or truth value (`NULL`: no row).
    value: Value,
    /// `string_agg` so far.
    text: Option<String>,
    /// `array_agg` so far.
    items: Option<Vec<Value>>,
}

impl Accumulator for Acc {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        let add = |a: Value, b: Value| crate::expr::binary(BinaryOp::Add, a, b);
        let v = args.first().cloned().unwrap_or(Value::Null);
        match self.func {
            AggFunc::Count => self.count += 1,
            AggFunc::Sum => {
                self.value = match (&self.value, v) {
                    (Value::Null, v @ (Value::Int(_) | Value::Float(_) | Value::Numeric(_))) => v,
                    (Value::Null, v @ Value::Interval(_)) => v,
                    (Value::Null, v) => {
                        return Err(Error::undefined_function(format!(
                            "function sum({}) does not exist",
                            crate::types::type_name(&v)
                        )));
                    }
                    (s, v) => add(s.clone(), v)?,
                }
            }
            AggFunc::Avg => {
                let v = match v {
                    Value::Int(i) => Value::Numeric(crate::decimal::Decimal::from_i64(i)),
                    v @ (Value::Float(_) | Value::Numeric(_) | Value::Interval(_)) => v,
                    v => {
                        return Err(Error::undefined_function(format!(
                            "function avg({}) does not exist",
                            crate::types::type_name(&v)
                        )));
                    }
                };
                self.value = if self.value.is_null() {
                    v
                } else {
                    add(self.value.clone(), v)?
                };
                self.count += 1;
            }
            AggFunc::Min | AggFunc::Max => {
                let replace = self.value.is_null() || {
                    let o = v.sql_cmp(&self.value)?.expect("not null");
                    if self.func == AggFunc::Min {
                        o.is_lt()
                    } else {
                        o.is_gt()
                    }
                };
                if replace {
                    self.value = v;
                }
            }
            AggFunc::StringAgg => {
                let text = v.render();
                self.text = Some(match self.text.take() {
                    None => text,
                    Some(mut so_far) => {
                        if let Some(sep) = args.get(1).filter(|s| !s.is_null()) {
                            so_far.push_str(&sep.render());
                        }
                        so_far.push_str(&text);
                        so_far
                    }
                });
            }
            AggFunc::ArrayAgg => self.items.get_or_insert_with(Vec::new).push(v),
            AggFunc::BoolAnd | AggFunc::BoolOr => {
                let x = v.truth()?.expect("not null");
                self.value = Value::Bool(match self.value {
                    Value::Bool(y) if self.func == AggFunc::BoolAnd => x && y,
                    Value::Bool(y) => x || y,
                    _ => x,
                });
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Ok(match self.func {
            AggFunc::Count => Value::Int(self.count),
            AggFunc::Avg => match std::mem::replace(&mut self.value, Value::Null) {
                _ if self.count == 0 => Value::Null,
                Value::Numeric(d) => {
                    Value::Numeric(d.checked_div(crate::decimal::Decimal::from_i64(self.count))?)
                }
                Value::Interval(i) => Value::Interval(i.checked_div(self.count as f64)?),
                Value::Float(f) => Value::Float(f / self.count as f64),
                _ => Value::Null,
            },
            AggFunc::StringAgg => self.text.take().map_or(Value::Null, Value::Text),
            AggFunc::ArrayAgg => self.items.take().map_or(Value::Null, Value::Array),
            _ => std::mem::replace(&mut self.value, Value::Null),
        })
    }
}

// ------------------------------------------------------- table functions

/// `generate_series(start, stop [, step])` over integers, numerics or
/// timestamps (with an interval step).
struct GenerateSeries;

impl TableFunction for GenerateSeries {
    fn name(&self) -> &str {
        "generate_series"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=3
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<TableSignature> {
        let ty = common_type(&args[..2])?.unwrap_or(DataType::Int);
        if !(ty.is_number() || ty.is_datetime()) {
            return Err(Error::undefined_function(format!(
                "function generate_series({ty}, {ty}) does not exist"
            )));
        }
        let mut a = vec![Arg::Common, Arg::Common];
        if args.len() == 3 {
            a.push(Arg::Untyped(if ty.is_datetime() {
                DataType::Interval
            } else {
                ty
            }));
        }
        let out = if ty == DataType::Date {
            DataType::Timestamp
        } else {
            ty
        };
        Ok(TableSignature {
            args: a,
            columns: vec![(self.name().into(), Some(out))],
        })
    }

    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        crate::exec::generate_series(args)
    }
}

/// `unnest(array)`: its elements.
struct Unnest;

impl TableFunction for Unnest {
    fn name(&self) -> &str {
        "unnest"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![Arg::Keep],
            columns: vec![(self.name().into(), args[0].ty.and_then(DataType::element))],
        })
    }

    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        Ok(match &args[0] {
            Value::Array(items) => items.iter().map(|v| vec![v.clone()]).collect(),
            _ => Vec::new(),
        })
    }
}

// ------------------------------------------------------------------ types

/// The built-in types: name, the other names SQL accepts, category.
const TYPES: &[(DataType, &[&str], &str)] = &[
    (DataType::Bool, &["boolean", "bool"], "boolean"),
    (
        DataType::Int,
        &[
            "bigint", "int", "integer", "smallint", "int8", "int4", "int2",
        ],
        "number",
    ),
    (
        DataType::Float,
        &[
            "double precision",
            "float",
            "real",
            "float8",
            "float4",
            "double",
        ],
        "number",
    ),
    (DataType::Numeric, &["numeric", "decimal", "dec"], "number"),
    (
        DataType::Text,
        &[
            "text",
            "varchar",
            "character varying",
            "char",
            "character",
            "string",
        ],
        "text",
    ),
    (DataType::Date, &["date"], "datetime"),
    (
        DataType::Timestamp,
        &["timestamp", "timestamp without time zone"],
        "datetime",
    ),
    (
        DataType::TimestampTz,
        &["timestamptz", "timestamp with time zone"],
        "datetime",
    ),
    (DataType::Interval, &["interval"], "interval"),
];

/// A built-in type as a [`Type`]: its values are native, and its abilities
/// are the engine's own code (the hot paths call it directly).
struct BuiltinType {
    ty: DataType,
    names: &'static [&'static str],
    category: &'static str,
}

/// The native abilities, shared by every built-in type.
struct Native;

impl Equality for Native {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool> {
        Ok(a.sql_eq(b)?.unwrap_or(false))
    }
}

impl Hashing for Native {
    fn hash(&self, v: &Value, state: &mut dyn std::hash::Hasher) {
        struct H<'a>(&'a mut dyn std::hash::Hasher);
        impl std::hash::Hasher for H<'_> {
            fn finish(&self) -> u64 {
                self.0.finish()
            }
            fn write(&mut self, bytes: &[u8]) {
                self.0.write(bytes);
            }
        }
        std::hash::Hash::hash(v, &mut H(state));
    }
}

impl TotalOrder for Native {
    fn compare(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        Ok(a.sql_cmp(b)?.unwrap_or(std::cmp::Ordering::Equal))
    }
}

impl KeyEncoding for Native {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        crate::encoding::encode_key_value(out, v);
    }
    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        crate::encoding::decode_key_value(b)
    }
}

impl Type for BuiltinType {
    fn name(&self) -> &str {
        self.names[0]
    }
    fn aliases(&self) -> &[&str] {
        &self.names[1..]
    }
    fn category(&self) -> &str {
        self.category
    }
    fn input(&self, text: &str) -> Result<Value> {
        Value::Text(text.to_string()).cast(self.ty)
    }
    fn output(&self, v: &Value) -> String {
        v.render()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&Native)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&Native)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&Native)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Native)
    }
}

/// The [`Type`] behind any type: a built-in one's descriptor (arrays of
/// built-ins share their abilities), or a registered type's own.
pub fn type_of(ty: DataType) -> &'static dyn Type {
    static ALL: OnceLock<Vec<BuiltinType>> = OnceLock::new();
    static ARRAY: BuiltinType = BuiltinType {
        ty: DataType::Array(&DataType::Text),
        names: &["array"],
        category: "array",
    };
    if let DataType::Ext(t) = ty {
        return t.imp();
    }
    let all = ALL.get_or_init(|| {
        TYPES
            .iter()
            .map(|(ty, names, category)| BuiltinType {
                ty: *ty,
                names,
                category,
            })
            .collect()
    });
    all.iter().find(|b| b.ty == ty).map_or(&ARRAY, |b| b)
}
