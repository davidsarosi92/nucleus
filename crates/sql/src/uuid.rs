//! `uuid` (PLAN.md §M14.19): the first type built as a registered type —
//! values are [`Value::Ext`] with 16 canonical bytes — with its functions:
//! `gen_random_uuid()` / `uuidv4()` (version 4, random), `uuidv7()`
//! (version 7, time-ordered: new values land at the end of an index),
//! `uuid_extract_version` and `uuid_extract_timestamp`.

use std::cmp::Ordering;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, CanonicalBytes, Equality, Hashing, KeyEncoding, Registry, Returns,
    ScalarFunction, Signature, TotalOrder, Type, TypeRef,
};
use crate::types::{DataType, Value};

/// The `uuid` type.
pub struct Uuid;

/// The interned `uuid` type (the built-in module registers it).
pub fn uuid_type() -> TypeRef {
    TypeRef::named("uuid").expect("the built-in module registers uuid")
}

/// A uuid value from its 16 bytes.
pub fn value(b: [u8; 16]) -> Value {
    Value::ext(uuid_type(), b.to_vec())
}

fn bytes(v: &Value) -> Result<[u8; 16]> {
    v.ext_bytes()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::internal("not a uuid"))
}

/// Parses PostgreSQL's input forms: 32 hex digits, upper or lower case, a
/// hyphen allowed after any group of four, optionally in braces.
pub fn parse(text: &str) -> Result<[u8; 16]> {
    let bad = || Error::invalid_text(format!("invalid input syntax for type uuid: \"{text}\""));
    let s = text.trim();
    let s = match s.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}').ok_or_else(bad)?,
        None => s,
    };
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut hyphen_ok = false;
    for c in s.chars() {
        if c == '-' {
            if !hyphen_ok {
                return Err(bad());
            }
            hyphen_ok = false;
            continue;
        }
        let d = c.to_digit(16).ok_or_else(bad)? as u8;
        if n == 32 {
            return Err(bad());
        }
        out[n / 2] |= if n % 2 == 0 { d << 4 } else { d };
        n += 1;
        hyphen_ok = n % 4 == 0 && n < 32;
    }
    if n != 32 || s.ends_with('-') {
        return Err(bad());
    }
    Ok(out)
}

/// The canonical text form: lower case, `8-4-4-4-12`.
pub fn format(b: &[u8; 16]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Byte order (PostgreSQL's: `memcmp`).
struct Bytewise;

impl TotalOrder for Bytewise {
    fn compare(&self, a: &Value, b: &Value) -> Result<Ordering> {
        Ok(a.ext_bytes().cmp(&b.ext_bytes()))
    }
}

/// The 16 bytes as they are: fixed width, so self-delimiting, and their
/// order is the type's.
struct Raw;

impl KeyEncoding for Raw {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        out.extend_from_slice(v.ext_bytes().unwrap_or_default());
    }
    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        let raw: [u8; 16] = b
            .get(..16)
            .and_then(|x| x.try_into().ok())
            .ok_or_else(|| Error::internal("malformed uuid key"))?;
        Ok((value(raw), 16))
    }
}

impl Type for Uuid {
    fn name(&self) -> &str {
        "uuid"
    }
    fn category(&self) -> &str {
        "uuid"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Ok(value(parse(text)?))
    }
    fn output(&self, v: &Value) -> String {
        bytes(v).map_or_else(|_| "?".into(), |b| format(&b))
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&Bytewise)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Raw)
    }
    fn examples(&self) -> Vec<Value> {
        let mut max = [0xffu8; 16];
        max[0] = 0x7f;
        [
            [0u8; 16],
            [0xff; 16],
            max,
            parse("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").expect("valid"),
        ]
        .map(value)
        .to_vec()
    }
}

// ------------------------------------------------------------- functions

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Random,
    V7,
    Version,
    Timestamp,
}

