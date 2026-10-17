//! The built-in module: every built-in scalar function, the `public` and
//! `nc_catalog` schemas and the `nc_catalog` views — registered through the
//! extension API ([`crate::ext`]) like any other module.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use crate::ast::BinaryOp;
use crate::binder::arith_type;
use crate::error::{Error, Result};
use crate::expr::Func;
use crate::ext::{
    Arg, ArgInfo, CallContext, FnRef, Module, Registry, Returns, ScalarFunction, Signature,
};
use crate::types::{DataType, Value};

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
