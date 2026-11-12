//! BERT's tokenizer: its normalizer, its pre-tokenizer and WordPiece, as
//! Hugging Face's `tokenizers` runs them (bge, MiniLM, GTE, BERT itself).
//! The character classes are Unicode 8's ([`super::bert_tables`]: the
//! version `tokenizers` takes them from), the case mapping Rust's.

use std::collections::HashMap;

use super::bert_tables::{CLASSES, COMBINING, DECOMPOSED, DECOMPOSITIONS};

const CONTROL: u8 = 1;
const PUNCTUATION: u8 = 2;
const NONSPACING: u8 = 4;

fn class(c: char) -> u8 {
    let c = c as u32;
    let i = CLASSES.partition_point(|&(_, hi, _)| hi < c);
    match CLASSES.get(i) {
        Some(&(lo, _, v)) if lo <= c => v,
        _ => 0,
    }
}

/// Whitespace as the normalizer and the pre-tokenizer see it: Rust's
/// (White_Space), which has tab, line feed and carriage return.
fn whitespace(c: char) -> bool {
    c.is_whitespace()
}

fn control(c: char) -> bool {
    !matches!(c, '\t' | '\n' | '\r') && class(c) & CONTROL != 0
}

fn punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_ascii() && class(c) & PUNCTUATION != 0)
}

/// The CJK ideographs BERT's normalizer surrounds with spaces (its
/// original list, Unicode's blocks as they were).
fn chinese(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF
        | 0x3400..=0x4DBF
        | 0x20000..=0x2A6DF
        | 0x2A700..=0x2B73F
        | 0x2B740..=0x2B81F
        | 0x2B920..=0x2CEAF
        | 0xF900..=0xFAFF
        | 0x2F800..=0x2FA1F)
}

fn combining_class(c: char) -> u8 {
    let c = c as u32;
    let i = COMBINING.partition_point(|&(_, hi, _)| hi < c);
    match COMBINING.get(i) {
        Some(&(lo, _, v)) if lo <= c => v,
        _ => 0,
    }
}

/// Appends `c`'s canonical decomposition.
fn decompose(c: char, out: &mut Vec<char>) {
    let u = c as u32;
    if (0xAC00..=0xD7A3).contains(&u) {
        // Hangul syllables, by arithmetic (Unicode 3.12).
        let s = u - 0xAC00;
        out.push(char::from_u32(0x1100 + s / 588).unwrap_or(c));
        out.push(char::from_u32(0x1161 + (s % 588) / 28).unwrap_or(c));
        if !s.is_multiple_of(28) {
            out.push(char::from_u32(0x11A7 + s % 28).unwrap_or(c));
        }
        return;
    }
    if u < 0xC0 {
        out.push(c);
        return;
    }
    match DECOMPOSITIONS.binary_search_by_key(&u, |&(k, _, _)| k) {
        Ok(i) => {
            let (_, at, n) = DECOMPOSITIONS[i];
            let at = at as usize;
            out.extend(
                DECOMPOSED[at..at + n as usize]
                    .iter()
                    .filter_map(|&x| char::from_u32(x)),
            );
        }
        Err(_) => out.push(c),
    }
}

/// Canonical ordering: each run of combining marks sorted, stably, by
/// combining class.
fn reorder(s: &mut [char]) {
    let mut i = 0;
    while i < s.len() {
        if combining_class(s[i]) == 0 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < s.len() && combining_class(s[j]) != 0 {
            j += 1;
        }
        s[i..j].sort_by_key(|&c| combining_class(c));
        i = j;
    }
}

/// BERT's normalizer's settings.
#[derive(Clone, Debug)]
pub(crate) struct Normalizer {
    pub clean_text: bool,
    pub chinese: bool,
    pub strip_accents: bool,
    pub lowercase: bool,
}

