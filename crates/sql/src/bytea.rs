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

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in padded(data, true).chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (o, x) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// MD5 (RFC 1321) — for `md5()`, not for security.
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for block in padded(data, false).chunks(64) {
        let m: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
            .collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 16];
    for (o, x) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&x.to_le_bytes());
    }
    out
}

/// `data`, a `1` bit, zeros, and its bit length (big- or little-endian),
/// to a multiple of 64 bytes.
fn padded(data: &[u8], big_endian: bool) -> Vec<u8> {
    let mut m = data.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    let bits = (data.len() as u64).wrapping_mul(8);
    m.extend_from_slice(&if big_endian {
        bits.to_be_bytes()
    } else {
        bits.to_le_bytes()
    });
    m
}

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
            F::Sha256 => blob(sha256(bytes(&args[0])?).to_vec()),
            F::Md5 => Value::Text(hex(&md5(match &args[0] {
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

    #[test]
    fn digests() {
        // FIPS 180-4 and RFC 1321 test vectors.
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }
}
