//! BSON (bsonspec.org 1.1), written for this module: documents read from
//! and written to bytes, and mapped to and from nucleus's `json` by
//! Extended JSON (PLAN.md §M15 item 4) — an ObjectId is `{"$oid": hex}`,
//! a date `{"$date": "…Z"}`, a decimal `{"$numberDecimal": "…"}`, binary
//! `{"$binary": {"base64": …, "subType": "00"}}` — so a document's types
//! come back as they went in, and the core keeps plain JSON.
//!
//! JSON has one number type: an `int32`, an `int64` and a `double` are
//! all numbers, and a number comes back as an `int32` when it is an
//! integer that fits, an `int64` when it fits that, else a `double` — so
//! `1.0` comes back `1` (recorded in DOCS.md §8.42).

use std::fmt::Write as _;

/// A BSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum B {
    Double(f64),
    Str(String),
    Doc(Doc),
    Arr(Vec<B>),
    Binary(u8, Vec<u8>),
    Undefined,
    ObjectId([u8; 12]),
    Bool(bool),
    /// Milliseconds since 1970-01-01 UTC.
    Date(i64),
    Null,
    Regex(String, String),
    Int32(i32),
    Timestamp(u32, u32),
    Int64(i64),
    Decimal(u128),
    MinKey,
    MaxKey,
}

/// A document: its fields in their order.
pub type Doc = Vec<(String, B)>;

/// A malformed document.
#[derive(Debug)]
pub struct Bad(pub String);

fn bad<T>(what: &str) -> Result<T, Bad> {
    Err(Bad(what.to_string()))
}

