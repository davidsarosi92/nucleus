//! Buckets (PLAN.md §M14.27 S5): `width_bucket` — PostgreSQL's, by equal
//! widths over doubles or (exactly) decimals, or by an array of lower
//! bounds over any ordered type — and the `histogram` aggregate, the
//! count of each of its buckets.
//!
//! `histogram(x, low, high, n)` is `n + 2` counts: element `i` (from 1)
//! counts the rows `width_bucket(x, low, high, n)` puts in bucket
//! `i − 1` — the first those below `low`, the last those from `high` on.
//! `histogram(x, bounds)` is `len(bounds) + 1` counts the same way.
//! Equal-depth buckets are quantiles' (`percentile_disc` of an array of
//! fractions gives their bounds).  The counts of a group's rows add up
//! whichever worker counted them, so a histogram is aggregated in
//! parallel.

use std::cmp::Ordering;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::bigint::BigInt;
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, GroupAccumulator, Registry, Returns,
    ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};
use crate::vector::Vector;

/// Registers `width_bucket` and `histogram`.
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_function(&["width_bucket"], Arc::new(WidthBucket))?;
    r.add_aggregate(&["histogram"], Arc::new(Histogram))?;
    Ok(())
}

/// How the equal-width form computes: in doubles, or exactly in decimals
/// (PostgreSQL's two `width_bucket`s; integers take the doubles', its
/// preferred numeric type).
fn equal_width(name: &str, args: &[ArgInfo]) -> Result<Vec<Arg>> {
    use DataType::*;
    let ty = |a: &ArgInfo| a.ty;
    if args[..3]
        .iter()
        .any(|a| ty(a).is_some_and(|t| !t.is_number()))
    {
        return Err(no_such(name, args));
    }
    if ty(&args[3]).is_some_and(|t| !t.is_integer()) {
        return Err(no_such(name, args));
    }
    let numeric = args[..3].iter().any(|a| ty(a) == Some(Numeric))
        && !args[..3]
            .iter()
            .any(|a| matches!(ty(a), Some(Float | Real)));
    let t = if numeric { Numeric } else { Float };
    let conv = |a: &ArgInfo| {
        if a.ty.is_some() {
            Arg::Convert(t)
        } else {
            Arg::Untyped(t)
        }
    };
    Ok(vec![
        conv(&args[0]),
        conv(&args[1]),
        conv(&args[2]),
        if args[3].ty.is_some() {
            Arg::Convert(Integer)
        } else {
            Arg::Untyped(Integer)
        },
    ])
}

fn by_bounds(name: &str, args: &[ArgInfo]) -> Result<Vec<Arg>> {
    match args[1].ty {
        None | Some(DataType::Array(_)) => Ok(vec![Arg::Keep, Arg::Keep]),
        Some(_) => Err(no_such(name, args)),
    }
}

fn no_such(name: &str, args: &[ArgInfo]) -> Error {
    let names: Vec<&str> = args
        .iter()
        .map(|a| {
            a.ty.map_or("unknown", |t| crate::builtin::type_of(t).name())
        })
        .collect();
    Error::undefined_function(format!(
        "function {name}({}) does not exist",
        names.join(", ")
    ))
    .with_hint(format!(
        "{name} takes (value, low, high, count) over numbers, or (value, array of lower bounds)."
    ))
}

fn invalid(message: &str) -> Error {
    Error::invalid_width_bucket(message)
}

/// The buckets of a call: equal widths, or lower bounds.
#[derive(Clone, Debug)]
enum Bounds {
    Float {
        low: f64,
        high: f64,
        count: i32,
    },
    Numeric {
        low: Decimal,
        high: Decimal,
        count: i32,
    },
    Lower(Vec<Value>),
}

