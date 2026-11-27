//! String functions PostgreSQL has and the built-in list did not (issue
//! #16, met by a random-query fuzzer): `lpad`, `rpad`, `ltrim`, `rtrim`,
//! `btrim` (with the characters to trim), `split_part`, `concat_ws`,
//! `translate`, `reverse`, `initcap`, `overlay` — and `cbrt`.  Each
//! counts characters, not bytes, as PostgreSQL does in UTF-8.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum F {
    Lpad,
    Rpad,
    Ltrim,
    Rtrim,
    Btrim,
    SplitPart,
    ConcatWs,
    Translate,
    Reverse,
    Initcap,
    Overlay,
    Cbrt,
    QuoteIdent,
    QuoteLiteral,
}

struct StrFn(F, &'static str);

fn text(v: &Value) -> Result<&str> {
    match v {
        Value::Text(s) => Ok(s),
        Value::Char(s) => Ok(crate::types::unpadded(s)),
        v => Err(Error::internal(format!("expected text, got {v:?}"))),
    }
}

fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        v => Err(Error::internal(format!("expected an integer, got {v:?}"))),
    }
}

/// The most characters a padded string may have (PostgreSQL's limit on a
/// text value, 1 GB, counted in characters): past it, `54000`.
const MOST: i64 = 1 << 28;

/// `s` padded on the left (or the right) with `fill` repeated to `len`
/// characters, or cut to them.
fn pad(s: &str, len: i64, fill: &str, left: bool) -> Result<String> {
    if len > MOST {
        return Err(Error::program_limit("requested length too large"));
    }
    let len = len.max(0) as usize;
    let n = s.chars().count();
    if n >= len || fill.is_empty() {
        return Ok(s.chars().take(len).collect());
    }
    let padding: String = fill.chars().cycle().take(len - n).collect();
    Ok(if left {
        padding + s
    } else {
        s.to_string() + &padding
    })
}

/// `s` without the characters of `set` at its start, its end or both.
fn trim<'a>(s: &'a str, set: &str, start: bool, end: bool) -> &'a str {
    let is = |c: char| set.contains(c);
    let s = if start { s.trim_start_matches(is) } else { s };
    if end { s.trim_end_matches(is) } else { s }
}

/// `split_part`: the `n`-th field of `s` split at `delim` (from the end
/// when `n` is negative); `''` past the last.
fn split_part(s: &str, delim: &str, n: i64) -> Result<String> {
    if n == 0 {
        return Err(Error::invalid_parameter("field position must not be zero"));
    }
    if delim.is_empty() {
        return Ok(if n == 1 || n == -1 {
            s.to_string()
        } else {
            String::new()
        });
    }
    let fields: Vec<&str> = s.split(delim).collect();
    let at = if n > 0 {
        usize::try_from(n - 1).ok()
    } else {
        usize::try_from(-n)
            .ok()
            .and_then(|k| fields.len().checked_sub(k))
    };
    Ok(at
        .and_then(|i| fields.get(i))
        .map_or(String::new(), |f| f.to_string()))
}

/// `translate`: each character of `s` found in `from` replaced by the one
/// at its place in `to` (the first place, if it is there twice), or left
/// out when `to` is shorter.
fn translate(s: &str, from: &str, to: &str) -> String {
    let (from, to): (Vec<char>, Vec<char>) = (from.chars().collect(), to.chars().collect());
    s.chars()
        .filter_map(|c| match from.iter().position(|&f| f == c) {
            Some(i) => to.get(i).copied(),
            None => Some(c),
        })
        .collect()
}

