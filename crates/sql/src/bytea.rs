//! `bytea`, binary strings (PLAN.md §M14.26): the text forms, `encode` /
//! `decode`, digests, and the byte functions.
//!
//! Input is PostgreSQL's: `\x` then hex digit pairs (spaces between pairs
//! allowed), or the escape form — any character as its UTF-8 bytes, `\\`
//! for a backslash, `\ooo` for an octal byte.  Output is the hex form
//! (`\xdeadbeef`).  `length`, `octet_length`, `substring`, `position` and
//! `||` work on bytes (their text versions dispatch here).

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};

/// Parses a `bytea` literal (either form).
pub fn parse(s: &str) -> Result<Vec<u8>> {
    match s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        Some(hex) => parse_hex(hex),
        None => parse_escape(s),
    }
}

fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut digits = s.chars().filter(|c| !c.is_ascii_whitespace());
    while let Some(hi) = digits.next() {
        let Some(lo) = digits.next() else {
            return Err(Error::invalid_parameter(
                "invalid hexadecimal data: odd number of digits",
            ));
        };
        out.push(hex_digit(hi)? << 4 | hex_digit(lo)?);
    }
    Ok(out)
}

fn hex_digit(c: char) -> Result<u8> {
    c.to_digit(16)
        .map(|d| d as u8)
        .ok_or_else(|| Error::invalid_parameter(format!("invalid hexadecimal digit: \"{c}\"")))
}

fn parse_escape(s: &str) -> Result<Vec<u8>> {
    let bad = || Error::invalid_text("invalid input syntax for type bytea");
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if b.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else {
            let oct = b.get(i + 1..i + 4).ok_or_else(bad)?;
            if !(b'0'..=b'3').contains(&oct[0])
                || !oct[1..].iter().all(|d| (b'0'..=b'7').contains(d))
            {
                return Err(bad());
            }
            out.push((oct[0] - b'0') << 6 | (oct[1] - b'0') << 3 | (oct[2] - b'0'));
            i += 4;
        }
    }
    Ok(out)
}

/// The hex output form: `\x` and two lower-case digits per byte.
pub fn format_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + 2 * b.len());
    s.push_str("\\x");
    s.push_str(&hex(b));
    s
}

fn hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 * b.len());
    for &x in b {
        s.push(DIGITS[usize::from(x >> 4)] as char);
        s.push(DIGITS[usize::from(x & 15)] as char);
    }
    s
}