/// Reads one document from the front of `b`; it and the bytes it took.
pub fn read_doc(b: &[u8]) -> Result<(Doc, usize), Bad> {
    if b.len() < 5 {
        return bad("a document shorter than 5 bytes");
    }
    let n = i32::from_le_bytes(b[0..4].try_into().unwrap());
    let n = usize::try_from(n).map_err(|_| Bad("a negative document length".into()))?;
    if n < 5 || n > b.len() || b[n - 1] != 0 {
        return bad("a document's length does not fit");
    }
    let mut r = Reader {
        b: &b[4..n - 1],
        at: 0,
        depth: 0,
    };
    let d = r.fields()?;
    Ok((d, n))
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
    depth: u32,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Bad> {
        if self.b.len() - self.at < n {
            return bad("a value runs past its document");
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn i32(&mut self) -> Result<i32, Bad> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, Bad> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn cstr(&mut self) -> Result<String, Bad> {
        let rest = &self.b[self.at..];
        let Some(end) = rest.iter().position(|&c| c == 0) else {
            return bad("a name without its end");
        };
        let s = std::str::from_utf8(&rest[..end]).map_err(|_| Bad("a name not UTF-8".into()))?;
        self.at += end + 1;
        Ok(s.to_string())
    }
    fn string(&mut self) -> Result<String, Bad> {
        let n = self.i32()?;
        let n = usize::try_from(n).map_err(|_| Bad("a negative string length".into()))?;
        if n == 0 {
            return bad("a string without its end");
        }
        let s = self.take(n)?;
        if s[n - 1] != 0 {
            return bad("a string without its end");
        }
        Ok(std::str::from_utf8(&s[..n - 1])
            .map_err(|_| Bad("a string not UTF-8".into()))?
            .to_string())
    }
    fn fields(&mut self) -> Result<Doc, Bad> {
        let mut out = Vec::new();
        while self.at < self.b.len() {
            let tag = self.take(1)?[0];
            let name = self.cstr()?;
            out.push((name, self.value(tag)?));
        }
        Ok(out)
    }
    fn nested(&mut self) -> Result<Doc, Bad> {
        if self.depth >= 100 {
            return bad("documents nested more than 100 deep");
        }
        let start = self.at;
        let n = self.i32()?;
        let n = usize::try_from(n).map_err(|_| Bad("a negative document length".into()))?;
        if n < 5 || self.b.len() - start < n || self.b[start + n - 1] != 0 {
            return bad("an embedded document's length does not fit");
        }
        let mut inner = Reader {
            b: &self.b[start + 4..start + n - 1],
            at: 0,
            depth: self.depth + 1,
        };
        let d = inner.fields()?;
        self.at = start + n;
        Ok(d)
    }
    fn value(&mut self, tag: u8) -> Result<B, Bad> {
        Ok(match tag {
            0x01 => B::Double(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x02 => B::Str(self.string()?),
            0x03 => B::Doc(self.nested()?),
            0x04 => B::Arr(self.nested()?.into_iter().map(|(_, v)| v).collect()),
            0x05 => {
                let n = self.i32()?;
                let n = usize::try_from(n).map_err(|_| Bad("a negative binary length".into()))?;
                let sub = self.take(1)?[0];
                B::Binary(sub, self.take(n)?.to_vec())
            }
            0x06 => B::Undefined,
            0x07 => B::ObjectId(self.take(12)?.try_into().unwrap()),
            0x08 => B::Bool(self.take(1)?[0] != 0),
            0x09 => B::Date(self.i64()?),
            0x0A => B::Null,
            0x0B => B::Regex(self.cstr()?, self.cstr()?),
            0x10 => B::Int32(self.i32()?),
            0x11 => {
                let i = self.i32()? as u32;
                B::Timestamp(self.i32()? as u32, i)
            }
            0x12 => B::Int64(self.i64()?),
            0x13 => B::Decimal(u128::from_le_bytes(self.take(16)?.try_into().unwrap())),
            0xFF => B::MinKey,
            0x7F => B::MaxKey,
            t => return Err(Bad(format!("BSON type 0x{t:02x} is not supported"))),
        })
    }
}

/// A document's bytes.
pub fn write_doc(d: &[(String, B)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    put_doc(&mut out, d.iter().map(|(k, v)| (k.as_str(), v)));
    out
}

fn put_doc<'a>(out: &mut Vec<u8>, fields: impl Iterator<Item = (&'a str, &'a B)>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (k, v) in fields {
        put_value(out, k, v);
    }
    out.push(0);
    let n = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&n.to_le_bytes());
}

fn put_cstr(out: &mut Vec<u8>, s: &str) {
    // A name holding NUL cannot be written: cut there.
    out.extend_from_slice(s.split('\0').next().unwrap_or("").as_bytes());
    out.push(0);
}

fn put_value(out: &mut Vec<u8>, k: &str, v: &B) {
    let tag = match v {
        B::Double(_) => 0x01,
        B::Str(_) => 0x02,
        B::Doc(_) => 0x03,
        B::Arr(_) => 0x04,
        B::Binary(..) => 0x05,
        B::Undefined => 0x06,
        B::ObjectId(_) => 0x07,
        B::Bool(_) => 0x08,
        B::Date(_) => 0x09,
        B::Null => 0x0A,
        B::Regex(..) => 0x0B,
        B::Int32(_) => 0x10,
        B::Timestamp(..) => 0x11,
        B::Int64(_) => 0x12,
        B::Decimal(_) => 0x13,
        B::MinKey => 0xFF,
        B::MaxKey => 0x7F,
    };
    out.push(tag);
    put_cstr(out, k);
    match v {
        B::Double(f) => out.extend_from_slice(&f.to_le_bytes()),
        B::Str(s) => {
            out.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        B::Doc(d) => put_doc(out, d.iter().map(|(k, v)| (k.as_str(), v))),
        B::Arr(a) => {
            let names: Vec<String> = (0..a.len()).map(|i| i.to_string()).collect();
            put_doc(out, names.iter().map(String::as_str).zip(a.iter()));
        }
        B::Binary(sub, b) => {
            out.extend_from_slice(&(b.len() as i32).to_le_bytes());
            out.push(*sub);
            out.extend_from_slice(b);
        }
        B::ObjectId(o) => out.extend_from_slice(o),
        B::Bool(b) => out.push(u8::from(*b)),
        B::Date(ms) => out.extend_from_slice(&ms.to_le_bytes()),
        B::Regex(p, o) => {
            put_cstr(out, p);
            put_cstr(out, o);
        }
        B::Int32(i) => out.extend_from_slice(&i.to_le_bytes()),
        B::Timestamp(t, i) => {
            out.extend_from_slice(&i.to_le_bytes());
            out.extend_from_slice(&t.to_le_bytes());
        }
        B::Int64(i) => out.extend_from_slice(&i.to_le_bytes()),
        B::Decimal(d) => out.extend_from_slice(&d.to_le_bytes()),
        B::Undefined | B::Null | B::MinKey | B::MaxKey => {}
    }
}

/// A field of a document by name.
pub fn get<'a>(d: &'a [(String, B)], name: &str) -> Option<&'a B> {
    d.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

impl B {
    /// A number as an i64 (a whole double too).
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            B::Int32(i) => Some(i64::from(i)),
            B::Int64(i) => Some(i),
            B::Double(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Some(f as i64),
            B::Decimal(_) => None,
            _ => None,
        }
    }
    /// Whether it is true as MongoDB reads a flag (a non-zero number too).
    pub fn truthy(&self) -> bool {
        match self {
            B::Bool(b) => *b,
            B::Null | B::Undefined => false,
            v => v.as_i64().is_none_or(|i| i != 0),
        }
    }
    /// A string's text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            B::Str(s) => Some(s),
            _ => None,
        }
    }
    /// A document's fields.
    pub fn as_doc(&self) -> Option<&Doc> {
        match self {
            B::Doc(d) => Some(d),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- JSON

/// The value as nucleus's `json` keeps it: JSON, BSON's other types in
/// Extended JSON.  `regex_as_filter`: a regular expression as a filter's
/// `{"$regex": …, "$options": …}`, not a stored value's
/// `{"$regularExpression": …}`.
pub fn to_json(v: &B, regex_as_filter: bool, out: &mut String) {
    match v {
        B::Double(f) if f.is_finite() => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                let _ = write!(out, "{}", *f as i64);
            } else {
                let _ = write!(out, "{f:?}");
            }
        }
        B::Double(f) => {
            let s = if f.is_nan() {
                "NaN"
            } else if *f > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            let _ = write!(out, "{{\"$numberDouble\": \"{s}\"}}");
        }
        B::Str(s) => string(out, s),
        B::Doc(d) => {
            out.push('{');
            for (i, (k, v)) in d.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                string(out, k);
                out.push_str(": ");
                to_json(v, regex_as_filter, out);
            }
            out.push('}');
        }
        B::Arr(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                to_json(v, regex_as_filter, out);
            }
            out.push(']');
        }
        B::Binary(sub, b) => {
            let _ = write!(
                out,
                "{{\"$binary\": {{\"base64\": \"{}\", \"subType\": \"{sub:02x}\"}}}}",
                base64(b)
            );
        }
        B::Undefined | B::Null => out.push_str("null"),
        B::ObjectId(o) => {
            out.push_str("{\"$oid\": \"");
            for b in o {
                let _ = write!(out, "{b:02x}");
            }
            out.push_str("\"}");
        }
        B::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        B::Date(ms) => match iso(*ms) {
            Some(t) => {
                let _ = write!(out, "{{\"$date\": \"{t}\"}}");
            }
            None => {
                let _ = write!(out, "{{\"$date\": {{\"$numberLong\": \"{ms}\"}}}}");
            }
        },
        B::Regex(p, o) if regex_as_filter => {
            out.push_str("{\"$regex\": ");
            string(out, p);
            out.push_str(", \"$options\": ");
            string(out, o);
            out.push('}');
        }
        B::Regex(p, o) => {
            out.push_str("{\"$regularExpression\": {\"pattern\": ");
            string(out, p);
            out.push_str(", \"options\": ");
            string(out, o);
            out.push_str("}}");
        }
        B::Int32(i) => {
            let _ = write!(out, "{i}");
        }
        B::Int64(i) => {
            let _ = write!(out, "{i}");
        }
        B::Timestamp(t, i) => {
            let _ = write!(out, "{{\"$timestamp\": {{\"t\": {t}, \"i\": {i}}}}}");
        }
        B::Decimal(d) => {
            out.push_str("{\"$numberDecimal\": \"");
            out.push_str(&decimal_text(*d));
            out.push_str("\"}");
        }
        B::MinKey => out.push_str("{\"$minKey\": 1}"),
        B::MaxKey => out.push_str("{\"$maxKey\": 1}"),
    }
}

