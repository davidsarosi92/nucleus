//! JSON text to the binary form ([`super::form`]): strict RFC 8259 — no
//! comments, trailing commas, `NaN` or single quotes — with the depth
//! bounded ([`super::MAX_DEPTH`]), surrogate pairs joined, lone ones and
//! `\u0000` refused (`text` holds no NUL, so a value's strings must not).
//!
//! One pass, recursive descent: each container's elements are written
//! into a buffer kept per depth (reused, so a document costs a handful of
//! allocations), then the container around them — an object's keys sorted
//! and their duplicates dropped, the last kept.  A string without escapes
//! is a slice of the input.

use std::borrow::Cow;
use std::ops::Range;

use super::form;
use crate::error::{Error, Result};

/// Parses JSON text into its binary form.
pub fn parse(text: &str) -> Result<Vec<u8>> {
    let mut p = Parser {
        b: text.as_bytes(),
        text,
        at: 0,
        levels: Vec::new(),
    };
    let mut out = Vec::with_capacity(text.len());
    p.ws();
    p.value(&mut out, 0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err(p.bad("text after the value"));
    }
    Ok(out)
}

/// What one depth reuses.
#[derive(Default)]
struct Level<'a> {
    data: Vec<u8>,
    ends: Vec<usize>,
    pairs: Vec<(Cow<'a, str>, Range<usize>)>,
}

struct Parser<'a> {
    b: &'a [u8],
    text: &'a str,
    at: usize,
    levels: Vec<Level<'a>>,
}