impl Bounds {
    /// From the arguments after the value (`NULL` ones give `None`).
    fn of(args: &[Value]) -> Result<Option<Bounds>> {
        if args.iter().any(Value::is_null) {
            return Ok(None);
        }
        let count = |v: &Value| -> Result<i32> {
            let c = match v {
                Value::Int(c) => {
                    i32::try_from(*c).map_err(|_| Error::out_of_range("integer out of range"))?
                }
                _ => return Err(Error::internal("width_bucket count not an integer")),
            };
            if c <= 0 {
                return Err(invalid("count must be greater than zero"));
            }
            Ok(c)
        };
        Ok(Some(match args {
            [Value::Array(bounds)] => {
                if bounds.iter().any(Value::is_null) {
                    return Err(Error::null_value("thresholds array must not contain NULLs"));
                }
                Bounds::Lower(bounds.clone())
            }
            [Value::Float(low), Value::Float(high), c] => {
                let count = count(c)?;
                if low.is_nan() || high.is_nan() {
                    return Err(invalid(
                        "operand, lower bound, and upper bound cannot be NaN",
                    ));
                }
                if low.is_infinite() || high.is_infinite() {
                    return Err(invalid("lower and upper bounds must be finite"));
                }
                if low == high {
                    return Err(invalid("lower bound cannot equal upper bound"));
                }
                Bounds::Float {
                    low: *low,
                    high: *high,
                    count,
                }
            }
            [Value::Numeric(low), Value::Numeric(high), c] => {
                let count = count(c)?;
                if low == high {
                    return Err(invalid("lower bound cannot equal upper bound"));
                }
                Bounds::Numeric {
                    low: *low,
                    high: *high,
                    count,
                }
            }
            _ => {
                return Err(Error::internal(
                    "width_bucket arguments of unexpected types",
                ));
            }
        }))
    }

    /// How many counts a histogram over these buckets has.
    fn len(&self) -> usize {
        match self {
            Bounds::Float { count, .. } | Bounds::Numeric { count, .. } => *count as usize + 2,
            Bounds::Lower(b) => b.len() + 1,
        }
    }

    /// The bucket of `x` (not `NULL`).
    fn bucket(&self, x: &Value) -> Result<i32> {
        match (self, x) {
            (Bounds::Float { low, high, count }, Value::Float(x)) => {
                float_bucket(*x, *low, *high, *count)
            }
            (Bounds::Numeric { low, high, count }, Value::Numeric(x)) => {
                numeric_bucket(*x, *low, *high, *count)
            }
            (Bounds::Lower(b), x) => {
                let n = b.partition_point(|t| t.sort_cmp(x) != Ordering::Greater);
                i32::try_from(n).map_err(|_| Error::out_of_range("integer out of range"))
            }
            _ => Err(Error::internal(
                "width_bucket operand of an unexpected type",
            )),
        }
    }
}

fn past(count: i32) -> Result<i32> {
    count
        .checked_add(1)
        .ok_or_else(|| Error::out_of_range("integer out of range"))
}

/// PostgreSQL's `width_bucket_float8`, step for step.
fn float_bucket(x: f64, low: f64, high: f64, count: i32) -> Result<i32> {
    if x.is_nan() {
        return Err(invalid(
            "operand, lower bound, and upper bound cannot be NaN",
        ));
    }
    let c = f64::from(count);
    let inside = |num: f64, den: f64, half: f64| -> i32 {
        let q = if den.is_infinite() { half } else { num / den };
        let r = (c * q) as i32;
        r.min(count - 1) + 1
    };
    Ok(if low < high {
        if x < low {
            0
        } else if x >= high {
            past(count)?
        } else {
            inside(
                x - low,
                high - low,
                (x / 2.0 - low / 2.0) / (high / 2.0 - low / 2.0),
            )
        }
    } else if x > low {
        0
    } else if x <= high {
        past(count)?
    } else {
        inside(
            low - x,
            low - high,
            (low / 2.0 - x / 2.0) / (low / 2.0 - high / 2.0),
        )
    })
}

/// PostgreSQL's `width_bucket_numeric`: `floor(count·(x − low) / (high −
/// low)) + 1`, exact.
fn numeric_bucket(x: Decimal, low: Decimal, high: Decimal, count: i32) -> Result<i32> {
    let (from, to, ascending) = if low < high {
        (low, high, true)
    } else {
        (high, low, false)
    };
    let below = if ascending { x < from } else { x > to };
    let above = if ascending { x >= to } else { x <= from };
    if below {
        return Ok(0);
    }
    if above {
        return past(count);
    }
    let (num, den) = if ascending {
        (x.checked_sub(low)?, high.checked_sub(low)?)
    } else {
        (low.checked_sub(x)?, low.checked_sub(high)?)
    };
    let scale = num.scale().max(den.scale());
    let big = |d: Decimal| BigInt::from_i128(d.mantissa()).mul_pow10(scale - d.scale());
    let (q, _) = big(num)
        .mul(&BigInt::from_i128(i128::from(count)))
        .div_big(&big(den));
    let q = q.to_i128().unwrap_or(i128::MAX);
    Ok(q.min(i128::from(count) - 1) as i32 + 1)
}