/// A document as JSON text.
pub fn doc_json(d: &[(String, B)], regex_as_filter: bool) -> String {
    let mut s = String::new();
    to_json(&B::Doc(d.to_vec()), regex_as_filter, &mut s);
    s
}

fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// `ms` since the epoch as ISO 8601 with milliseconds, UTC — for the years
/// 0 to 9999 (the form that sorts as it reads); else None.
pub fn iso(ms: i64) -> Option<String> {
    let days = ms.div_euclid(86_400_000);
    let rest = ms.rem_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    if !(0..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    ))
}

/// An ISO 8601 date and time (`YYYY-MM-DD[THH:MM[:SS[.fff]]][Z|±HH:MM]`)
/// as ms since the epoch.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
    {
        return None;
    }
    let (mut h, mut mi, mut sec, mut ms) = (0, 0, 0, 0);
    let mut at = 10;
    if matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        h = num(11, 2)?;
        if b.get(13) != Some(&b':') {
            return None;
        }
        mi = num(14, 2)?;
        at = 16;
        if b.get(16) == Some(&b':') {
            sec = num(17, 2)?;
            at = 19;
            if b.get(19) == Some(&b'.') {
                let digits = s[20..].bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    return None;
                }
                let frac = &s[20..20 + digits.min(3)];
                ms = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
                at = 20 + digits;
            }
        }
    }
    let offset = match &s[at..] {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let v = num(at + 1, 2)? * 60 + num(at + 4, 2)?;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // days_from_civil.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi - offset) * 60_000 + sec * 1000 + ms)
}