impl Normalizer {
    pub(crate) fn apply(&self, s: &str) -> String {
        let mut chars: Vec<char> = Vec::with_capacity(s.len());
        for c in s.chars() {
            if self.clean_text {
                if c == '\0' || c == '\u{FFFD}' || control(c) {
                    continue;
                }
                if whitespace(c) {
                    chars.push(' ');
                    continue;
                }
            }
            if self.chinese && chinese(c) {
                chars.extend([' ', c, ' ']);
            } else {
                chars.push(c);
            }
        }
        if self.strip_accents && !chars.iter().all(char::is_ascii) {
            let mut d = Vec::with_capacity(chars.len());
            for &c in &chars {
                decompose(c, &mut d);
            }
            reorder(&mut d);
            d.retain(|&c| c.is_ascii() || class(c) & NONSPACING == 0);
            chars = d;
        }
        let mut out = String::with_capacity(chars.len());
        for c in chars {
            if self.lowercase {
                out.extend(c.to_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }
}

/// BERT's pre-tokenizer: words split at whitespace, each punctuation
/// character a word of its own.
pub(crate) fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(whitespace).flat_map(|w| {
        let mut rest = w;
        std::iter::from_fn(move || {
            let mut it = rest.char_indices();
            let (_, c) = it.next()?;
            let end = if punctuation(c) {
                c.len_utf8()
            } else {
                it.find(|&(_, c)| punctuation(c))
                    .map_or(rest.len(), |(i, _)| i)
            };
            let (word, tail) = rest.split_at(end);
            rest = tail;
            Some(word)
        })
    })
}

/// WordPiece: a word as the longest pieces of the vocabulary from its
/// start, the ones after the first marked by the continuing prefix
/// (`##`); a word that cannot be covered, or of more than
/// `max_chars`, is the unknown token.
pub(crate) struct WordPiece {
    /// The pieces that start a word.
    first: HashMap<Box<str>, u32>,
    /// The pieces that continue one, without their prefix.
    next: HashMap<Box<str>, u32>,
    unk: u32,
    max_chars: usize,
}

impl WordPiece {
    pub(crate) fn new(
        vocab: impl IntoIterator<Item = (String, u32)>,
        prefix: &str,
        unk: u32,
        max_chars: usize,
    ) -> WordPiece {
        let mut first = HashMap::new();
        let mut next = HashMap::new();
        for (piece, id) in vocab {
            if !prefix.is_empty()
                && let Some(rest) = piece.strip_prefix(prefix)
            {
                next.insert(rest.into(), id);
            }
            first.insert(piece.into_boxed_str(), id);
        }
        WordPiece {
            first,
            next,
            unk,
            max_chars,
        }
    }

    pub(crate) fn word(&self, w: &str, out: &mut Vec<u32>) {
        if w.chars().count() > self.max_chars {
            out.push(self.unk);
            return;
        }
        let mark = out.len();
        let mut start = 0;
        while start < w.len() {
            let map = if start == 0 { &self.first } else { &self.next };
            let mut end = w.len();
            loop {
                if let Some(&id) = map.get(&w[start..end]) {
                    out.push(id);
                    break;
                }
                // One character shorter.
                end = w[..end].char_indices().last().map_or(start, |(i, _)| i);
                if end <= start {
                    out.truncate(mark);
                    out.push(self.unk);
                    return;
                }
            }
            start = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bert() -> Normalizer {
        Normalizer {
            clean_text: true,
            chinese: true,
            strip_accents: true,
            lowercase: true,
        }
    }

    #[test]
    fn normalizes_as_bert_does() {
        let n = bert();
        assert_eq!(n.apply("Árvíztűrő\ttükörfúrógép"), "arvizturo tukorfurogep");
        assert_eq!(n.apply("a\u{0}b\u{FFFD}c\u{200B}d"), "abcd", "Cf removed");
        assert_eq!(n.apply("中文"), " 中  文 ");
        assert_eq!(n.apply("한"), "\u{1112}\u{1161}\u{11AB}");
        assert_eq!(n.apply("\u{378}"), "\u{378}", "unassigned kept");
    }

    #[test]
    fn words_and_punctuation() {
        let w: Vec<&str> = words(" hello, world!!  x-y ").collect();
        assert_eq!(w, ["hello", ",", "world", "!", "!", "x", "-", "y"]);
        // $ and ^ are not Unicode punctuation but ASCII's.
        assert_eq!(words("a$b").collect::<Vec<_>>(), ["a", "$", "b"]);
    }

    #[test]
    fn wordpiece_longest_first() {
        let vocab = ["[UNK]", "un", "##aff", "##able", "aff", "##a"]
            .iter()
            .enumerate()
            .map(|(i, p)| (p.to_string(), i as u32));
        let wp = WordPiece::new(vocab, "##", 0, 10);
        let mut out = Vec::new();
        wp.word("unaffable", &mut out);
        assert_eq!(out, [1, 2, 3]);
        out.clear();
        wp.word("unx", &mut out);
        assert_eq!(out, [0], "a word not covered is unknown");
        out.clear();
        wp.word("unaffableunaff", &mut out);
        assert_eq!(out, [0], "too long");
    }
}