struct UuidFn(F, &'static str);

/// Microseconds from 1970 to 2000 (nucleus timestamps count from 2000).
const UNIX_TO_2000_MICROS: i64 = 946_684_800_000_000;

/// The last version-7 value made in this process (unix ms << 12 |
/// sub-millisecond bits), so values made here only increase.
static LAST_V7: AtomicU64 = AtomicU64::new(0);

impl ScalarFunction for UuidFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::Random | F::V7 => 0..=0,
            F::Version | F::Timestamp => 1..=1,
        }
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        let u = DataType::Ext(uuid_type());
        Ok(match self.0 {
            F::Random | F::V7 => Signature::keep(0, Some(u)),
            F::Version => Signature::new(vec![Arg::Untyped(u)], Returns::Type(Some(DataType::Int))),
            F::Timestamp => Signature::new(
                vec![Arg::Untyped(u)],
                Returns::Type(Some(DataType::TimestampTz)),
            ),
        })
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let mut b = [0u8; 16];
        Ok(match self.0 {
            F::Random => {
                cx.random(&mut b)?;
                set_version(&mut b, 4);
                value(b)
            }
            F::V7 => {
                let unix_us = cx.clock()? + UNIX_TO_2000_MICROS;
                let ms = (unix_us.max(0) / 1000) as u64;
                // The fraction of the millisecond in 12 bits, as
                // PostgreSQL does, and never below the last value made.
                let sub = ((unix_us.max(0) % 1000) as u64 * 4096) / 1000;
                let now = (ms << 12) | sub;
                let last =
                    LAST_V7.update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |last| {
                        now.max(last + 1)
                    });
                let next = now.max(last + 1);
                cx.random(&mut b)?;
                let (ms, sub) = (next >> 12, next & 0xfff);
                b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
                b[6] = (sub >> 8) as u8;
                b[7] = sub as u8;
                set_version(&mut b, 7);
                value(b)
            }
            F::Version => {
                let b = bytes(&args[0])?;
                if b[8] & 0xc0 == 0x80 {
                    Value::Int(i64::from(b[6] >> 4))
                } else {
                    Value::Null
                }
            }
            F::Timestamp => {
                let b = bytes(&args[0])?;
                if b[8] & 0xc0 != 0x80 {
                    return Ok(Value::Null);
                }
                match b[6] >> 4 {
                    7 => {
                        let mut ms = [0u8; 8];
                        ms[2..].copy_from_slice(&b[..6]);
                        let unix_ms = u64::from_be_bytes(ms) as i64;
                        Value::TimestampTz(unix_ms * 1000 - UNIX_TO_2000_MICROS)
                    }
                    1 => {
                        // 60 bits of 100 ns since 1582-10-15: time_low,
                        // time_mid, time_hi.
                        let low = u64::from(u32::from_be_bytes(b[0..4].try_into().unwrap()));
                        let mid = u64::from(u16::from_be_bytes([b[4], b[5]]));
                        let hi = u64::from(u16::from_be_bytes([b[6], b[7]]) & 0x0fff);
                        let ticks = (hi << 48) | (mid << 32) | low;
                        const GREGORIAN_TO_UNIX_TICKS: i64 = 122_192_928_000_000_000;
                        let unix_us = (ticks as i64 - GREGORIAN_TO_UNIX_TICKS) / 10;
                        Value::TimestampTz(unix_us - UNIX_TO_2000_MICROS)
                    }
                    _ => Value::Null,
                }
            }
        })
    }
}

/// Sets the version nibble and the RFC 9562 variant.
fn set_version(b: &mut [u8; 16], version: u8) {
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
}

/// Registers the type and its functions (part of the built-in module).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(Uuid))?;
    r.add_function(
        &["gen_random_uuid", "uuidv4"],
        Arc::new(UuidFn(F::Random, "gen_random_uuid")),
    )?;
    r.add_function(&["uuidv7"], Arc::new(UuidFn(F::V7, "uuidv7")))?;
    r.add_function(
        &["uuid_extract_version"],
        Arc::new(UuidFn(F::Version, "uuid_extract_version")),
    )?;
    r.add_function(
        &["uuid_extract_timestamp"],
        Arc::new(UuidFn(F::Timestamp, "uuid_extract_timestamp")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_forms() {
        let want = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        for s in [
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11",
            "{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}",
        ] {
            assert_eq!(format(&parse(s).unwrap()), want, "{s}");
        }
        for s in [
            "",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a111",
            "-a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11-",
            "a0eeb-c999c0b4ef8bb6d6bb9bd380a11",
            "a0ee--bc999c0b4ef8bb6d6bb9bd380a11",
            "{a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "g0eebc999c0b4ef8bb6d6bb9bd380a11",
        ] {
            assert!(parse(s).is_err(), "{s:?} accepted");
        }
    }
}