/// `initcap`: each word's first letter in upper case, the rest in lower;
/// words are runs of letters and digits.
fn initcap(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_word = false;
    for c in s.chars() {
        if in_word {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        in_word = c.is_alphanumeric();
    }
    out
}

/// `overlay(s PLACING r FROM start [FOR count])`: `s`'s characters from
/// `start` (counting from 1), `count` of them (`r`'s length by default),
/// replaced by `r` — PostgreSQL's `substring(s, 1, start - 1) || r ||
/// substring(s, start + count)`, a negative `count` included.
fn overlay(s: &str, r: &str, start: i64, count: Option<i64>) -> Result<String> {
    let count = count.unwrap_or(r.chars().count() as i64);
    if start < 1 {
        return Err(Error::substring("negative substring length not allowed"));
    }
    let head = s.chars().take((start - 1) as usize);
    let rest = start.saturating_add(count).saturating_sub(1).max(0);
    let tail = s.chars().skip(usize::try_from(rest).unwrap_or(usize::MAX));
    Ok(head.chain(r.chars()).chain(tail).collect())
}

impl ScalarFunction for StrFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::Reverse | F::Initcap | F::Cbrt | F::QuoteIdent | F::QuoteLiteral => 1..=1,
            F::Ltrim | F::Rtrim | F::Btrim => 1..=2,
            F::Lpad | F::Rpad => 2..=3,
            F::SplitPart | F::Translate => 3..=3,
            F::Overlay => 3..=4,
            F::ConcatWs => 1..=usize::MAX,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        use DataType::{Float, Integer, Text};
        let n = args.len();
        let (want, r): (Vec<DataType>, DataType) = match self.0 {
            F::Lpad | F::Rpad => ([Text, Integer, Text][..n].to_vec(), Text),
            F::Ltrim | F::Rtrim | F::Btrim => (vec![Text; n], Text),
            F::SplitPart => (vec![Text, Text, Integer], Text),
            F::Translate => (vec![Text; 3], Text),
            F::Reverse | F::Initcap | F::QuoteIdent | F::QuoteLiteral => (vec![Text], Text),
            F::Overlay => ([Text, Text, Integer, Integer][..n].to_vec(), Text),
            F::Cbrt => (vec![Float], Float),
            // The separator text; the values any type, as text.
            F::ConcatWs => {
                let mut a = vec![Arg::Untyped(Text)];
                a.extend(std::iter::repeat_n(Arg::Keep, n - 1));
                return Ok(Signature::new(a, Returns::Type(Some(Text))));
            }
        };
        for (arg, want) in args.iter().zip(&want) {
            if let Some(t) = arg.ty.filter(|_| !arg.text_literal)
                && t != *want
                && !(*want == Text && t == DataType::Char)
                && !(want.is_integer() && t.is_integer())
                && !(*want == Float && t.is_number())
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
        let args = want
            .into_iter()
            .map(|t| {
                if t == Float {
                    Arg::Convert(Float)
                } else {
                    Arg::Untyped(t)
                }
            })
            .collect();
        Ok(Signature::new(args, Returns::Type(Some(r))))
    }

    fn strict(&self) -> bool {
        // concat_ws leaves its NULL values out (a NULL separator: NULL).
        self.0 != F::ConcatWs
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let t = |i: usize| text(&args[i]);
        Ok(Value::Text(match self.0 {
            F::Lpad | F::Rpad => {
                let fill = if args.len() == 3 { t(2)? } else { " " };
                pad(t(0)?, int(&args[1])?, fill, self.0 == F::Lpad)?
            }
            F::Ltrim | F::Rtrim | F::Btrim => {
                let set = if args.len() == 2 { t(1)? } else { " " };
                trim(t(0)?, set, self.0 != F::Rtrim, self.0 != F::Ltrim).to_string()
            }
            F::SplitPart => split_part(t(0)?, t(1)?, int(&args[2])?)?,
            F::Translate => translate(t(0)?, t(1)?, t(2)?),
            F::Reverse => t(0)?.chars().rev().collect(),
            // Quoted where it must be (as `definitions` writes names).
            F::QuoteIdent => crate::definitions::ident(t(0)?),
            // PostgreSQL's: `E'…'` when it holds a backslash.
            F::QuoteLiteral => {
                let s = t(0)?;
                let body = s.replace('\'', "''");
                if s.contains('\\') {
                    format!("E'{}'", body.replace('\\', "\\\\"))
                } else {
                    format!("'{body}'")
                }
            }
            F::Initcap => initcap(t(0)?),
            F::Overlay => {
                let count = args.get(3).map(int).transpose()?;
                overlay(t(0)?, t(1)?, int(&args[2])?, count)?
            }
            F::ConcatWs => {
                if args[0].is_null() {
                    return Ok(Value::Null);
                }
                let sep = t(0)?;
                let parts: Vec<String> = args[1..]
                    .iter()
                    .filter(|v| !v.is_null())
                    .map(Value::render)
                    .collect();
                parts.join(sep)
            }
            F::Cbrt => {
                let Value::Float(x) = args[0] else {
                    return Err(Error::internal("cbrt of a non-float"));
                };
                return Ok(Value::Float(x.cbrt()));
            }
        }))
    }
}

