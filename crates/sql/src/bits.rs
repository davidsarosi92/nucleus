//! The integers' bitwise operators (issue #16): `&` (and), `|` (or), `#`
//! (exclusive or), `<<` and `>>` (shifts) — `~` (not) is the grammar's,
//! a prefix.  As PostgreSQL's: `&`, `|` and `#` on two integers of
//! the wider type; a shift of the left operand's type, by its count
//! modulo the bits of the type it is computed in (a C shift on x86, as
//! PostgreSQL's are), never an error.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    And,
    Or,
    Xor,
    Shl,
    Shr,
}

struct BitOp {
    op: Op,
    /// The left operand's type, the type a shift is computed in.
    left: DataType,
    /// The result's type.
    ty: DataType,
    name: &'static str,
}

/// The wider of two integer types.
fn wider(a: DataType, b: DataType) -> DataType {
    let rank = |t| match t {
        DataType::SmallInt => 0,
        DataType::Integer => 1,
        _ => 2,
    };
    if rank(a) >= rank(b) { a } else { b }
}

/// `x << n` or `x >> n` computed as PostgreSQL computes it for `ty`: a
/// `smallint` as a 32-bit `int` cut back to 16 bits, an `integer` in 32,
/// a `bigint` in 64; the count taken modulo the width.
fn shift(x: i64, n: i64, ty: DataType, left: bool) -> i64 {
    let n = n as u32;
    match ty {
        DataType::SmallInt | DataType::Integer => {
            let x = x as i32;
            let v = if left {
                x.wrapping_shl(n)
            } else {
                x.wrapping_shr(n)
            };
            if ty == DataType::SmallInt {
                i64::from(v as i16)
            } else {
                i64::from(v)
            }
        }
        _ => {
            if left {
                x.wrapping_shl(n)
            } else {
                x.wrapping_shr(n)
            }
        }
    }
}

impl ScalarFunction for BitOp {
    fn name(&self) -> &str {
        self.name
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Keep, Arg::Keep],
            Returns::Type(Some(self.ty)),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (Value::Int(a), Value::Int(b)) = (&args[0], &args[1]) else {
            return Err(Error::internal(format!("{} of non-integers", self.name)));
        };
        Ok(Value::Int(match self.op {
            Op::And => a & b,
            Op::Or => a | b,
            Op::Xor => a ^ b,
            Op::Shl => shift(*a, *b, self.left, true),
            Op::Shr => shift(*a, *b, self.left, false),
        }))
    }
}

/// Registers the operators for every pair of integer types.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    use DataType::{BigInt, Integer, SmallInt};
    for (op, symbol, name) in [
        (Op::And, "&", "int_and"),
        (Op::Or, "|", "int_or"),
        (Op::Xor, "#", "int_xor"),
        (Op::Shl, "<<", "int_shl"),
        (Op::Shr, ">>", "int_shr"),
    ] {
        for left in [SmallInt, Integer, BigInt] {
            for right in [SmallInt, Integer, BigInt] {
                let ty = if matches!(op, Op::Shl | Op::Shr) {
                    left
                } else {
                    wider(left, right)
                };
                let f = BitOp { op, left, ty, name };
                r.add_operator(symbol, left, right, Arc::new(f))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use DataType::{BigInt, Integer, SmallInt};

    #[test]
    fn shifts_as_postgresql() {
        assert_eq!(shift(1, 31, Integer, true), i64::from(i32::MIN));
        assert_eq!(shift(1, 32, Integer, true), 1);
        assert_eq!(shift(1, 16, SmallInt, true), 0);
        assert_eq!(shift(-1, 20, SmallInt, false), -1);
        assert_eq!(shift(1, 63, BigInt, true), i64::MIN);
        assert_eq!(shift(1, 64, BigInt, true), 1);
        assert_eq!(shift(-16, 2, BigInt, false), -4);
        assert_eq!(shift(5, -1, Integer, true), i64::from(i32::MIN));
        assert_eq!(wider(SmallInt, BigInt), BigInt);
    }
}
