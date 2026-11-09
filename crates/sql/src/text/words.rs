//! Word boundaries as Unicode defines them (UAX #29, "Unicode Text
//! Segmentation", the default rules), from the property tables in
//! [`super::word_breaks`].  The rules' numbers are the standard's.  Words
//! are the segments with a letter or a digit in them — the rest is spaces
//! and punctuation; an ideograph is a word of its own (no rule joins
//! them), as Lucene's `StandardTokenizer` makes it.

/// A code point's word-break class (UAX #29 table 3); the order is
/// `scripts/gen-word-breaks.py`'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Break {
    Other,
    Cr,
    Lf,
    Newline,
    Extend,
    Zwj,
    RegionalIndicator,
    Format,
    Katakana,
    HebrewLetter,
    ALetter,
    SingleQuote,
    DoubleQuote,
    MidNumLet,
    MidLetter,
    MidNum,
    Numeric,
    ExtendNumLet,
    WSegSpace,
}

const CLASSES: [Break; 19] = [
    Break::Other,
    Break::Cr,
    Break::Lf,
    Break::Newline,
    Break::Extend,
    Break::Zwj,
    Break::RegionalIndicator,
    Break::Format,
    Break::Katakana,
    Break::HebrewLetter,
    Break::ALetter,
    Break::SingleQuote,
    Break::DoubleQuote,
    Break::MidNumLet,
    Break::MidLetter,
    Break::MidNum,
    Break::Numeric,
    Break::ExtendNumLet,
    Break::WSegSpace,
];

/// `c`'s class and whether it is Extended_Pictographic.
pub(crate) fn class(c: char) -> (Break, bool) {
    let c = c as u32;
    if c < 0x80 {
        // ASCII without a search: letters, digits, the rest.
        let b = match c as u8 {
            b'a'..=b'z' | b'A'..=b'Z' => Break::ALetter,
            b'0'..=b'9' => Break::Numeric,
            b'\r' => Break::Cr,
            b'\n' => Break::Lf,
            0x0B | 0x0C => Break::Newline,
            b' ' => Break::WSegSpace,
            b'\'' => Break::SingleQuote,
            b'"' => Break::DoubleQuote,
            b'.' => Break::MidNumLet,
            b':' => Break::MidLetter,
            b',' | b';' => Break::MidNum,
            b'_' => Break::ExtendNumLet,
            _ => Break::Other,
        };
        return (b, false);
    }
    let table = &super::word_breaks::RANGES;
    let i = table.partition_point(|&(_, hi, _)| hi < c);
    match table.get(i) {
        Some(&(lo, _, v)) if lo <= c => (CLASSES[(v & 0x1F) as usize], v & 0x20 != 0),
        _ => (Break::Other, false),
    }
}

fn ignored(b: Break) -> bool {
    matches!(b, Break::Extend | Break::Format | Break::Zwj)
}

fn ah_letter(b: Break) -> bool {
    matches!(b, Break::ALetter | Break::HebrewLetter)
}

fn mid_letter(b: Break) -> bool {
    matches!(b, Break::MidLetter | Break::MidNumLet | Break::SingleQuote)
}

fn mid_num(b: Break) -> bool {
    matches!(b, Break::MidNum | Break::MidNumLet | Break::SingleQuote)
}

fn newline(b: Break) -> bool {
    matches!(b, Break::Newline | Break::Cr | Break::Lf)
}

