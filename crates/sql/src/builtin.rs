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
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, Equality, FnRef, GroupAccumulator, Hashing,
    KeyEncoding, Module, Registry, Returns, ScalarFunction, Signature, TableFunction,
    TableSignature, TotalOrder, Type, Volatility, common_type,
};
use crate::fsum::ExactSum;
use crate::types::{DataType, Row, Value};
use crate::vector::{Sel, Vector};

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
    (Func::Repeat, &["repeat"], 2..=2),
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
    (Func::LocalTime, &["localtime"], 0..=0),
    (Func::MakeTime, &["make_time"], 3..=3),
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
    (Func::CharFit, &["__char_fit"], 2..=2),
    (
        Func::CurrentUser,
        &["current_user", "user", "current_role"],
        0..=0,
    ),
    (Func::SessionUser, &["session_user"], 0..=0),
    (Func::CurrentSetting, &["current_setting"], 1..=2),
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
        r.add_function(&["partition_hash"], crate::partition::partition_hash_arc())?;
        for (ty, names, _) in TYPES {
            r.add_builtin_type(names, *ty);
            crate::ext::lawful(type_of(*ty))?;
            crate::ext::lawful(type_of(ty.array_of().expect("arrays")))?;
        }
        crate::uuid::register(r)?;
        crate::vectors::register(r)?;
        crate::moments::register(r)?;
        crate::quantile::register(r)?;
        crate::buckets::register(r)?;
        crate::regression::register(r)?;
        crate::bytea::register(r)?;
        crate::inet::register(r)?;
        crate::statements::register(r)?;
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

    fn volatility(&self) -> Volatility {
        match self.func {
            Func::ClockTimestamp => Volatility::Volatile,
            Func::Now
            | Func::CurrentDate
            | Func::LocalTimestamp
            | Func::LocalTime
            | Func::CurrentUser
            | Func::SessionUser
            | Func::CurrentSetting
            | Func::CurrentDatabase
            | Func::CurrentSchema => Volatility::Stable,
            _ => Volatility::Immutable,
        }
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

    fn call_batch(
        &self,
        _: &CallContext<'_>,
        args: &[&Vector],
        len: usize,
        sel: &Sel,
    ) -> Option<Result<Vector>> {
        match self.func {
            Func::Substr => substr_batch(args, len, sel).map(Ok),
            _ => None,
        }
    }
}