impl<'a> Parser<'a> {
    fn bad(&self, what: &str) -> Error {
        if self.at >= self.b.len() {
            return Error::invalid_text(
                "invalid input syntax for type json: the input ended unexpectedly",
            );
        }
        let token: String = self.text[self.at..].chars().take(12).collect();
        Error::invalid_text(format!(
            "invalid input syntax for type json: {what} at character {} (\"{token}\")",
            self.text[..self.at].chars().count() + 1
        ))
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.at) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &str) -> Result<()> {
        if self.b[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(())
        } else {
            Err(self.bad("an invalid token"))
        }
    }

    fn level(&mut self, depth: usize) -> Level<'a> {
        if self.levels.len() <= depth {
            self.levels.resize_with(depth + 1, Level::default);
        }
        let mut l = std::mem::take(&mut self.levels[depth]);
        l.data.clear();
        l.ends.clear();
        l.pairs.clear();
        l
    }

    fn value(&mut self, out: &mut Vec<u8>, depth: usize) -> Result<()> {
        match self.b.get(self.at) {
            Some(b'{') => self.object(out, depth),
            Some(b'[') => self.array(out, depth),
            Some(b'"') => {
                let s = self.string()?;
                form::put_str(out, &s);
                Ok(())
            }
            Some(b't') => {
                self.literal("true")?;
                form::put_bool(out, true);
                Ok(())
            }
            Some(b'f') => {
                self.literal("false")?;
                form::put_bool(out, false);
                Ok(())
            }
            Some(b'n') => {
                self.literal("null")?;
                form::put_null(out);
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => self.number(out),
            _ => Err(self.bad("expected a value")),
        }
    }

    fn array(&mut self, out: &mut Vec<u8>, depth: usize) -> Result<()> {
        if depth >= super::MAX_DEPTH {
            return Err(super::too_deep());
        }
        self.at += 1;
        let mut l = self.level(depth);
        self.ws();
        if self.b.get(self.at) == Some(&b']') {
            self.at += 1;
        } else {
            loop {
                self.ws();
                self.value(&mut l.data, depth + 1)?;
                l.ends.push(l.data.len());
                self.ws();
                match self.b.get(self.at) {
                    Some(b',') => self.at += 1,
                    Some(b']') => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(self.bad("expected \",\" or \"]\"")),
                }
            }
        }
        form::put_array(out, &l.ends, &l.data)?;
        self.levels[depth] = l;
        Ok(())
    }

    fn object(&mut self, out: &mut Vec<u8>, depth: usize) -> Result<()> {
        if depth >= super::MAX_DEPTH {
            return Err(super::too_deep());
        }
        self.at += 1;
        let mut l = self.level(depth);
        self.ws();
        if self.b.get(self.at) == Some(&b'}') {
            self.at += 1;
        } else {
            loop {
                self.ws();
                if self.b.get(self.at) != Some(&b'"') {
                    return Err(self.bad("expected a string as the key"));
                }
                let key = self.string()?;
                self.ws();
                if self.b.get(self.at) != Some(&b':') {
                    return Err(self.bad("expected \":\""));
                }
                self.at += 1;
                self.ws();
                let start = l.data.len();
                self.value(&mut l.data, depth + 1)?;
                l.pairs.push((key, start..l.data.len()));
                self.ws();
                match self.b.get(self.at) {
                    Some(b',') => self.at += 1,
                    Some(b'}') => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(self.bad("expected \",\" or \"}\"")),
                }
            }
        }
        form::put_object(out, &mut l.pairs, &l.data)?;
        self.levels[depth] = l;
        Ok(())
    }

    /// A string at `"`: a slice of the input when it has no escapes.
    fn string(&mut self) -> Result<Cow<'a, str>> {
        self.at += 1;
        let start = self.at;
        // The run up to a quote, a backslash or a control character,
        // eight bytes at a time.
        let b = self.b;
        let mut i = start;
        while i + 8 <= b.len() {
            let w = u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
            if has_special(w) {
                break;
            }
            i += 8;
        }
        while i < b.len() && !matches!(b[i], b'"' | b'\\' | 0..=0x1f) {
            i += 1;
        }
        match b.get(i) {
            Some(b'"') => {
                self.at = i + 1;
                Ok(Cow::Borrowed(&self.text[start..i]))
            }
            Some(b'\\') => {
                let mut s = String::from(&self.text[start..i]);
                self.at = i;
                self.escaped(&mut s)?;
                Ok(Cow::Owned(s))
            }
            _ => {
                self.at = i;
                Err(self.bad("an unterminated string or a control character in it"))
            }
        }
    }

    /// The rest of a string with escapes, from a backslash.
    fn escaped(&mut self, s: &mut String) -> Result<()> {
        loop {
            let run = self.at;
            while self.at < self.b.len() && !matches!(self.b[self.at], b'"' | b'\\' | 0..=0x1f) {
                self.at += 1;
            }
            s.push_str(&self.text[run..self.at]);
            match self.b.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(());
                }
                Some(b'\\') => {
                    self.at += 1;
                    let c = match self.b.get(self.at) {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.at += 1;
                            let c = self.unicode()?;
                            s.push(c);
                            continue;
                        }
                        _ => return Err(self.bad("an invalid escape")),
                    };
                    self.at += 1;
                    s.push(c);
                }
                _ => return Err(self.bad("an unterminated string or a control character in it")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self
            .b
            .get(self.at..self.at + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| {
                u32::from_str_radix(h, 16)
                    .ok()
                    .filter(|_| h.bytes().all(|c| c.is_ascii_hexdigit()))
            })
            .ok_or_else(|| self.bad("\\u must be followed by four hex digits"))?;
        self.at += 4;
        Ok(h)
    }

    /// The character of `\uXXXX` (after the `u`), a surrogate pair joined.
    fn unicode(&mut self) -> Result<char> {
        let hi = self.hex4()?;
        let code = match hi {
            0 => {
                return Err(Error::untranslatable(
                    "unsupported Unicode escape sequence: \\u0000 cannot be converted to text",
                ));
            }
            0xd800..=0xdbff => {
                if !self.b[self.at..].starts_with(b"\\u") {
                    return Err(self.bad("a Unicode high surrogate not followed by a low one"));
                }
                self.at += 2;
                let lo = self.hex4()?;
                if !(0xdc00..=0xdfff).contains(&lo) {
                    return Err(self.bad("a Unicode high surrogate not followed by a low one"));
                }
                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
            }
            0xdc00..=0xdfff => {
                return Err(self.bad("a Unicode low surrogate without a high one"));
            }
            c => c,
        };
        char::from_u32(code).ok_or_else(|| self.bad("an invalid Unicode escape"))
    }

    fn number(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let start = self.at;
        let b = self.b;
        let negative = b[self.at] == b'-';
        if negative {
            self.at += 1;
        }
        let int = self.at;
        match b.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = b.get(self.at) {
                    self.at += 1;
                }
            }
            _ => return Err(self.bad("an invalid number")),
        }
        let int_end = self.at;
        let (mut frac, mut frac_end) = (self.at, self.at);
        if b.get(self.at) == Some(&b'.') {
            self.at += 1;
            frac = self.at;
            while let Some(b'0'..=b'9') = b.get(self.at) {
                self.at += 1;
            }
            frac_end = self.at;
            if frac == frac_end {
                self.at = start;
                return Err(self.bad("an invalid number"));
            }
        }
        let mut exp: i64 = 0;
        if let Some(b'e' | b'E') = b.get(self.at) {
            self.at += 1;
            let neg_exp = match b.get(self.at) {
                Some(b'-') => {
                    self.at += 1;
                    true
                }
                Some(b'+') => {
                    self.at += 1;
                    false
                }
                _ => false,
            };
            let e = self.at;
            while let Some(b'0'..=b'9') = b.get(self.at) {
                // Saturated: far past any bound, refused later.
                exp = (exp * 10 + i64::from(b[self.at] - b'0')).min(1 << 40);
                self.at += 1;
            }
            if e == self.at {
                self.at = start;
                return Err(self.bad("an invalid number"));
            }
            if neg_exp {
                exp = -exp;
            }
        }
        // The common case: an integer of up to 18 digits.
        if frac == frac_end && exp == 0 && int_end - int <= 18 {
            let mut v: i64 = 0;
            for d in &b[int..int_end] {
                v = v * 10 + i64::from(d - b'0');
            }
            form::put_int(out, if negative { -v } else { v });
            return Ok(());
        }
        // Packed from the text's digits as they are.
        form::put_ascii_number(out, negative, &b[int..int_end], &b[frac..frac_end], exp)
    }
}

/// Whether any of the eight bytes in `w` is `"`, `\` or below 0x20.
pub(crate) fn has_special(w: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let zero = |x: u64| x.wrapping_sub(ONES) & !x & HIGH;
    let quote = zero(w ^ (ONES * u64::from(b'"')));
    let slash = zero(w ^ (ONES * u64::from(b'\\')));
    // Bytes below 0x20: subtracting 0x20 borrows into the high bit (and
    // the byte's own high bit was clear).
    let control = w.wrapping_sub(ONES * 0x20) & !w & HIGH;
    quote | slash | control != 0
}