struct WidthBucket;

impl ScalarFunction for WidthBucket {
    fn name(&self) -> &str {
        "width_bucket"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=4
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let sig = match args.len() {
            4 => equal_width("width_bucket", args)?,
            2 => by_bounds("width_bucket", args)?,
            _ => return Err(no_such("width_bucket", args)),
        };
        Ok(Signature::new(sig, Returns::Type(Some(DataType::Integer))))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let Some(b) = Bounds::of(&args[1..])? else {
            return Ok(Value::Null);
        };
        if args[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Int(i64::from(b.bucket(&args[0])?)))
    }
}

struct Histogram;

impl Aggregate for Histogram {
    fn name(&self) -> &str {
        "histogram"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=4
    }

    fn order_free(&self) -> bool {
        true
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let sig = match args.len() {
            4 => equal_width("histogram", args)?,
            2 => by_bounds("histogram", args)?,
            _ => return Err(no_such("histogram", args)),
        };
        Ok(Signature::new(
            sig,
            Returns::Type(Some(DataType::Array(&DataType::BigInt))),
        ))
    }

    fn start(&self) -> Box<dyn Accumulator> {
        let mut g = Counts::default();
        g.resize(1);
        Box::new(OneHistogram(g))
    }

    fn grouped(&self) -> Option<Box<dyn GroupAccumulator>> {
        Some(Box::new(Counts::default()))
    }
}

/// A histogram's groups: their buckets (from their first row; none for a
/// state only combined into) and counts.
#[derive(Default)]
struct Counts {
    groups: Vec<Option<(Option<Bounds>, Vec<i64>)>>,
}

impl Counts {
    fn add(&mut self, g: usize, x: &Value, rest: impl FnOnce() -> Vec<Value>) -> Result<()> {
        let slot = &mut self.groups[g];
        if !matches!(slot, Some((Some(_), _))) {
            let Some(b) = Bounds::of(&rest())? else {
                return Ok(()); // NULL bounds: the row is not counted
            };
            let counts = match slot.take() {
                Some((_, c)) if c.len() == b.len() => c,
                _ => vec![0; b.len()],
            };
            *slot = Some((Some(b), counts));
        }
        let Some((Some(b), counts)) = slot else {
            unreachable!()
        };
        counts[b.bucket(x)? as usize] += 1;
        Ok(())
    }
}

impl GroupAccumulator for Counts {
    fn resize(&mut self, groups: usize) {
        if groups > self.groups.len() {
            self.groups.resize(groups, None);
        }
    }

    fn update(&mut self, rows: &[u32], groups: &[u32], args: &[&Vector]) -> Result<()> {
        for (&r, &g) in rows.iter().zip(groups) {
            let (r, g) = (r as usize, g as usize);
            if let (Vector::Float(d, _), Some((Some(Bounds::Float { low, high, count }), counts))) =
                (args[0], &mut self.groups[g])
            {
                counts[float_bucket(d[r], *low, *high, *count)? as usize] += 1;
                continue;
            }
            self.add(g, &args[0].value(r), || {
                args[1..].iter().map(|a| a.value(r)).collect()
            })?;
        }
        Ok(())
    }

    fn combinable(&self) -> bool {
        true
    }

    fn partial(&mut self, group: usize) -> Result<Value> {
        Ok(match &self.groups[group] {
            None => Value::Null,
            Some((_, counts)) => Value::Array(counts.iter().map(|&c| Value::Int(c)).collect()),
        })
    }

    fn combine(&mut self, group: usize, partial: Value) -> Result<()> {
        let Value::Array(items) = partial else {
            return Ok(());
        };
        let add = items.iter().map(|v| match v {
            Value::Int(c) => *c,
            _ => 0,
        });
        match &mut self.groups[group] {
            Some((_, counts)) => {
                for (c, a) in counts.iter_mut().zip(add) {
                    *c += a;
                }
            }
            slot => *slot = Some((None, add.collect())),
        }
        Ok(())
    }

    fn finish(&mut self, group: usize) -> Result<Value> {
        self.partial(group)
    }

    fn finish_keeps_state(&self) -> bool {
        true
    }
}

struct OneHistogram(Counts);

impl Accumulator for OneHistogram {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.0.add(0, &args[0], || args[1..].to_vec())
    }

    fn finish(&mut self) -> Result<Value> {
        GroupAccumulator::finish(&mut self.0, 0)
    }
}