/// `substr(text, from [, count])` over a text or `char(n)` vector (a
/// char without its trailing blanks) with constant positions: characters
/// `from .. from + count`, as [`crate::expr::func`] takes them.  `None`
/// for anything else (bytea, positions that vary, `NULL` or negative
/// counts — the row's way decides those).
fn substr_batch(args: &[&Vector], len: usize, sel: &Sel) -> Option<Vector> {
    let (o, b, nulls, char) = crate::vector::text_view(args.first()?)?;
    let int = |v: &Vector| match v {
        Vector::Const(Value::Int(i)) => Some(*i),
        _ => None,
    };
    let from = int(args.get(1)?)?;
    let count = match args.get(2) {
        Some(v) => Some(int(v).filter(|&c| c >= 0)?),
        None => None,
    };
    let start = from.max(1);
    let end = count.map_or(i64::MAX, |c| from.saturating_add(c).max(start));
    // Characters [a, b) of the string, as a byte range.
    let (a, z) = ((start - 1) as usize, (end - 1).max(0) as usize);
    let mut offsets = Vec::with_capacity(len + 1);
    let mut bytes = Vec::new();
    offsets.push(0u32);
    let mut rows = sel.iter().peekable();
    for i in 0..len {
        if rows.peek() == Some(&i) {
            rows.next();
            if !nulls.as_ref().is_some_and(|n| n[i]) {
                let s = crate::vector::text_at(o, b, char, i);
                let range = if s.is_ascii() {
                    a.min(s.len())..z.min(s.len())
                } else {
                    let t = std::str::from_utf8(s).ok()?;
                    let at = |k: usize| t.char_indices().nth(k).map_or(t.len(), |(p, _)| p);
                    at(a)..at(z.max(a))
                };
                if range.start < range.end {
                    bytes.extend_from_slice(&s[range]);
                }
            }
        }
        offsets.push(bytes.len() as u32);
    }
    Some(Vector::Text(offsets, bytes, nulls.clone()))
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
        Func::Substr if t(0) == Some(Bytea) => keep(Some(Bytea)),
        Func::Lower
        | Func::Upper
        | Func::Trim
        | Func::Substr
        | Func::Concat
        | Func::Replace
        | Func::Repeat
        | Func::Version
        | Func::CurrentUser
        | Func::SessionUser
        | Func::CurrentDatabase
        | Func::CurrentSchema
        | Func::ArrayToString
        | Func::RegexpReplace => keep(Some(Text)),
        Func::Length | Func::Strpos => keep(Some(Integer)),
        Func::ArrayUpper | Func::ArrayLower | Func::ArrayLength | Func::Cardinality => {
            keep(Some(Integer))
        }
        Func::IsDistinctFrom => keep(Some(Bool)),
        Func::CurrentSetting => Ok(Signature::new(
            [Arg::Untyped(Text), Arg::Untyped(Bool)][..n].to_vec(),
            Returns::Type(Some(Text)),
        )),
        Func::Sqrt | Func::Power => keep(Some(Float)),
        // round(int, n) and trunc(int) are numeric.
        Func::Round | Func::Trunc
            if t(0).is_some_and(DataType::is_integer) && (f == Func::Trunc || n == 2) =>
        {
            let mut a = vec![Arg::Keep; n];
            a[0] = Arg::Convert(Numeric);
            Ok(Signature::new(a, Returns::Type(Some(Numeric))))
        }
        // Of a real, these are a double's (PostgreSQL has no real ones).
        Func::Round | Func::Trunc | Func::Floor | Func::Ceil | Func::Sign if t(0) == Some(Real) => {
            let mut a = vec![Arg::Keep; n];
            a[0] = Arg::Convert(Float);
            Ok(Signature::new(a, Returns::Type(Some(Float))))
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
        Func::LocalTime => keep(Some(Time)),
        Func::MakeTime => Ok(Signature::new(
            vec![
                Arg::Untyped(Integer),
                Arg::Untyped(Integer),
                Arg::Convert(Float),
            ],
            Returns::Type(Some(Time)),
        )),
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
            if t1.is_some_and(|t| !t.is_datetime() && t != Interval && t != Time)
                || (f == Func::DateTrunc && t1 == Some(Time))
            {
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
        Func::CharFit => keep(Some(Char)),
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

    /// Counts, least and greatest values and the boolean ones cannot fail.
    fn infallible(&self) -> bool {
        matches!(
            self.0,
            AggFunc::Count | AggFunc::Min | AggFunc::Max | AggFunc::BoolAnd | AggFunc::BoolOr
        )
    }

    /// All but `string_agg` and `array_agg`, whose values come in the
    /// order of their rows (a sum of doubles is exact, so in any order).
    fn order_free(&self) -> bool {
        !matches!(self.0, AggFunc::StringAgg | AggFunc::ArrayAgg)
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
            AggFunc::Count => keep(Some(BigInt)),
            // As PostgreSQL: the average of integers is numeric.
            AggFunc::Avg => keep(match at {
                Some(t) if t.is_integer() => Some(Numeric),
                Some(Real) => Some(Float),
                None => Some(Numeric),
                t => t,
            }),
            // The sum of smallints or integers is a bigint (as
            // PostgreSQL); of bigints, a bigint (PostgreSQL's is numeric).
            AggFunc::Sum => keep(match at {
                Some(t) if t.is_integer() => Some(BigInt),
                t => t,
            }),
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
        Box::new(Acc::new(self.0))
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        if self.0 == AggFunc::Count {
            return Some(Box::new(CountGroups(Vec::new())));
        }
        Some(Box::new(Grouped::new(self.0)))
    }
}

/// `count`'s groups: a count each (no state beside it, and an integer
/// for a partial one).
struct CountGroups(Vec<i64>);

impl GroupAccumulator for CountGroups {
    fn resize(&mut self, groups: usize) {
        self.0.resize(groups, 0);
    }

    fn group_bytes(&self) -> usize {
        std::mem::size_of::<i64>()
    }

    fn update(&mut self, _rows: &[u32], groups: &[u32], _args: &[&Vector]) -> Result<()> {
        for &g in groups {
            self.0[g as usize] += 1;
        }
        Ok(())
    }

    fn update_all(
        &mut self,
        len: usize,
        group: u32,
        skip_nulls: bool,
        args: &[&Vector],
    ) -> Result<()> {
        self.0[group as usize] += match args.first() {
            Some(a) if skip_nulls => (0..len).filter(|&r| !a.is_null(r)).count() as i64,
            _ => len as i64,
        };
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(Value::Int(self.0[group]))
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        match partial {
            Value::Int(n) => self.0[group] += n,
            Value::Null => {}
            _ => return Err(Error::internal("a malformed partial count")),
        }
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        Ok(Value::Int(self.0[group]))
    }

    fn finish_keeps_state(&self) -> bool {
        true
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
    /// The integers summed so far (`sum`, `avg`): exactly, whatever their
    /// order, so a sum overflows only if its result does.
    int: Option<i128>,
    /// The doubles and reals summed so far, exactly (rounded once, at the
    /// end), and whether they were all reals.
    float: Option<Box<ExactSum>>,
    real: bool,
}

impl Acc {
    fn new(func: AggFunc) -> Self {
        Acc {
            func,
            count: 0,
            value: Value::Null,
            text: None,
            items: None,
            int: None,
            float: None,
            real: true,
        }
    }

    /// Adds a double (`real`: a real) to a `sum` or `avg`.
    fn add_float(&mut self, x: f64, real: bool) {
        self.float.get_or_insert_with(Default::default).add(x);
        self.real &= real;
        if self.func == AggFunc::Avg {
            self.count += 1;
        }
    }

    /// The state as one value, for [`Acc::combine`] in another thread's
    /// accumulator: what parallel aggregation passes between workers.
    fn partial(&self) -> Value {
        let float = match &self.float {
            None => Value::Null,
            Some(f) => Value::Bytea(f.to_bytes().into()),
        };
        Value::Array(vec![
            Value::Int(self.count),
            self.value.clone(),
            self.int.map_or(Value::Null, |i| {
                Value::Bytea(i.to_le_bytes().to_vec().into())
            }),
            float,
            Value::Bool(self.real),
            self.items.clone().map_or(Value::Null, Value::Array),
        ])
    }

    /// Adds a state another accumulator of the same aggregate made
    /// ([`Acc::partial`]), as if its rows had come after this one's.
    fn combine(&mut self, partial: Value) -> Result<()> {
        let bad = || Error::internal("a malformed partial aggregate state");
        let Value::Array(parts) = partial else {
            return Err(bad());
        };
        let [count, value, int, float, real, items] =
            <[Value; 6]>::try_from(parts).map_err(|_| bad())?;
        let Value::Int(count) = count else {
            return Err(bad());
        };
        self.count += count;
        if !value.is_null() {
            match self.func {
                AggFunc::Min | AggFunc::Max | AggFunc::BoolAnd | AggFunc::BoolOr => {
                    let n = self.count;
                    self.add(&[value])?;
                    self.count = n;
                }
                _ if self.value.is_null() => self.value = value,
                _ => {
                    let v = std::mem::replace(&mut self.value, Value::Null);
                    self.value = crate::expr::binary(BinaryOp::Add, v, value)?;
                }
            }
        }
        if let Value::Bytea(b) = int {
            let i = i128::from_le_bytes(b[..].try_into().map_err(|_| bad())?);
            self.int = Some(self.int.unwrap_or(0) + i);
        }
        if let Value::Bytea(f) = float {
            self.float
                .get_or_insert_with(Default::default)
                .merge_bytes(&f)
                .ok_or_else(bad)?;
            self.real &= real == Value::Bool(true);
        }
        if let Value::Array(more) = items {
            self.items.get_or_insert_with(Vec::new).extend(more);
        }
        Ok(())
    }

    /// Adds an integer to a `sum` or `avg`.
    fn add_int(&mut self, i: i64) {
        self.int = Some(self.int.unwrap_or(0) + i128::from(i));
        if self.func == AggFunc::Avg {
            self.count += 1;
        }
    }

    /// Adds a decimal to a `sum` or `avg`, as [`Acc::add`] adds it.
    #[inline]
    fn add_dec(&mut self, d: crate::decimal::Decimal) -> Result<()> {
        match self.value {
            // At the sum's scale (a column's): its mantissas added.
            Value::Numeric(sum) if sum.scale() == d.scale() => {
                let m = sum
                    .mantissa()
                    .checked_add(d.mantissa())
                    .filter(|m| m.unsigned_abs() <= crate::decimal::LIMIT)
                    .ok_or_else(crate::decimal::overflow)?;
                self.value = Value::Numeric(crate::decimal::Decimal::raw(m, d.scale()));
            }
            Value::Numeric(sum) => self.value = Value::Numeric(sum.checked_add(d)?),
            Value::Null => self.value = Value::Numeric(d),
            _ => return self.add(&[Value::Numeric(d)]),
        }
        if self.func == AggFunc::Avg {
            self.count += 1;
        }
        Ok(())
    }

    /// The running sum: the integers', the doubles' and the other values'
    /// (the state kept: a window's frame may grow).
    fn total(&self) -> Result<Value> {
        let mut value = self.value.clone();
        let alone = value.is_null() && self.float.is_none();
        if let Some(int) = self.int {
            let int = match i64::try_from(int) {
                Ok(i) if self.func == AggFunc::Sum => Value::Int(i),
                Err(_) if self.func == AggFunc::Sum && alone => {
                    return Err(Error::out_of_range("bigint out of range"));
                }
                _ => Value::Numeric(crate::decimal::Decimal::new(int, 0)?),
            };
            value = if value.is_null() {
                int
            } else {
                crate::expr::binary(BinaryOp::Add, value, int)?
            };
        }
        if let Some(f) = &self.float {
            let f = f.total().ok_or_else(crate::types::float_overflow)?;
            let f = if self.real && self.func == AggFunc::Sum {
                let r = f as f32;
                if r.is_infinite() && f.is_finite() {
                    return Err(crate::types::float_overflow());
                }
                Value::Real(r)
            } else {
                Value::Float(f)
            };
            value = if value.is_null() {
                f
            } else {
                crate::expr::binary(BinaryOp::Add, value, f)?
            };
        }
        Ok(value)
    }
}

/// The built-in aggregates over many groups (`count`'s are
/// [`CountGroups`]), fed a batch at a time: each group's state in a column
/// of the kind its aggregate and argument need (§M14.20 W6) — a value for
/// a least, greatest or boolean one, an exact sum and a row count for a
/// sum or mean of integers or of decimals at one scale, else the [`Acc`]
/// the row-at-a-time path builds.  The results are the same whichever.
struct Grouped {
    func: AggFunc,
    /// The groups made room for.
    len: usize,
    states: States,
}

/// [`Grouped`]'s states.
enum States {
    /// A `sum` or `avg` before its first rows or partial state, which
    /// choose between [`States::Sums`] and [`States::Accs`].
    Unset,
    /// `min`, `max`, `bool_and`, `bool_or`: a group's value is its whole
    /// state (`NULL`: no row yet).
    Values(Vec<Value>),
    /// A `sum` or `avg` of integers (`scale` `None`) or of decimals at one
    /// scale: each group's exact sum (a decimal's mantissa, in range) and
    /// its rows (a `sum`'s: not 0 once it has one).
    Sums {
        scale: Option<u32>,
        sums: Vec<i128>,
        rows: Vec<i64>,
    },
    /// Any other state, or a sum of other values than its first ones.
    Accs(Vec<Acc>),
}

/// Two decimal mantissas added, in range.
#[inline]
fn add_mantissas(a: i128, b: i128) -> Result<i128> {
    a.checked_add(b)
        .filter(|m| m.unsigned_abs() <= crate::decimal::LIMIT)
        .ok_or_else(crate::decimal::overflow)
}

/// An integer sum as a partial state: its 16 bytes.
fn int_partial(i: i128) -> Value {
    Value::Bytea(i.to_le_bytes().to_vec().into())
}

/// The integer sum of an [`int_partial`].
fn int_of_partial(b: &[u8]) -> Result<i128> {
    Ok(i128::from_le_bytes(
        b.try_into()
            .map_err(|_| Error::internal("a malformed partial sum"))?,
    ))
}

/// Folds a value (not `NULL`) into a `min`, `max`, `bool_and` or
/// `bool_or`'s value so far (`NULL`: none).
fn fold(func: AggFunc, cur: &mut Value, v: Value) -> Result<()> {
    match func {
        AggFunc::Min | AggFunc::Max => {
            let replace = cur.is_null() || {
                let o = v.sql_cmp(cur)?.expect("not null");
                if func == AggFunc::Min {
                    o.is_lt()
                } else {
                    o.is_gt()
                }
            };
            if replace {
                *cur = v;
            }
        }
        _ => {
            let x = v.truth()?.expect("not null");
            *cur = Value::Bool(match *cur {
                Value::Bool(y) if func == AggFunc::BoolAnd => x && y,
                Value::Bool(y) => x || y,
                _ => x,
            });
        }
    }
    Ok(())
}

impl Grouped {
    fn new(func: AggFunc) -> Self {
        use AggFunc::*;
        let states = match func {
            Min | Max | BoolAnd | BoolOr => States::Values(Vec::new()),
            Sum | Avg => States::Unset,
            _ => States::Accs(Vec::new()),
        };
        Grouped {
            func,
            len: 0,
            states,
        }
    }

    /// The sums and rows of integers (`scale` `None`) or of decimals at
    /// `scale`: chosen if no state is yet, `None` if another is.
    fn sums(&mut self, scale: Option<u32>) -> Option<(&mut [i128], &mut [i64])> {
        if let States::Unset = self.states {
            self.states = States::Sums {
                scale,
                sums: vec![0; self.len],
                rows: vec![0; self.len],
            };
        }
        match &mut self.states {
            States::Sums {
                scale: s,
                sums,
                rows,
            } if *s == scale => Some((sums, rows)),
            _ => None,
        }
    }

    /// The states as [`Acc`]s, made from the typed ones if need be.
    fn accs(&mut self) -> &mut [Acc] {
        let func = self.func;
        let accs = match std::mem::replace(&mut self.states, States::Unset) {
            States::Accs(a) => a,
            States::Unset => (0..self.len).map(|_| Acc::new(func)).collect(),
            States::Values(v) => v
                .into_iter()
                .map(|value| Acc {
                    value,
                    ..Acc::new(func)
                })
                .collect(),
            States::Sums { scale, sums, rows } => sums
                .into_iter()
                .zip(rows)
                .map(|(sum, n)| {
                    let mut acc = Acc::new(func);
                    if n > 0 {
                        match scale {
                            None => acc.int = Some(sum),
                            Some(s) => {
                                acc.value = Value::Numeric(crate::decimal::Decimal::raw(sum, s));
                            }
                        }
                        if func == AggFunc::Avg {
                            acc.count = n;
                        }
                    }
                    acc
                })
                .collect(),
        };
        self.states = States::Accs(accs);
        match &mut self.states {
            States::Accs(a) => a,
            _ => unreachable!("just made"),
        }
    }
}

impl GroupAccumulator for Grouped {
    fn resize(&mut self, groups: usize) {
        let func = self.func;
        self.len = groups;
        match &mut self.states {
            States::Unset => {}
            States::Values(v) => v.resize(groups, Value::Null),
            States::Sums { sums, rows, .. } => {
                sums.resize(groups, 0);
                rows.resize(groups, 0);
            }
            States::Accs(a) => a.resize_with(groups, || Acc::new(func)),
        }
    }

    fn group_bytes(&self) -> usize {
        match &self.states {
            States::Values(_) => std::mem::size_of::<Value>(),
            States::Unset | States::Sums { .. } => {
                std::mem::size_of::<i128>() + std::mem::size_of::<i64>()
            }
            States::Accs(_) => std::mem::size_of::<Acc>(),
        }
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        use AggFunc::*;
        let func = self.func;
        let pairs = || {
            rows.iter()
                .zip(groups)
                .map(|(&r, &g)| (r as usize, g as usize))
        };
        if let States::Values(vals) = &mut self.states {
            let min = func == Min;
            match args.first() {
                Some(Vector::Int(v, _)) if matches!(func, Min | Max) => {
                    for (r, g) in pairs() {
                        let x = v[r];
                        match &mut vals[g] {
                            Value::Int(cur) => {
                                if (min && x < *cur) || (!min && x > *cur) {
                                    *cur = x;
                                }
                            }
                            Value::Null => vals[g] = Value::Int(x),
                            cur => fold(func, cur, Value::Int(x))?,
                        }
                    }
                }
                Some(Vector::Values(v)) => {
                    for (r, g) in pairs() {
                        fold(func, &mut vals[g], v[r].clone())?;
                    }
                }
                Some(a) => {
                    for (r, g) in pairs() {
                        fold(func, &mut vals[g], a.value(r))?;
                    }
                }
                None => {}
            }
            return Ok(());
        }
        match (func, args.first()) {
            (Sum | Avg, Some(Vector::Int(v, _))) => {
                if let Some((sums, n)) = self.sums(None) {
                    for (r, g) in pairs() {
                        sums[g] += i128::from(v[r]);
                        n[g] += 1;
                    }
                    return Ok(());
                }
            }
            (Sum | Avg, Some(Vector::Dec(s, v, _))) => {
                if let Some((sums, n)) = self.sums(Some(*s)) {
                    for (r, g) in pairs() {
                        sums[g] = add_mantissas(sums[g], v.at(r))?;
                        n[g] += 1;
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
        let accs = self.accs();
        match (func, args.first()) {
            (Count, _) => {
                for (_, g) in pairs() {
                    accs[g].count += 1;
                }
            }
            (Sum | Avg, Some(Vector::Int(v, _))) => {
                for (r, g) in pairs() {
                    accs[g].add_int(v[r]);
                }
            }
            (Sum | Avg, Some(Vector::Float(v, _))) => {
                for (r, g) in pairs() {
                    accs[g].add_float(v[r], false);
                }
            }
            (Sum | Avg, Some(Vector::Dec(s, v, _))) => {
                for (r, g) in pairs() {
                    accs[g].add_dec(crate::decimal::Decimal::raw(v.at(r), *s))?;
                }
            }
            // One argument of any values: each passed as it is.
            (_, Some(Vector::Values(vals))) if args.len() == 1 => {
                for (r, g) in pairs() {
                    accs[g].add(std::slice::from_ref(&vals[r]))?;
                }
            }
            (_, Some(_)) => {
                for (r, g) in pairs() {
                    let vals: Vec<Value> = args.iter().map(|a| a.value(r)).collect();
                    accs[g].add(&vals)?;
                }
            }
            (_, None) => {
                for (_, g) in pairs() {
                    accs[g].add(&[])?;
                }
            }
        }
        Ok(())
    }

    fn update_all(
        &mut self,
        len: usize,
        group: u32,
        skip_nulls: bool,
        args: &[&Vector],
    ) -> Result<()> {
        use AggFunc::*;
        let (func, g) = (self.func, group as usize);
        match (func, &mut self.states, args.first()) {
            (Min | Max, States::Values(vals), Some(Vector::Int(v, n)))
                if matches!(vals[g], Value::Int(_) | Value::Null) =>
            {
                let max = func == Max;
                if let Some(x) =
                    crate::kernels::minmax_i64(&v[..len], n.as_ref().map(|n| &n[..len]), max)
                {
                    vals[g] = match vals[g] {
                        Value::Int(cur) if (max && cur >= x) || (!max && cur <= x) => {
                            Value::Int(cur)
                        }
                        _ => Value::Int(x),
                    };
                }
                return Ok(());
            }
            (Sum | Avg, _, Some(Vector::Int(v, n))) => {
                if let Some((sums, rows)) = self.sums(None) {
                    let (sum, count) =
                        crate::kernels::sum_i64(&v[..len], n.as_ref().map(|n| &n[..len]));
                    sums[g] += sum;
                    rows[g] += count as i64;
                    return Ok(());
                }
            }
            (Sum | Avg, _, Some(Vector::Dec(s, v, n))) => {
                if let Some((sums, rows)) = self.sums(Some(*s)) {
                    for r in 0..len {
                        if !n.as_ref().is_some_and(|n| n[r]) {
                            sums[g] = add_mantissas(sums[g], v.at(r))?;
                            rows[g] += 1;
                        }
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
        let rows = crate::ext::all_rows(len, skip_nulls, args);
        self.update(&rows, &vec![group; rows.len()], args)
    }

    fn finish_keeps_state(&self) -> bool {
        !matches!(self.func, AggFunc::StringAgg | AggFunc::ArrayAgg)
    }

    fn combinable(&self) -> bool {
        // A string_agg's separator belongs to the row it follows, which a
        // partial state does not keep.
        self.func != AggFunc::StringAgg
    }

    /// A least, greatest or boolean value is its own partial state; a sum
    /// its value (an integer one's as its 16 bytes, [`int_partial`]); a
    /// mean the pair of its rows and its sum; any other an
    /// [`Acc::partial`].  `NULL`: no row.
    fn partial(&mut self, group: usize) -> Result<Value> {
        let func = self.func;
        let pair = |n: i64, sum: Value| match func {
            AggFunc::Avg => Value::Array(vec![Value::Int(n), sum]),
            _ => sum,
        };
        Ok(match &self.states {
            States::Unset => Value::Null,
            States::Values(v) => v[group].clone(),
            States::Sums { rows, .. } if rows[group] == 0 => Value::Null,
            States::Sums { scale, sums, rows } => pair(
                rows[group],
                match scale {
                    None => int_partial(sums[group]),
                    Some(s) => Value::Numeric(crate::decimal::Decimal::raw(sums[group], *s)),
                },
            ),
            States::Accs(accs) => {
                let acc = &accs[group];
                let sum = match (acc.int, &acc.value) {
                    _ if acc.float.is_some() || acc.items.is_some() => None,
                    (None, v) => Some(v.clone()),
                    (Some(i), Value::Null) => Some(int_partial(i)),
                    _ => None,
                };
                match (func, sum) {
                    (AggFunc::Sum, Some(s)) => s,
                    (AggFunc::Avg, Some(_)) if acc.count == 0 => Value::Null,
                    (AggFunc::Avg, Some(s)) => pair(acc.count, s),
                    _ => acc.partial(),
                }
            }
        })
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let func = self.func;
        if partial.is_null() {
            return Ok(());
        }
        if let States::Values(v) = &mut self.states {
            return fold(func, &mut v[group], partial);
        }
        let bad = || Error::internal("a malformed partial aggregate state");
        // A sum's value, or a mean's rows and sum; else an Acc's state.
        let (n, sum) = match (func, partial) {
            (AggFunc::Avg, Value::Array(a)) if a.len() == 2 => {
                let [n, sum] = <[Value; 2]>::try_from(a).map_err(|_| bad())?;
                let Value::Int(n) = n else {
                    return Err(bad());
                };
                (n, sum)
            }
            (AggFunc::Sum, sum) if !matches!(sum, Value::Array(_)) => (1, sum),
            (_, p) => return self.accs()[group].combine(p),
        };
        let typed = match &sum {
            Value::Bytea(b) => Some((None, int_of_partial(b)?)),
            Value::Numeric(d) => Some((Some(d.scale()), d.mantissa())),
            _ => None,
        };
        if let Some((scale, m)) = typed
            && let Some((sums, rows)) = self.sums(scale)
        {
            sums[group] = match scale {
                None => sums[group] + m,
                Some(_) => add_mantissas(sums[group], m)?,
            };
            rows[group] += n;
            return Ok(());
        }
        let acc = &mut self.accs()[group];
        if func == AggFunc::Avg {
            acc.count += n;
        }
        match sum {
            Value::Bytea(b) => acc.int = Some(acc.int.unwrap_or(0) + int_of_partial(&b)?),
            v if acc.value.is_null() => acc.value = v,
            v => {
                let s = std::mem::replace(&mut acc.value, Value::Null);
                acc.value = crate::expr::binary(BinaryOp::Add, s, v)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        let func = self.func;
        match &mut self.states {
            States::Unset => Ok(Value::Null),
            States::Values(v) => Ok(v[group].clone()),
            States::Sums { rows, .. } if rows[group] == 0 => Ok(Value::Null),
            States::Sums { scale, sums, rows } => {
                let sum = sums[group];
                Ok(match (func, *scale) {
                    (AggFunc::Sum, None) => Value::Int(
                        i64::try_from(sum)
                            .map_err(|_| Error::out_of_range("bigint out of range"))?,
                    ),
                    (AggFunc::Sum, Some(s)) => Value::Numeric(crate::decimal::Decimal::raw(sum, s)),
                    (_, s) => Value::Numeric(
                        crate::decimal::Decimal::new(sum, s.unwrap_or(0))?
                            .checked_div(crate::decimal::Decimal::from_i64(rows[group]))?,
                    ),
                })
            }
            States::Accs(a) => a[group].finish(),
        }
    }
}

impl Accumulator for Acc {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        let add = |a: Value, b: Value| crate::expr::binary(BinaryOp::Add, a, b);
        let v = args.first().cloned().unwrap_or(Value::Null);
        match (self.func, &v) {
            (AggFunc::Sum | AggFunc::Avg, Value::Int(i)) => {
                self.add_int(*i);
                return Ok(());
            }
            (AggFunc::Sum | AggFunc::Avg, Value::Float(f)) => {
                self.add_float(*f, false);
                return Ok(());
            }
            (AggFunc::Sum | AggFunc::Avg, Value::Real(f)) => {
                self.add_float(f64::from(*f), true);
                return Ok(());
            }
            // Decimals added as they are (no dispatch, no copy of the sum).
            (AggFunc::Sum | AggFunc::Avg, Value::Numeric(d))
                if matches!(self.value, Value::Numeric(_) | Value::Null) =>
            {
                self.value = match self.value {
                    Value::Numeric(sum) => Value::Numeric(sum.checked_add(*d)?),
                    _ => Value::Numeric(*d),
                };
                if self.func == AggFunc::Avg {
                    self.count += 1;
                }
                return Ok(());
            }
            _ => {}
        }
        match self.func {
            AggFunc::Count => self.count += 1,
            AggFunc::Sum => {
                self.value = match (&self.value, v) {
                    (
                        Value::Null,
                        v @ (Value::Int(_) | Value::Real(_) | Value::Float(_) | Value::Numeric(_)),
                    ) => v,
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
                    Value::Real(f) => Value::Float(f64::from(f)),
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
            AggFunc::Min | AggFunc::Max | AggFunc::BoolAnd | AggFunc::BoolOr => {
                fold(self.func, &mut self.value, v)?
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
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Ok(match self.func {
            AggFunc::Count => Value::Int(self.count),
            AggFunc::Sum => self.total()?,
            // Doubles alone: their exact sum divided, rounded once.
            AggFunc::Avg => match (&self.float, self.int, &self.value) {
                (Some(f), None, Value::Null) if self.count > 0 => Value::Float(
                    f.mean(self.count as u64)
                        .ok_or_else(crate::types::float_overflow)?,
                ),
                _ => match self.total()? {
                    _ if self.count == 0 => Value::Null,
                    Value::Numeric(d) => Value::Numeric(
                        d.checked_div(crate::decimal::Decimal::from_i64(self.count))?,
                    ),
                    Value::Interval(i) => Value::Interval(i.checked_div(self.count as f64)?),
                    Value::Float(f) => Value::Float(f / self.count as f64),
                    _ => Value::Null,
                },
            },
            AggFunc::StringAgg => self.text.take().map_or(Value::Null, Value::Text),
            AggFunc::ArrayAgg => self.items.take().map_or(Value::Null, Value::Array),
            _ => self.value.clone(),
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
        let ty = common_type(&args[..2])?.unwrap_or(DataType::BigInt);
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

    fn stream(
        &self,
        _: &CallContext<'_>,
        args: &[Value],
    ) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
        crate::exec::series(args)
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
    (DataType::SmallInt, &["smallint", "int2"], "number"),
    (DataType::Integer, &["integer", "int", "int4"], "number"),
    (DataType::BigInt, &["bigint", "int8"], "number"),
    (DataType::Real, &["real", "float4"], "number"),
    (
        DataType::Float,
        &["double precision", "float", "float8", "double"],
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
    (DataType::Char, &["character", "char"], "text"),
    (DataType::Bytea, &["bytea"], "binary"),
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
    (
        DataType::Time,
        &["time", "time without time zone"],
        "datetime",
    ),
    (DataType::Interval, &["interval"], "interval"),
    (DataType::Inet, &["inet"], "network"),
    (DataType::Cidr, &["cidr"], "network"),
];

/// A built-in type as a [`Type`]: its values are native, and its abilities
/// are the engine's own code (the hot paths call it directly).
struct BuiltinType {
    ty: DataType,
    names: Vec<&'static str>,
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
    fn examples(&self) -> Vec<Value> {
        examples(self.ty)
    }
}

/// Edge cases of a built-in type.
fn examples(ty: DataType) -> Vec<Value> {
    use crate::datetime::Interval;
    use crate::decimal::Decimal;
    match ty {
        DataType::Bool => vec![Value::Bool(false), Value::Bool(true)],
        DataType::SmallInt | DataType::Integer | DataType::BigInt => {
            let (lo, hi) = ty.int_range().expect("an integer type");
            [0, 1, -1, lo, hi, lo + 1, hi - 1].map(Value::Int).to_vec()
        }
        DataType::Real => [
            0.0,
            -0.0,
            1.5,
            0.1,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MAX,
            f32::MIN_POSITIVE,
            1e-45,
        ]
        .map(Value::Real)
        .to_vec(),
        DataType::Float => [
            0.0,
            -0.0,
            1.5,
            -2.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
        ]
        .map(Value::Float)
        .to_vec(),
        DataType::Numeric => [
            (0, 0),
            (150, 2),
            (15, 1),
            (-1, 0),
            (i128::from(i64::MAX), 3),
        ]
        .iter()
        .map(|&(m, s)| Value::Numeric(Decimal::new(m, s).expect("in range")))
        .collect(),
        DataType::Text => ["", "a", "a\u{0}b", "é", "{a,\"b\"}", "NULL"]
            .map(|s| Value::Text(s.to_string()))
            .to_vec(),
        DataType::Char => ["", " ", "a", "a ", "a\u{0}", "é  ", "b"]
            .map(|s| Value::Char(s.to_string()))
            .to_vec(),
        DataType::Bytea => [&b""[..], b"\0", b"\0\0", b"\0\x01", b"a", b"a\0", b"\xff"]
            .map(|b| Value::Bytea(b.into()))
            .to_vec(),
        DataType::Date => [0, 1, -1, -2_451_000, 100_000_000]
            .map(Value::Date)
            .to_vec(),
        DataType::Timestamp => [0, 1, -1, i64::MAX / 4].map(Value::Timestamp).to_vec(),
        DataType::Time => [0, 1, 43_200_000_000, 86_399_999_999, 86_400_000_000]
            .map(Value::Time)
            .to_vec(),
        DataType::TimestampTz => [0, 1, -1, i64::MAX / 4].map(Value::TimestampTz).to_vec(),
        DataType::Inet => [
            "0.0.0.0/0",
            "10.0.0.5/8",
            "10.0.0.5",
            "255.255.255.255",
            "::",
            "::1/64",
            "ffff::1",
        ]
        .map(|s| Value::Inet(crate::inet::parse_inet(s).expect("an inet")))
        .to_vec(),
        DataType::Cidr => [
            "0.0.0.0/0",
            "10.0.0.0/7",
            "10.0.0.0/8",
            "10.1.2.3/32",
            "::/0",
            "2001:db8::/32",
        ]
        .map(|s| Value::Cidr(crate::inet::parse_cidr(s).expect("a cidr")))
        .to_vec(),
        DataType::Interval => [(0, 0, 0), (1, 0, 0), (0, 30, 0), (-1, 2, -3_000_000)]
            .iter()
            .map(|&(months, days, micros)| {
                Value::Interval(Interval {
                    months,
                    days,
                    micros,
                })
            })
            .collect(),
        DataType::Array(e) => {
            let mut out = vec![Value::Array(Vec::new()), Value::Array(vec![Value::Null])];
            for v in examples(*e).into_iter().take(3) {
                out.push(Value::Array(vec![v.clone()]));
                out.push(Value::Array(vec![v.clone(), Value::Null]));
                out.push(Value::Array(vec![Value::Null, v]));
            }
            out
        }
        DataType::Ext(_) | DataType::Enum(_) => Vec::new(),
    }
}

/// Every enum's [`Type`]: native values (a sort key and a label), ordered
/// by declaration.  Its labels are the database's (the catalog's), so it
/// parses none itself.
struct EnumType;

impl Type for EnumType {
    fn name(&self) -> &str {
        "enum"
    }
    fn category(&self) -> &str {
        "enum"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Err(Error::internal(format!(
            "an enum label ({text:?}) is read through the catalog"
        )))
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

/// The [`Type`] behind any type: a built-in one's descriptor (and its
/// array's), or a registered type's own.
pub fn type_of(ty: DataType) -> &'static dyn Type {
    static ALL: OnceLock<Vec<BuiltinType>> = OnceLock::new();
    if let DataType::Ext(t) = ty {
        return t.imp();
    }
    if let DataType::Enum(_) = ty {
        return &EnumType;
    }
    let all = ALL.get_or_init(|| {
        let mut all: Vec<BuiltinType> = TYPES
            .iter()
            .map(|(ty, names, category)| BuiltinType {
                ty: *ty,
                names: names.to_vec(),
                category,
            })
            .collect();
        for (ty, _, _) in TYPES {
            let array = ty.array_of().expect("built-in types have arrays");
            all.push(BuiltinType {
                ty: array,
                names: vec![array.name()],
                category: "array",
            });
        }
        all
    });
    all.iter()
        .find(|b| b.ty == ty)
        .expect("every built-in type has a descriptor")
}
