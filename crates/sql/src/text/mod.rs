//! Text analysis for full-text search (PLAN.md §M14.21 T1): analyzers
//! turn a text into the terms an index keeps and a query looks for.
//! They are registered by name (principle 12) — the built-ins below, and
//! a module's through [`crate::ext::Registry::add_analyzer`] — and kept
//! for the process, as index methods are: an index names its analyzer
//! when it is defined and finds it again by that name.

pub(crate) mod highlight;
pub mod index;
mod porter;
pub(crate) mod query;
pub(crate) mod score;
mod snowball;
mod stop_words;
mod word_breaks;
pub(crate) mod words;

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock, RwLock};

use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::types::{DataType, Value};
use crate::vector::{Sel, Vector};

/// A term of a text: what it is, its position among the text's words
/// (a dropped stop word leaves a gap, so a phrase does not match across
/// it), and the bytes of the text it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The term.
    pub term: String,
    /// Its word's position in the text, from 0.
    pub position: u32,
    /// The byte where its word starts in the text.
    pub start: usize,
    /// The byte after its word.
    pub end: usize,
}

/// Text to terms.  The same text gives the same terms, always: an index
/// keeps what its analyzer gave when the row was written.
pub trait Analyzer: Send + Sync {
    /// The name indexes and functions refer to it by.
    fn name(&self) -> &str;

    /// `text`'s terms, in order, to `out`.
    fn analyze(&self, text: &str, out: &mut dyn FnMut(Token));

    /// A query's prefix or fuzzy word as the terms are compared with it:
    /// folded as a term is, not stemmed (a prefix of a word is no word).
    /// The default lower-cases.
    fn normalize(&self, word: &str) -> String {
        word.to_lowercase()
    }
}

/// `text`'s terms by `a`, collected.
pub fn tokens(a: &dyn Analyzer, text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    a.analyze(text, &mut |t| out.push(t));
    out
}

fn registry() -> &'static RwLock<HashMap<String, Arc<dyn Analyzer>>> {
    static ANALYZERS: OnceLock<RwLock<HashMap<String, Arc<dyn Analyzer>>>> = OnceLock::new();
    ANALYZERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Makes `a` available by its name (a module's replacing one registered
/// under it before — the same module in another registry).
pub fn add(a: Arc<dyn Analyzer>) {
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(a.name().to_string(), a);
}

/// The analyzer called `name`.
pub fn named(name: &str) -> Result<Arc<dyn Analyzer>> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
        .ok_or_else(|| Error::undefined_object(format!("text analyzer \"{name}\" does not exist")))
}

/// The registered analyzers' names, sorted.
pub fn names() -> Vec<String> {
    let mut out: Vec<String> = registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    out.sort();
    out
}

/// The analyzer a text search uses when none is named.
pub const DEFAULT: &str = "simple";

/// How a built-in analyzer splits a text.
#[derive(Clone, Copy)]
enum Split {
    /// Into words (UAX #29).
    Words,
    /// Into every run of `n` characters of the whole text, lower-cased —
    /// substrings found by their pieces (SQLite FTS5's `trigram`).
    Grams(usize),
}

/// A built-in analyzer: a split and the filters each word passes through,
/// in Lucene's order — possessive, lower case, stop words, stem.
struct Builtin {
    name: &'static str,
    split: Split,
    /// English possessives dropped: `'s`, `’s` (Lucene's
    /// `EnglishPossessiveFilter`).
    possessive: bool,
    /// Words dropped, sorted by bytes; compared after lower-casing.
    stop: &'static [&'static str],
    stem: Stem,
}

/// How a built-in analyzer stems.
#[derive(Clone, Copy)]
enum Stem {
    None,
    /// Porter's, with Lucene's departures ([`porter`]).
    Porter,
    /// A Snowball algorithm.
    Snowball(fn(&mut snowball::SnowballEnv) -> bool),
}

impl Analyzer for Builtin {
    fn name(&self) -> &str {
        self.name
    }