// ----------------------------------------------------- Decimal128 (BID)

const BIAS: i32 = 6176;

/// A decimal128's text (IEEE 754-2008, binary integer decimal): the
/// coefficient and exponent as written, `1.50` stays `1.50`.
pub fn decimal_text(d: u128) -> String {
    let neg = d >> 127 == 1;
    let sign = if neg { "-" } else { "" };
    let comb = (d >> 122) & 0x1F;
    if comb == 0x1F {
        return "NaN".into();
    }
    if comb == 0x1E {
        return format!("{sign}Infinity");
    }
    let (exp, coef) = if (d >> 125) & 3 == 3 {
        // The "11" form: a coefficient past 2^113, never canonical — zero.
        (((d >> 111) & 0x3FFF) as i32, 0u128)
    } else {
        (((d >> 113) & 0x3FFF) as i32, d & ((1u128 << 113) - 1))
    };
    let coef = if coef >= 10u128.pow(34) { 0 } else { coef };
    let e = exp - BIAS;
    let digits = coef.to_string();
    let adjusted = e + digits.len() as i32 - 1;
    if e <= 0 && adjusted >= -6 {
        if e == 0 {
            return format!("{sign}{digits}");
        }
        let point = digits.len() as i32 + e;
        if point > 0 {
            let (a, b) = digits.split_at(point as usize);
            format!("{sign}{a}.{b}")
        } else {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!(
            "{sign}{first}{frac}E{}{adjusted}",
            if adjusted >= 0 { "+" } else { "" }
        )
    }
}

/// A decimal's text as a decimal128; None if it does not fit (more than
/// 34 digits, an exponent out of range).
pub fn decimal_of(s: &str) -> Option<u128> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let sign = u128::from(neg) << 127;
    match body.to_ascii_lowercase().as_str() {
        "nan" => return Some(0x1F << 122),
        "inf" | "infinity" => return Some(sign | (0x1E << 122)),
        _ => {}
    }
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = match mant.find('.') {
        Some(i) => (&mant[..i], &mant[i + 1..]),
        None => (mant, ""),
    };
    if int.is_empty() && frac.is_empty()
        || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{int}{frac}");
    let digits = digits.trim_start_matches('0');
    if digits.len() > 34 {
        return None;
    }
    let coef: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    let e = exp - frac.len() as i32 + BIAS;
    if !(0..=12287).contains(&e) {
        return None;
    }
    Some(sign | ((e as u128) << 113) | coef)
}