/// Registers the string functions.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    for (f, name) in [
        (F::Lpad, "lpad"),
        (F::Rpad, "rpad"),
        (F::Ltrim, "ltrim"),
        (F::Rtrim, "rtrim"),
        (F::Btrim, "btrim"),
        (F::SplitPart, "split_part"),
        (F::ConcatWs, "concat_ws"),
        (F::Translate, "translate"),
        (F::Reverse, "reverse"),
        (F::QuoteIdent, "quote_ident"),
        (F::QuoteLiteral, "quote_literal"),
        (F::Initcap, "initcap"),
        (F::Overlay, "overlay"),
        (F::Cbrt, "cbrt"),
    ] {
        r.add_function(&[name], Arc::new(StrFn(f, name)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_postgresql_has_them() {
        assert_eq!(pad("hi", 5, "xy", true).unwrap(), "xyxhi");
        assert_eq!(pad("hi", 5, "xy", false).unwrap(), "hixyx");
        assert_eq!(pad("hello", 2, "x", true).unwrap(), "he");
        assert_eq!(pad("hi", 5, "", true).unwrap(), "hi");
        assert_eq!(pad("hi", -1, "x", true).unwrap(), "");
        assert_eq!(pad("é", 3, "ő", true).unwrap(), "őőé");
        assert_eq!(trim("xxhixx", "x", true, false), "hixx");
        assert_eq!(trim("xyhiyx", "xy", true, true), "hi");
        assert_eq!(trim("\thi ", " ", true, true), "\thi");
        assert_eq!(split_part("a,b,,c", ",", 2).unwrap(), "b");
        assert_eq!(split_part("a,b,,c", ",", 3).unwrap(), "");
        assert_eq!(split_part("a,b,,c", ",", -1).unwrap(), "c");
        assert_eq!(split_part("a,b", ",", 5).unwrap(), "");
        assert_eq!(split_part("a,b", ",", -5).unwrap(), "");
        assert_eq!(split_part("ab", "", 1).unwrap(), "ab");
        assert_eq!(split_part("ab", "", 2).unwrap(), "");
        assert_eq!(split_part("ab", ",", 0).unwrap_err().code, "22023");
        assert_eq!(translate("12345", "143", "ax"), "a2x5");
        assert_eq!(translate("aab", "aa", "xy"), "xxb");
        assert_eq!(initcap("hi THOMAS o'neil x2y"), "Hi Thomas O'Neil X2y");
        assert_eq!(overlay("Txxxxas", "hom", 2, Some(4)).unwrap(), "Thomas");
        assert_eq!(overlay("abc", "XY", 2, None).unwrap(), "aXY");
        assert_eq!(overlay("abc", "XY", 5, None).unwrap(), "abcXY");
        assert_eq!(overlay("abcdef", "X", 3, Some(-1)).unwrap(), "abXbcdef");
        assert_eq!(overlay("abc", "X", 0, None).unwrap_err().code, "22011");
    }
}