    fn analyze(&self, text: &str, out: &mut dyn FnMut(Token)) {
        match self.split {
            Split::Grams(n) => {
                let chars: Vec<(usize, char)> = text.char_indices().collect();
                for (p, w) in chars.windows(n).enumerate() {
                    let (start, last) = (w[0].0, w[n - 1]);
                    out(Token {
                        term: w.iter().flat_map(|(_, c)| c.to_lowercase()).collect(),
                        position: p as u32,
                        start,
                        end: last.0 + last.1.len_utf8(),
                    });
                }
            }
            Split::Words => {
                for (position, (start, end)) in words::words(text).enumerate() {
                    let mut w = &text[start..end];
                    if self.possessive {
                        w = without_possessive(w);
                    }
                    let lower = w.to_lowercase();
                    if self.stop.binary_search(&lower.as_str()).is_ok() {
                        continue;
                    }
                    let term = match self.stem {
                        Stem::None => lower,
                        Stem::Porter => porter::stem(&lower),
                        Stem::Snowball(algorithm) => snowball::stem(algorithm, &lower),
                    };
                    out(Token {
                        term,
                        position: position as u32,
                        start,
                        end,
                    });
                }
            }
        }
    }
}

fn without_possessive(w: &str) -> &str {
    for suffix in [
        "'s",
        "'S",
        "\u{2019}s",
        "\u{2019}S",
        "\u{FF07}s",
        "\u{FF07}S",
    ] {
        if let Some(head) = w.strip_suffix(suffix)
            && !head.is_empty()
        {
            return head;
        }
    }
    w
}

/// Lucene's English stop words (`EnglishAnalyzer.ENGLISH_STOP_WORDS_SET`),
/// sorted.
const LUCENE_ENGLISH: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Snowball's stop words for `language` (none for a language without a
/// list).
fn stop_words(language: &str) -> &'static [&'static str] {
    stop_words::LISTS
        .iter()
        .find(|(l, _)| *l == language)
        .map_or(&[], |(_, w)| *w)
}

/// The built-in analyzers:
///
/// - `simple`: words, lower case — nothing a language decides.
/// - `porter`: Lucene's `EnglishAnalyzer` (and Anserini's BM25
///   baselines): words, possessives dropped, lower case, Lucene's 33
///   English stop words, the Porter stemmer of 1980.
/// - `trigram`: every three characters of the text, lower case — words
///   by their parts, any language, substrings and misspellings near.
/// - one for each of Snowball's languages, by its name (`english`,
///   `hungarian`, `german`, `french`, …): words, lower case, the
///   language's stop words, its Snowball stemmer.
pub fn register_builtins() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(add_builtins);
}

fn add_builtins() {
    let builtins = [
        Builtin {
            name: "simple",
            split: Split::Words,
            possessive: false,
            stop: &[],
            stem: Stem::None,
        },
        Builtin {
            name: "porter",
            split: Split::Words,
            possessive: true,
            stop: LUCENE_ENGLISH,
            stem: Stem::Porter,
        },
        Builtin {
            name: "trigram",
            split: Split::Grams(3),
            possessive: false,
            stop: &[],
            stem: Stem::None,
        },
    ];
    let languages = snowball::LANGUAGES.map(|(name, algorithm)| Builtin {
        name,
        split: Split::Words,
        possessive: false,
        stop: stop_words(name),
        stem: Stem::Snowball(algorithm),
    });
    for b in builtins.into_iter().chain(languages) {
        add(Arc::new(b));
    }
}

/// `tokenize(text [, analyzer])`: the terms an analyzer makes of a text,
/// in order — what an index on it would keep.
struct Tokenize;

impl ScalarFunction for Tokenize {
    fn name(&self) -> &str {
        "tokenize"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=2
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); args.len()],
            Returns::Type(DataType::Text.array_of()),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let Value::Text(text) = &args[0] else {
            return Err(Error::internal("tokenize of a text"));
        };
        let analyzer = match args.get(1) {
            Some(Value::Text(name)) => named(name)?,
            Some(_) => return Err(Error::internal("tokenize by a name")),
            None => named(DEFAULT)?,
        };
        Ok(Value::Array(
            tokens(analyzer.as_ref(), text)
                .into_iter()
                .map(|t| Value::Text(t.term))
                .collect(),
        ))
    }
}

/// `text @@ query`: whether a text matches a query ([`query`]), both
/// analyzed by one analyzer — a third argument the binder gives (T4: the
/// column's text index's), else [`DEFAULT`].  The same answer on a row
/// as from an index: an index is a way to it, never another one.
pub(crate) struct Matches;

impl Matches {
    fn analyzer(args: &[&Value]) -> Result<Arc<dyn Analyzer>> {
        match args.get(2) {
            Some(Value::Text(name)) => named(name),
            Some(_) => Err(Error::internal("@@ by an analyzer's name")),
            None => named(DEFAULT),
        }
    }
}

