//! The binary form back to text: PostgreSQL's `jsonb` layout (`{"a": 1,
//! "b": [1, 2]}`), and indented (`json_pretty`, as `jsonb_pretty`).
//!
//! Written as bytes — strings copied in runs between the bytes JSON must
//! escape, found eight at a time; integers formatted by hand — and made a
//! `String` once (what it writes is UTF-8: the form's strings are, and
//! everything else is ASCII).

use super::form::{self, Dec, Node};
use crate::error::Result;

/// Appends `n` as JSON text.
pub fn write(n: Node<'_>, out: &mut String) -> Result<()> {
    let mut b = std::mem::take(out).into_bytes();
    let r = write_bytes(n, &mut b);
    *out = String::from_utf8(b).map_err(|_| form::malformed())?;
    r
}

/// Appends `n` as JSON text, in bytes.
pub fn write_bytes(n: Node<'_>, out: &mut Vec<u8>) -> Result<()> {
    match n {
        Node::Null => out.extend_from_slice(b"null"),
        Node::Bool(true) => out.extend_from_slice(b"true"),
        Node::Bool(false) => out.extend_from_slice(b"false"),
        Node::Int(i) => write_int(i, out),
        Node::Dec(d) => write_dec_bytes(d, out),
        Node::Str(s) => write_str_bytes(s, out),
        Node::Array(a) => {
            out.push(b'[');
            for i in 0..a.len() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                write_bytes(a.get(i)?, out)?;
            }
            out.push(b']');
        }
        Node::Object(o) => {
            out.push(b'{');
            for i in 0..o.len() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                write_str_bytes(o.key_bytes(i)?, out);
                out.extend_from_slice(b": ");
                write_bytes(o.value(i)?, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Appends `n` as indented JSON text, four spaces a level.
pub fn pretty(n: Node<'_>, out: &mut String, level: usize) -> Result<()> {
    let mut b = std::mem::take(out).into_bytes();
    let r = pretty_bytes(n, &mut b, level);
    *out = String::from_utf8(b).map_err(|_| form::malformed())?;
    r
}

fn pretty_bytes(n: Node<'_>, out: &mut Vec<u8>, level: usize) -> Result<()> {
    let indent = |out: &mut Vec<u8>, l: usize| {
        out.push(b'\n');
        out.resize(out.len() + 4 * l, b' ');
    };
    match n {
        Node::Array(a) if !a.is_empty() => {
            out.push(b'[');
            for i in 0..a.len() {
                if i > 0 {
                    out.push(b',');
                }
                indent(out, level + 1);
                pretty_bytes(a.get(i)?, out, level + 1)?;
            }
            indent(out, level);
            out.push(b']');
        }
        Node::Object(o) if !o.is_empty() => {
            out.push(b'{');
            for i in 0..o.len() {
                if i > 0 {
                    out.push(b',');
                }
                indent(out, level + 1);
                write_str_bytes(o.key_bytes(i)?, out);
                out.extend_from_slice(b": ");
                pretty_bytes(o.value(i)?, out, level + 1)?;
            }
            indent(out, level);
            out.push(b'}');
        }
        // Empty, as `jsonb_pretty` prints one: its brackets on two lines.
        Node::Array(_) => {
            out.push(b'[');
            indent(out, level);
            out.push(b']');
        }
        Node::Object(_) => {
            out.push(b'{');
            indent(out, level);
            out.push(b'}');
        }
        other => write_bytes(other, out)?,
    }
    Ok(())
}

fn write_int(i: i64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    let mut u = i.unsigned_abs();
    loop {
        at -= 1;
        buf[at] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if i < 0 {
        out.push(b'-');
    }
    out.extend_from_slice(&buf[at..]);
}

/// Appends `s` quoted, escaped as JSON requires (and no more: neither `/`
/// nor anything outside ASCII).
pub fn write_str(s: &str, out: &mut String) {
    let mut b = std::mem::take(out).into_bytes();
    write_str_bytes(s.as_bytes(), &mut b);
    *out = String::from_utf8(b).unwrap_or_default();
}

fn write_str_bytes(b: &[u8], out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.reserve(b.len() + 2);
    out.push(b'"');
    let mut run = 0;
    let mut i = 0;
    while i < b.len() {
        // Eight bytes at a time while none needs escaping.
        if i + 8 <= b.len() {
            let w = u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
            if !super::parse::has_special(w) {
                i += 8;
                continue;
            }
        }
        let c = b[i];
        let esc: &[u8] = match c {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0..=0x1f => b"",
            _ => {
                i += 1;
                continue;
            }
        };
        out.extend_from_slice(&b[run..i]);
        if esc.is_empty() {
            out.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(c >> 4)],
                HEX[usize::from(c & 0xf)],
            ]);
        } else {
            out.extend_from_slice(esc);
        }
        i += 1;
        run = i;
    }
    out.extend_from_slice(&b[run..]);
    out.push(b'"');
}

/// Appends a number in plain notation (`0.0001`, `100000000000000000000`),
/// as PostgreSQL's `numeric` prints.
pub fn write_dec(d: Dec<'_>, out: &mut String) {
    let mut b = std::mem::take(out).into_bytes();
    write_dec_bytes(d, &mut b);
    *out = String::from_utf8(b).unwrap_or_default();
}

fn write_dec_bytes(d: Dec<'_>, out: &mut Vec<u8>) {
    if d.negative {
        out.push(b'-');
    }
    let digits = |out: &mut Vec<u8>, from: usize, to: usize| d.write_digits(from, to, out);
    let len = d.len as i64;
    if d.exp >= 0 {
        digits(out, 0, d.len);
        out.resize(out.len() + d.exp as usize, b'0');
        return;
    }
    let point = len + d.exp;
    if point > 0 {
        digits(out, 0, point as usize);
        out.push(b'.');
        digits(out, point as usize, d.len);
    } else {
        out.extend_from_slice(b"0.");
        out.resize(out.len() + (-point) as usize, b'0');
        digits(out, 0, d.len);
    }
}
