//! An order-preserving key encoding of JSON values, and with it their
//! order — PostgreSQL's for `jsonb`: null < string < number < boolean
//! (false < true) < array < object; arrays by length, then element by
//! element; objects by their number of keys, then key and value by key,
//! in the keys' order.  Strings by their bytes (`text`'s order here),
//! numbers by value.
//!
//! - null `01`; a string `02`, its bytes (`00` as `00 FF`), `00 01`;
//! - a number `03`, then `01` negative, `02` zero, `03` positive; a
//!   non-zero one the power of ten of its first digit (8 bytes, biased,
//!   big-endian) and its digits as 1–10, then `00` — all of it inverted
//!   when negative, so a larger magnitude sorts lower;
//! - false `04`, true `05`;
//! - an array `06`, its length (4 bytes, big-endian), its elements;
//! - an object `07`, its key count, then each key (as a string's bytes)
//!   and value, in the keys' order.

use std::cmp::Ordering;

use super::form::{self, Node};
use crate::error::{Error, Result};

fn bad() -> Error {
    Error::internal("malformed json key")
}

/// Appends `n`'s key encoding.
pub fn encode(n: Node<'_>, out: &mut Vec<u8>) -> Result<()> {
    match n {
        Node::Null => out.push(0x01),
        Node::Str(s) => {
            out.push(0x02);
            escape(s, out);
        }
        Node::Int(i) => {
            out.push(0x03);
            let mut digits: Vec<u8> = i
                .unsigned_abs()
                .to_string()
                .bytes()
                .map(|d| d - b'0')
                .collect();
            if i == 0 {
                out.push(0x02);
                return Ok(());
            }
            let trailing = digits.iter().rev().take_while(|d| **d == 0).count();
            let lead = digits.len() as i64 - 1;
            digits.truncate(digits.len() - trailing);
            number(i < 0, lead, &digits, out);
        }
        Node::Dec(d) => {
            out.push(0x03);
            let digits = d.digits();
            number(d.negative, d.exp + d.len as i64 - 1, &digits, out);
        }
        Node::Bool(b) => out.push(if b { 0x05 } else { 0x04 }),
        Node::Array(a) => {
            out.push(0x06);
            out.extend_from_slice(&(a.len() as u32).to_be_bytes());
            for i in 0..a.len() {
                encode(a.get(i)?, out)?;
            }
        }
        Node::Object(o) => {
            out.push(0x07);
            out.extend_from_slice(&(o.len() as u32).to_be_bytes());
            for i in 0..o.len() {
                escape(o.key_bytes(i)?, out);
                encode(o.value(i)?, out)?;
            }
        }
    }
    Ok(())
}

fn escape(b: &[u8], out: &mut Vec<u8>) {
    for &x in b {
        out.push(x);
        if x == 0 {
            out.push(0xff);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}

fn unescape(b: &[u8], at: &mut usize) -> Result<String> {
    let mut s = Vec::new();
    loop {
        match (b.get(*at), b.get(*at + 1)) {
            // A json string holds no NUL.
            (Some(0), Some(0xff)) => return Err(bad()),
            (Some(0), Some(1)) => {
                *at += 2;
                return String::from_utf8(s).map_err(|_| bad());
            }
            (Some(c), _) if *c != 0 => {
                s.push(*c);
                *at += 1;
            }
            _ => return Err(bad()),
        }
    }
}

/// A non-zero number: its sign, the power of ten of its first digit and
/// its digits (no trailing zeros).
fn number(negative: bool, lead: i64, digits: &[u8], out: &mut Vec<u8>) {
    out.push(if negative { 0x01 } else { 0x03 });
    let start = out.len();
    out.extend_from_slice(&((lead ^ i64::MIN) as u64).to_be_bytes());
    out.extend(digits.iter().map(|d| d + 1));
    out.push(0x00);
    if negative {
        out[start..].iter_mut().for_each(|b| *b = !*b);
    }
}

/// The key encoding of the value whose binary form is `b`.
pub fn of(b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(b.len() + 8);
    encode(form::root(b)?, &mut out)?;
    Ok(out)
}

/// Compares two values in their binary forms.
pub fn compare(a: &[u8], b: &[u8]) -> Result<Ordering> {
    if a == b {
        return Ok(Ordering::Equal);
    }
    Ok(of(a)?.cmp(&of(b)?))
}

/// The value encoded at the start of `b` (its binary form), and the bytes
/// the encoding took.
pub fn decode(b: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut at = 0;
    let mut out = Vec::new();
    decode_into(b, &mut at, &mut out, 0)?;
    Ok((out, at))
}

fn decode_into(b: &[u8], at: &mut usize, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > super::MAX_DEPTH {
        return Err(bad());
    }
    let tag = *b.get(*at).ok_or_else(bad)?;
    *at += 1;
    match tag {
        0x01 => form::put_null(out),
        0x02 => form::put_str(out, &unescape(b, at)?),
        0x03 => {
            let class = *b.get(*at).ok_or_else(bad)?;
            *at += 1;
            match class {
                0x02 => form::put_int(out, 0),
                0x01 | 0x03 => {
                    let negative = class == 0x01;
                    let flip = |x: u8| if negative { !x } else { x };
                    let e = b.get(*at..*at + 8).ok_or_else(bad)?;
                    let mut eb = [0u8; 8];
                    for (i, x) in e.iter().enumerate() {
                        eb[i] = flip(*x);
                    }
                    let lead = (u64::from_be_bytes(eb) as i64) ^ i64::MIN;
                    *at += 8;
                    let mut digits = Vec::new();
                    loop {
                        let d = flip(*b.get(*at).ok_or_else(bad)?);
                        *at += 1;
                        match d {
                            0 => break,
                            1..=10 => digits.push(d - 1),
                            _ => return Err(bad()),
                        }
                    }
                    if digits.is_empty() || digits.last() == Some(&0) {
                        return Err(bad());
                    }
                    let exp = lead.checked_sub(digits.len() as i64 - 1).ok_or_else(bad)?;
                    form::put_number(out, negative, &digits, exp).map_err(|_| bad())?;
                }
                _ => return Err(bad()),
            }
        }
        0x04 => form::put_bool(out, false),
        0x05 => form::put_bool(out, true),
        0x06 => {
            let n = len(b, at)?;
            let mut data = Vec::new();
            let mut ends = Vec::new();
            for _ in 0..n {
                decode_into(b, at, &mut data, depth + 1)?;
                ends.push(data.len());
            }
            form::put_array(out, &ends, &data)?;
        }
        0x07 => {
            let n = len(b, at)?;
            let mut data = Vec::new();
            let mut pairs = Vec::new();
            for _ in 0..n {
                let k = unescape(b, at)?;
                let start = data.len();
                decode_into(b, at, &mut data, depth + 1)?;
                pairs.push((k, start..data.len()));
            }
            let before = pairs.len();
            form::put_object(out, &mut pairs, &data)?;
            if pairs.len() != before {
                return Err(bad());
            }
        }
        _ => return Err(bad()),
    }
    Ok(())
}

fn len(b: &[u8], at: &mut usize) -> Result<usize> {
    let n = b.get(*at..*at + 4).ok_or_else(bad)?;
    *at += 4;
    let n = u32::from_be_bytes(n.try_into().expect("4 bytes")) as usize;
    // An element takes a byte at least.
    if n > b.len() {
        return Err(bad());
    }
    Ok(n)
}
