//! Sentence boundaries as Unicode defines them (UAX #29, "Unicode Text
//! Segmentation", the default rules), from the property tables in
//! [`super::sentence_breaks`] (PLAN.md §M14.22 E6).  The rules' numbers
//! are the standard's.  A sentence keeps the spaces and the paragraph
//! separator after it, as the standard's segments do.

/// A code point's sentence-break class (UAX #29 table 4); the order is
/// `scripts/gen-sentence-breaks.py`'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Break {
    Other,
    Cr,
    Lf,
    Extend,
    Sep,
    Format,
    Sp,
    Lower,
    Upper,
    OLetter,
    Numeric,
    ATerm,
    SContinue,
    STerm,
    Close,
}

const CLASSES: [Break; 15] = [
    Break::Other,
    Break::Cr,
    Break::Lf,
    Break::Extend,
    Break::Sep,
    Break::Format,
    Break::Sp,
    Break::Lower,
    Break::Upper,
    Break::OLetter,
    Break::Numeric,
    Break::ATerm,
    Break::SContinue,
    Break::STerm,
    Break::Close,
];

/// `c`'s class.
pub(crate) fn class(c: char) -> Break {
    let c = c as u32;
    if c < 0x80 {
        // ASCII without a search.
        return match c as u8 {
            b'a'..=b'z' => Break::Lower,
            b'A'..=b'Z' => Break::Upper,
            b'0'..=b'9' => Break::Numeric,
            b'\r' => Break::Cr,
            b'\n' => Break::Lf,
            b' ' | b'\t' | 0x0B | 0x0C => Break::Sp,
            b'.' => Break::ATerm,
            b'!' | b'?' => Break::STerm,
            b',' | b'-' | b':' | b';' => Break::SContinue,
            b'"' | b'\'' | b'(' | b')' | b'[' | b']' | b'{' | b'}' => Break::Close,
            _ => Break::Other,
        };
    }
    let table = &super::sentence_breaks::RANGES;
    let i = table.partition_point(|&(_, hi, _)| hi < c);
    match table.get(i) {
        Some(&(lo, _, v)) if lo <= c => CLASSES[v as usize],
        _ => Break::Other,
    }
}

fn ignored(b: Break) -> bool {
    matches!(b, Break::Extend | Break::Format)
}

fn para_sep(b: Break) -> bool {
    matches!(b, Break::Sep | Break::Cr | Break::Lf)
}

fn sa_term(b: Break) -> bool {
    matches!(b, Break::STerm | Break::ATerm)
}

/// The boundaries of `text`'s sentences: byte offsets, the first 0 and
/// the last `text.len()` (none for an empty text).
// The rules look before and after each boundary: indexes, not an iterator.
#[allow(clippy::needless_range_loop)]
pub(crate) fn boundaries(text: &str) -> Vec<usize> {
    let chars: Vec<(usize, Break)> = text.char_indices().map(|(i, c)| (i, class(c))).collect();
    let mut out = Vec::new();
    if chars.is_empty() {
        return out;
    }
    out.push(0);
    let cls = |i: usize| chars[i].1;
    // The nearest index before `i` (exclusive) that SB5 does not fold
    // into what precedes it.
    let back = |mut i: usize| -> Option<usize> {
        while i > 0 {
            i -= 1;
            if !ignored(cls(i)) {
                return Some(i);
            }
        }
        None
    };
    for i in 1..chars.len() {
        let (a, b) = (cls(i - 1), cls(i));
        let keep = 'rules: {
            // SB3, SB4.
            if a == Break::Cr && b == Break::Lf {
                break 'rules true;
            }
            if para_sep(a) {
                break 'rules false;
            }
            // SB5: Extend and Format belong to what they follow.
            if ignored(b) {
                break 'rules true;
            }
            // What precedes, Extend and Format skipped — unless they
            // follow the start or a paragraph separator, where they
            // stand alone.
            let p = match back(i) {
                Some(p) if !para_sep(cls(p)) => p,
                _ => i - 1,
            };
            let a = cls(p);
            // SB6, SB7.
            if a == Break::ATerm && b == Break::Numeric {
                break 'rules true;
            }
            if a == Break::ATerm
                && b == Break::Upper
                && back(p).is_some_and(|q| matches!(cls(q), Break::Upper | Break::Lower))
            {
                break 'rules true;
            }
            // SATerm Close* Sp* before the boundary, read backwards.
            let mut q = Some(p);
            let mut spaces = false;
            while let Some(j) = q.filter(|&j| cls(j) == Break::Sp) {
                spaces = true;
                q = back(j);
            }
            while let Some(j) = q.filter(|&j| cls(j) == Break::Close) {
                q = back(j);
            }
            let Some(term) = q.map(cls).filter(|&t| sa_term(t)) else {
                break 'rules true; // SB998
            };
            // SB8: an ATerm before a lower-case letter that only
            // neutral characters separate from it.
            if term == Break::ATerm {
                let mut j = i;
                while j < chars.len() {
                    let c = cls(j);
                    if !ignored(c)
                        && matches!(
                            c,
                            Break::OLetter
                                | Break::Upper
                                | Break::Lower
                                | Break::Sep
                                | Break::Cr
                                | Break::Lf
                                | Break::STerm
                                | Break::ATerm
                        )
                    {
                        break;
                    }
                    j += 1;
                }
                if j < chars.len() && cls(j) == Break::Lower {
                    break 'rules true;
                }
            }
            // SB8a–SB10.
            if matches!(b, Break::SContinue) || sa_term(b) {
                break 'rules true;
            }
            if !spaces && matches!(b, Break::Close) {
                break 'rules true;
            }
            if b == Break::Sp || para_sep(b) {
                break 'rules true;
            }
            // SB11.
            false
        };
        if !keep {
            out.push(chars[i].0);
        }
    }
    out.push(text.len());
    out
}

/// `text`'s sentences: byte ranges, each with the spaces after it.
#[cfg(test)]
pub(crate) fn sentences(text: &str) -> Vec<(usize, usize)> {
    let b = boundaries(text);
    (1..b.len()).map(|i| (b[i - 1], b[i])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_of_a_text() {
        let t = "Mr. Smith went to Washington. He said \"hi!\" and left.  Then 3.14 is pi? Yes.\n\nNew one";
        let got: Vec<&str> = sentences(t).iter().map(|&(s, e)| &t[s..e]).collect();
        assert_eq!(
            got,
            [
                "Mr. ",
                "Smith went to Washington. ",
                // UAX #29 breaks after `!` and a quote, even before a
                // lower-case word (SB8 holds for `.` alone).
                "He said \"hi!\" ",
                "and left.  ",
                "Then 3.14 is pi? ",
                "Yes.\n",
                "\n",
                "New one"
            ]
        );
    }

    /// Every case of Unicode's own test (`SentenceBreakTest.txt`): `÷` a
    /// boundary, `×` none, between the code points given in hex.
    #[test]
    fn unicode_sentence_break_test() {
        let cases = include_str!("../../tests/data/sentence_break_test.txt");
        let mut failed = Vec::new();
        let mut n = 0;
        for line in cases.lines().filter(|l| !l.starts_with('#')) {
            n += 1;
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
        assert_eq!(n, 512);
        assert!(
            failed.is_empty(),
            "{} cases differ: {:?}",
            failed.len(),
            &failed[..failed.len().min(5)]
        );
    }
}