/// `encode(b, 'escape')`: printable ASCII as itself, a backslash doubled,
/// every other byte as `\ooo`.
fn encode_escape(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &x in b {
        match x {
            b'\\' => s.push_str("\\\\"),
            0 | 0x80..=0xff => s.push_str(&format!("\\{x:03o}")),
            x => s.push(x as char),
        }
    }
    s
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `encode(b, 'base64')`: with a newline after every 76 characters, as
/// PostgreSQL's (MIME's line length).
fn encode_base64(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 4 / 3 + 4);
    let mut line = 0;
    for chunk in b.chunks(3) {
        if line == 76 {
            s.push('\n');
            line = 0;
        }
        let n = chunk.len();
        let v = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for k in 0..4 {
            s.push(if k <= n {
                BASE64[(v >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
        line += 4;
    }
    s
}

/// `decode(s, 'base64')`: whitespace ignored; padding required.
fn decode_base64(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits, mut pad, mut count) = (0u32, 0, 0, 0usize);
    for c in s.chars() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == '=' {
            pad += 1;
            count += 1;
            continue;
        }
        let Some(v) = BASE64.iter().position(|&x| x as char == c) else {
            return Err(Error::invalid_parameter(format!(
                "invalid symbol \"{c}\" found while decoding base64 sequence"
            )));
        };
        if pad > 0 {
            return Err(Error::invalid_parameter("invalid base64 end sequence"));
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        count += 1;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if count % 4 != 0 || pad > 2 {
        return Err(Error::invalid_parameter("invalid base64 end sequence"));
    }
    Ok(out)
}

fn format_of(name: &str) -> Result<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "hex" => Ok("hex"),
        "base64" => Ok("base64"),
        "escape" => Ok("escape"),
        _ => Err(Error::invalid_parameter(format!(
            "unrecognized encoding: \"{name}\""
        ))),
    }
}

// ------------------------------------------------------------- digests

// ----------------------------------------------------------- functions

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Encode,
    Decode,
    Sha256,
    Md5,
    GetByte,
    SetByte,
    OctetLength,
}

struct ByteFn(F, &'static str);

/// A function's `bytea` argument.
pub fn bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Bytea(b) => Ok(b),
        v => Err(Error::internal(format!("expected bytea, got {v:?}"))),
    }
}

fn text(v: &Value) -> Result<&str> {
    match v {
        Value::Text(s) => Ok(s),
        v => Err(Error::internal(format!("expected text, got {v:?}"))),
    }
}

impl ScalarFunction for ByteFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::Sha256 | F::Md5 | F::OctetLength => 1..=1,
            F::Encode | F::Decode | F::GetByte => 2..=2,
            F::SetByte => 3..=3,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::{Bytea, Integer, Text};
        let (a, r) = match self.0 {
            F::Encode => (vec![Arg::Untyped(Bytea), Arg::Untyped(Text)], Text),
            F::Decode => (vec![Arg::Untyped(Text), Arg::Untyped(Text)], Bytea),
            F::Sha256 => (vec![Arg::Untyped(Bytea)], Bytea),
            // md5 and octet_length take text or bytea (an untyped literal:
            // text).
            F::Md5 | F::OctetLength => {
                let t = match args[0].ty {
                    Some(Bytea) => Bytea,
                    _ => Text,
                };
                let r = if self.0 == F::Md5 { Text } else { Integer };
                (vec![Arg::Untyped(t)], r)
            }
            F::GetByte => (vec![Arg::Untyped(Bytea), Arg::Untyped(Integer)], Integer),
            F::SetByte => (
                vec![
                    Arg::Untyped(Bytea),
                    Arg::Untyped(Integer),
                    Arg::Untyped(Integer),
                ],
                Bytea,
            ),
        };
        for (arg, want) in args.iter().zip(&a) {
            let Arg::Untyped(want) = want else { continue };
            if let Some(t) = arg.ty.filter(|_| !arg.text_literal)
                && t != *want
                && !(*want == Text && t == DataType::Char)
                && !(want.is_integer() && t.is_integer())
            {
                let names: Vec<String> = args
                    .iter()
                    .map(|a| a.ty.map_or("unknown".into(), |t| t.to_string()))
                    .collect();
                return Err(Error::undefined_function(format!(
                    "function {}({}) does not exist",
                    self.1,
                    names.join(", ")
                )));
            }
        }
        Ok(Signature::new(a, Returns::Type(Some(r))))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let blob = |b: Vec<u8>| Value::Bytea(Arc::from(b));
        Ok(match self.0 {
            F::Encode => {
                let b = bytes(&args[0])?;
                Value::Text(match format_of(text(&args[1])?)? {
                    "hex" => hex(b),
                    "base64" => encode_base64(b),
                    _ => encode_escape(b),
                })
            }
            F::Decode => {
                let s = text(&args[0])?;
                blob(match format_of(text(&args[1])?)? {
                    "hex" => parse_hex(s)?,
                    "base64" => decode_base64(s)?,
                    _ => parse_escape(s)?,
                })
            }
            F::Sha256 => blob(nucleus_crypto::sha256(bytes(&args[0])?).to_vec()),
            F::Md5 => Value::Text(hex(&nucleus_crypto::md5(match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Char(s) => crate::types::unpadded(s).as_bytes(),
                v => bytes(v)?,
            }))),
            F::OctetLength => Value::Int(match &args[0] {
                Value::Text(s) | Value::Char(s) => s.len(),
                v => bytes(v)?.len(),
            } as i64),
            F::GetByte | F::SetByte => {
                let b = bytes(&args[0])?;
                let Value::Int(n) = args[1] else {
                    return Err(Error::internal("get_byte index"));
                };
                let at = usize::try_from(n).ok().filter(|&n| n < b.len());
                let Some(at) = at else {
                    return Err(Error::array_subscript(format!(
                        "index {n} out of valid range, 0..{}",
                        b.len() as i64 - 1
                    )));
                };
                if self.0 == F::GetByte {
                    Value::Int(i64::from(b[at]))
                } else {
                    let Value::Int(v) = args[2] else {
                        return Err(Error::internal("set_byte value"));
                    };
                    let mut out = b.to_vec();
                    out[at] = v as u8;
                    blob(out)
                }
            }
        })
    }
}

/// Registers the `bytea` functions.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    for (f, name) in [
        (F::Encode, "encode"),
        (F::Decode, "decode"),
        (F::Sha256, "sha256"),
        (F::Md5, "md5"),
        (F::GetByte, "get_byte"),
        (F::SetByte, "set_byte"),
        (F::OctetLength, "octet_length"),
    ] {
        r.add_function(&[name], Arc::new(ByteFn(f, name)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_forms() {
        assert_eq!(parse("\\xDEADbeef").unwrap(), [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(parse("\\x de ad").unwrap(), [0xde, 0xad]);
        assert_eq!(parse("a\\\\b\\001").unwrap(), b"a\\b\x01");
        assert_eq!(parse("\\xabc").unwrap_err().code, "22023");
        assert_eq!(parse("a\\9").unwrap_err().code, "22P02");
        assert_eq!(format_hex(&[0, 0xff]), "\\x00ff");
        assert_eq!(encode_escape(b"\0\xff\\A"), "\\000\\377\\\\A");
    }

    #[test]
    fn base64() {
        for (raw, enc) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"hello", "aGVsbG8="),
        ] {
            assert_eq!(encode_base64(raw), enc);
            assert_eq!(decode_base64(enc).unwrap(), raw);
        }
        assert_eq!(decode_base64("aGVs bG8=").unwrap(), b"hello");
        assert!(decode_base64("a").is_err());
        assert!(decode_base64("Zg=a").is_err());
        let long = encode_base64(&[7u8; 100]);
        assert_eq!(long.find('\n'), Some(76));
    }
}