impl ScalarFunction for Matches {
    fn name(&self) -> &str {
        "@@"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        2..=3
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); args.len()],
            Returns::Type(Some(DataType::Bool)),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (Value::Text(text), Value::Text(q)) = (&args[0], &args[1]) else {
            return Err(Error::internal("@@ of texts"));
        };
        let a = Self::analyzer(&args.iter().collect::<Vec<_>>())?;
        let q = query::cached(q, &a)?;
        let mut hits = q.hits();
        Ok(Value::Bool(q.matches(a.as_ref(), text, &mut hits)))
    }

    /// A text vector against one query: parsed once, its scratch reused.
    fn call_batch(
        &self,
        _: &CallContext<'_>,
        args: &[&Vector],
        len: usize,
        sel: &Sel,
    ) -> Option<Result<Vector>> {
        let (o, b, nulls, char) = crate::vector::text_view(args.first()?)?;
        let consts: Vec<&Value> = args[1..]
            .iter()
            .map(|v| match v {
                Vector::Const(c) => Some(c),
                _ => None,
            })
            .collect::<Option<_>>()?;
        if matches!(consts[0], Value::Null) {
            return Some(Ok(Vector::Const(Value::Null)));
        }
        let Value::Text(q) = consts[0] else {
            return None;
        };
        let run = || -> Result<Vector> {
            let mut all = vec![&Value::Null, &Value::Null];
            all.extend(&consts[1..]);
            let a = Self::analyzer(&all)?;
            let q = query::cached(q, &a)?;
            let mut hits = q.hits();
            let mut out = vec![false; len];
            for i in sel.iter() {
                if !nulls.as_ref().is_some_and(|n| n[i]) {
                    let s = crate::vector::text_at(o, b, char, i);
                    let s = std::str::from_utf8(s)
                        .map_err(|_| Error::internal("a text vector of UTF-8"))?;
                    out[i] = q.matches(a.as_ref(), s, &mut hits);
                }
            }
            Ok(Vector::Bool(out, nulls.clone()))
        };
        Some(run())
    }
}

/// Registers the built-in analyzers and the text functions.
pub fn register(r: &mut Registry) -> Result<()> {
    register_builtins();
    r.add_function(&["tokenize"], Arc::new(Tokenize))?;
    r.add_operator("@@", DataType::Text, DataType::Text, Arc::new(Matches))?;
    r.add_function(&["score"], Arc::new(score::Score))?;
    r.add_function(&["highlight"], Arc::new(highlight::OfCondition::HIGHLIGHT))?;
    r.add_function(&["snippet"], Arc::new(highlight::OfCondition::SNIPPET))?;
    index::register(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(analyzer: &str, text: &str) -> Vec<String> {
        register_builtins();
        tokens(named(analyzer).unwrap().as_ref(), text)
            .into_iter()
            .map(|t| t.term)
            .collect()
    }

    #[test]
    fn the_builtins() {
        assert_eq!(
            terms("simple", "The Quick brown fox's jumps"),
            ["the", "quick", "brown", "fox's", "jumps"]
        );
        assert_eq!(
            terms(
                "porter",
                "The Quick brown fox's jumps, and generalizations!"
            ),
            ["quick", "brown", "fox", "jump", "gener"]
        );
        assert_eq!(terms("trigram", "AbcD"), ["abc", "bcd"]);
        // Multi-byte letters before a final s, possessive or not.
        assert_eq!(
            terms("porter", "café’s naïveté ñs"),
            ["café", "naïveté", "ñs"]
        );
        assert!(terms("trigram", "ab").is_empty());
    }

    #[test]
    fn stop_words_leave_gaps() {
        register_builtins();
        let t = tokens(named("porter").unwrap().as_ref(), "war and peace");
        assert_eq!(
            t.iter()
                .map(|t| (t.term.as_str(), t.position))
                .collect::<Vec<_>>(),
            [("war", 0), ("peac", 2)]
        );
        assert_eq!((t[1].start, t[1].end), (8, 13));
    }

    #[test]
    fn lists_are_sorted() {
        for (_, list) in stop_words::LISTS
            .iter()
            .chain([&("lucene", LUCENE_ENGLISH)])
        {
            assert!(list.windows(2).all(|w| w[0] < w[1]));
        }
    }
}