// ------------------------------------------------------ JSON to BSON

/// JSON text (nucleus's, or a client's) as BSON: numbers by size, and
/// Extended JSON's `$`-objects as their types.
pub fn from_json(text: &str) -> Result<B, Bad> {
    let mut p = Parser {
        s: text.as_bytes(),
        at: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.at != p.s.len() {
        return bad("trailing text after JSON");
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    depth: u32,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.at < self.s.len() && matches!(self.s[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.s.get(self.at) == Some(&c) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn value(&mut self) -> Result<B, Bad> {
        self.ws();
        match self.s.get(self.at) {
            Some(b'{') => {
                if self.depth >= 100 {
                    return bad("JSON nested more than 100 deep");
                }
                self.depth += 1;
                self.at += 1;
                let mut d = Vec::new();
                if !self.eat(b'}') {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        if !self.eat(b':') {
                            return bad("':' expected");
                        }
                        let v = self.value()?;
                        d.push((k, v));
                        if self.eat(b',') {
                            continue;
                        }
                        if self.eat(b'}') {
                            break;
                        }
                        return bad("',' or '}' expected");
                    }
                }
                self.depth -= 1;
                Ok(extended(d))
            }
            Some(b'[') => {
                if self.depth >= 100 {
                    return bad("JSON nested more than 100 deep");
                }
                self.depth += 1;
                self.at += 1;
                let mut a = Vec::new();
                if !self.eat(b']') {
                    loop {
                        a.push(self.value()?);
                        if self.eat(b',') {
                            continue;
                        }
                        if self.eat(b']') {
                            break;
                        }
                        return bad("',' or ']' expected");
                    }
                }
                self.depth -= 1;
                Ok(B::Arr(a))
            }
            Some(b'"') => Ok(B::Str(self.string()?)),
            Some(b't') if self.s[self.at..].starts_with(b"true") => {
                self.at += 4;
                Ok(B::Bool(true))
            }
            Some(b'f') if self.s[self.at..].starts_with(b"false") => {
                self.at += 5;
                Ok(B::Bool(false))
            }
            Some(b'n') if self.s[self.at..].starts_with(b"null") => {
                self.at += 4;
                Ok(B::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let start = self.at;
                while self.at < self.s.len()
                    && matches!(
                        self.s[self.at],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.at += 1;
                }
                let t = std::str::from_utf8(&self.s[start..self.at]).unwrap();
                Ok(number(t).ok_or_else(|| Bad(format!("a bad number {t}")))?)
            }
            _ => bad("a JSON value expected"),
        }
    }
    fn string(&mut self) -> Result<String, Bad> {
        if self.s.get(self.at) != Some(&b'"') {
            return bad("a string expected");
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.at) else {
                return bad("a string without its end");
            };
            self.at += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.s.get(self.at) else {
                        return bad("a string without its end");
                    };
                    self.at += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp)
                                && self.s[self.at..].starts_with(b"\\u")
                            {
                                self.at += 2;
                                let lo = self.hex4()?;
                                cp = 0x10000
                                    + ((cp - 0xD800) << 10)
                                    + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            let mut buf = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| Bad("a string not UTF-8".into()))
    }
    fn hex4(&mut self) -> Result<u32, Bad> {
        let h = self
            .s
            .get(self.at..self.at + 4)
            .ok_or_else(|| Bad("a bad \\u escape".into()))?;
        self.at += 4;
        u32::from_str_radix(std::str::from_utf8(h).unwrap_or("x"), 16)
            .map_err(|_| Bad("a bad \\u escape".into()))
    }
}

fn number(t: &str) -> Option<B> {
    if !t.contains(['.', 'e', 'E']) {
        if let Ok(i) = t.parse::<i32>() {
            return Some(B::Int32(i));
        }
        if let Ok(i) = t.parse::<i64>() {
            return Some(B::Int64(i));
        }
    }
    let f: f64 = t.parse().ok()?;
    // A whole number written with a fraction or exponent: an integer when
    // it fits (one number type in JSON).
    if f.fract() == 0.0 && f.abs() <= f64::from(i32::MAX) {
        return Some(B::Int32(f as i32));
    }
    if f.fract() == 0.0 && f.abs() < 9.007e15 {
        return Some(B::Int64(f as i64));
    }
    Some(B::Double(f))
}

/// An object that is one of Extended JSON's type wrappers, as that type.
fn extended(d: Doc) -> B {
    let one = |k: &str| d.len() == 1 && d[0].0 == k;
    if one("$oid") {
        if let B::Str(h) = &d[0].1
            && h.len() == 24
        {
            let mut o = [0u8; 12];
            if (0..12).all(|i| {
                u8::from_str_radix(&h[2 * i..2 * i + 2], 16)
                    .map(|b| o[i] = b)
                    .is_ok()
            }) {
                return B::ObjectId(o);
            }
        }
    } else if one("$date") {
        match &d[0].1 {
            B::Str(s) => {
                if let Some(ms) = parse_iso(s) {
                    return B::Date(ms);
                }
            }
            B::Doc(inner) if inner.len() == 1 && inner[0].0 == "$numberLong" => {
                if let Some(ms) = inner[0].1.as_str().and_then(|s| s.parse().ok()) {
                    return B::Date(ms);
                }
            }
            B::Int32(i) => return B::Date(i64::from(*i)),
            B::Int64(i) => return B::Date(*i),
            _ => {}
        }
    } else if one("$numberDecimal") {
        if let Some(v) = d[0].1.as_str().and_then(decimal_of) {
            return B::Decimal(v);
        }
    } else if one("$numberLong") {
        if let Some(v) = d[0].1.as_str().and_then(|s| s.parse().ok()) {
            return B::Int64(v);
        }
    } else if one("$numberInt") {
        if let Some(v) = d[0].1.as_str().and_then(|s| s.parse().ok()) {
            return B::Int32(v);
        }
    } else if one("$numberDouble") {
        if let Some(v) = d[0].1.as_str().and_then(|s| s.parse().ok()) {
            return B::Double(v);
        }
    } else if one("$binary") {
        if let B::Doc(inner) = &d[0].1 {
            let b64 = get(inner, "base64").and_then(B::as_str);
            let sub = get(inner, "subType")
                .and_then(B::as_str)
                .and_then(|s| u8::from_str_radix(s, 16).ok());
            if let (Some(b), Some(sub), 2) = (b64, sub, inner.len())
                && let Some(bytes) = unbase64(b)
            {
                return B::Binary(sub, bytes);
            }
        }
    } else if one("$regularExpression") {
        if let B::Doc(inner) = &d[0].1
            && let (Some(p), Some(o), 2) = (
                get(inner, "pattern").and_then(B::as_str),
                get(inner, "options").and_then(B::as_str),
                inner.len(),
            )
        {
            return B::Regex(p.into(), o.into());
        }
    } else if one("$timestamp") {
        if let B::Doc(inner) = &d[0].1
            && let (Some(t), Some(i)) = (
                get(inner, "t").and_then(B::as_i64),
                get(inner, "i").and_then(B::as_i64),
            )
        {
            return B::Timestamp(t as u32, i as u32);
        }
    } else if one("$minKey") {
        return B::MinKey;
    } else if one("$maxKey") {
        return B::MaxKey;
    }
    B::Doc(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_round_trip() {
        let d: Doc = vec![
            (
                "_id".into(),
                B::ObjectId([0x65, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
            ),
            ("n".into(), B::Int32(-7)),
            ("big".into(), B::Int64(1 << 40)),
            ("x".into(), B::Double(1.5)),
            ("s".into(), B::Str("é\"\\\n".into())),
            ("at".into(), B::Date(1_700_000_000_123)),
            ("old".into(), B::Date(-62_200_000_000_000)),
            ("dec".into(), B::Decimal(decimal_of("-12.340").unwrap())),
            ("bin".into(), B::Binary(4, vec![1, 2, 3, 250])),
            ("re".into(), B::Regex("^a".into(), "i".into())),
            ("ts".into(), B::Timestamp(5, 6)),
            (
                "a".into(),
                B::Arr(vec![
                    B::Null,
                    B::Bool(true),
                    B::Doc(vec![("k".into(), B::MinKey)]),
                ]),
            ),
        ];
        let bytes = write_doc(&d);
        let (back, n) = read_doc(&bytes).unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(back, d);
        // Through JSON: every type comes back.
        let json = doc_json(&d, false);
        assert!(
            json.contains(r#""$oid": "650102030405060708090a0b""#),
            "{json}"
        );
        assert!(
            json.contains(r#""$date": "2023-11-14T22:13:20.123Z""#),
            "{json}"
        );
        assert!(json.contains(r#""$numberDecimal": "-12.340""#), "{json}");
        assert_eq!(from_json(&json).unwrap(), B::Doc(d));
        // A regex in a filter is the operator.
        assert_eq!(
            doc_json(&[("n".into(), B::Regex("x".into(), "".into()))], true),
            r#"{"n": {"$regex": "x", "$options": ""}}"#
        );
    }

    #[test]
    fn numbers_and_dates() {
        assert_eq!(from_json("1.0").unwrap(), B::Int32(1));
        assert_eq!(from_json("3000000000").unwrap(), B::Int64(3_000_000_000));
        assert_eq!(from_json("2.5").unwrap(), B::Double(2.5));
        assert_eq!(from_json("1e300").unwrap(), B::Double(1e300));
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso("2024-02-29T12:00:00.5+01:00"),
            Some(1_709_204_400_500)
        );
        assert_eq!(
            iso(1_709_204_400_500).as_deref(),
            Some("2024-02-29T11:00:00.500Z")
        );
        for ms in [
            0,
            1,
            -1,
            951_782_400_000,
            253_402_300_799_999,
            -62_167_219_200_000,
        ] {
            assert_eq!(parse_iso(&iso(ms).unwrap()), Some(ms));
        }
        assert_eq!(iso(253_402_300_800_000), None);
        for t in [
            "0",
            "1.50",
            "-0.001",
            "1.23E+40",
            "1E-10",
            "12345678901234567890123456789012.34",
        ] {
            assert_eq!(decimal_text(decimal_of(t).unwrap()), t);
        }
        assert_eq!(decimal_text(decimal_of("NaN").unwrap()), "NaN");
        assert!(decimal_of("1e7000").is_none());
    }

    #[test]
    fn malformed_bytes_are_refused() {
        assert!(read_doc(&[5, 0, 0, 0]).is_err());
        assert!(read_doc(&[6, 0, 0, 0, 0x10, 0]).is_err());
        assert!(read_doc(&[12, 0, 0, 0, 0x02, b'a', 0, 9, 0, 0, 0, 0]).is_err());
        let mut nested = vec![0u8; 0];
        for _ in 0..200 {
            nested = [vec![0x03, b'a', 0], nested].concat();
        }
        let _ = read_doc(&nested);
    }
}