/// The boundaries of `text`'s segments: byte offsets, the first 0 and the
/// last `text.len()` (none for an empty text).
// The rules look before and after each boundary: indexes, not an iterator.
#[allow(clippy::needless_range_loop)]
pub(crate) fn boundaries(text: &str) -> Vec<usize> {
    let chars: Vec<(usize, Break, bool)> = text
        .char_indices()
        .map(|(i, c)| {
            let (b, p) = class(c);
            (i, b, p)
        })
        .collect();
    let mut out = Vec::with_capacity(chars.len() / 4 + 2);
    if chars.is_empty() {
        return out;
    }
    out.push(0);
    let cls = |i: usize| chars[i].1;
    // The nearest class before `i` (exclusive) that WB4 does not fold
    // into what precedes it, with its index.
    let back = |mut i: usize| -> Option<usize> {
        while i > 0 {
            i -= 1;
            if !ignored(cls(i)) {
                return Some(i);
            }
        }
        None
    };
    let ahead = |mut i: usize| -> Option<Break> {
        while i < chars.len() {
            if !ignored(cls(i)) {
                return Some(cls(i));
            }
            i += 1;
        }
        None
    };
    for i in 1..chars.len() {
        let (a, b) = (cls(i - 1), cls(i));
        let keep = 'rules: {
            // WB3, WB3a, WB3b.
            if a == Break::Cr && b == Break::Lf {
                break 'rules true;
            }
            if newline(a) || newline(b) {
                break 'rules false;
            }
            // WB3c, WB3d.
            if a == Break::Zwj && chars[i].2 {
                break 'rules true;
            }
            if a == Break::WSegSpace && b == Break::WSegSpace {
                break 'rules true;
            }
            // WB4: Extend, Format and ZWJ belong to what they follow.
            if ignored(b) {
                break 'rules true;
            }
            // What precedes, Extend and the like skipped — unless they
            // follow the start or a line break, where they stand alone.
            let p = match back(i) {
                Some(p) if !newline(cls(p)) => p,
                _ => i - 1,
            };
            let a = cls(p);
            let z = back(p).map(cls);
            let c = ahead(i + 1);
            let is = |x: Option<Break>, f: fn(Break) -> bool| x.is_some_and(f);
            // WB5–WB7.
            if ah_letter(a) && ah_letter(b) {
                break 'rules true;
            }
            if ah_letter(a) && mid_letter(b) && is(c, ah_letter) {
                break 'rules true;
            }
            if is(z, ah_letter) && mid_letter(a) && ah_letter(b) {
                break 'rules true;
            }
            // WB7a–WB7c.
            if a == Break::HebrewLetter && b == Break::SingleQuote {
                break 'rules true;
            }
            if a == Break::HebrewLetter && b == Break::DoubleQuote && c == Some(Break::HebrewLetter)
            {
                break 'rules true;
            }
            if z == Some(Break::HebrewLetter) && a == Break::DoubleQuote && b == Break::HebrewLetter
            {
                break 'rules true;
            }
            // WB8–WB12.
            let num = |x: Break| x == Break::Numeric;
            if (num(a) || ah_letter(a)) && num(b) || num(a) && ah_letter(b) {
                break 'rules true;
            }
            if is(z, num) && mid_num(a) && num(b) {
                break 'rules true;
            }
            if num(a) && mid_num(b) && is(c, num) {
                break 'rules true;
            }
            // WB13–WB13b.
            if a == Break::Katakana && b == Break::Katakana {
                break 'rules true;
            }
            let joins = |x: Break| ah_letter(x) || matches!(x, Break::Numeric | Break::Katakana);
            if (joins(a) || a == Break::ExtendNumLet) && b == Break::ExtendNumLet {
                break 'rules true;
            }
            if a == Break::ExtendNumLet && joins(b) {
                break 'rules true;
            }
            // WB15, WB16: regional indicators in pairs.
            if a == Break::RegionalIndicator && b == Break::RegionalIndicator {
                let mut n = 1;
                let mut q = p;
                while let Some(r) = back(q) {
                    if cls(r) != Break::RegionalIndicator {
                        break;
                    }
                    n += 1;
                    q = r;
                }
                break 'rules n % 2 == 1;
            }
            // WB999.
            false
        };
        if !keep {
            out.push(chars[i].0);
        }
    }
    out.push(text.len());
    out
}

/// `text`'s words: byte ranges of the segments with a letter or a digit.
pub(crate) fn words(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = boundaries(text);
    (1..b.len())
        .map(move |i| (b[i - 1], b[i]))
        .filter(move |&(s, e)| text[s..e].chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_of_a_sentence() {
        let t = "The quick (\"brown\") fox can't jump 32.3 feet, right? 日本語 e.g. a_b";
        let got: Vec<&str> = words(t).map(|(s, e)| &t[s..e]).collect();
        assert_eq!(
            got,
            [
                "The", "quick", "brown", "fox", "can't", "jump", "32.3", "feet", "right", "日",
                "本", "語", "e.g", "a_b"
            ]
        );
    }

    /// Every case of Unicode's own test (`WordBreakTest.txt`): `÷` a
    /// boundary, `×` none, between the code points given in hex.
    #[test]
    fn unicode_word_break_test() {
        let cases = include_str!("../../tests/data/word_break_test.txt");
        let mut failed = Vec::new();
        for line in cases.lines().filter(|l| !l.starts_with('#')) {
            let mut text = String::new();
            let mut want = Vec::new();
            for part in line.split_whitespace() {
                match part {
                    "÷" => want.push(text.len()),
                    "×" => {}
                    hex => {
                        text.push(char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap())
                    }
                }
            }
            if boundaries(&text) != want {
                failed.push(line);
            }
        }
        assert!(
            failed.is_empty(),
            "{} cases differ: {:?}",
            failed.len(),
            &failed[..failed.len().min(5)]
        );
    }
}
