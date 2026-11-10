//! Highlights and snippets (PLAN.md §M14.21 T6): `highlight(body @@ q)`
//! — the text with the words the query found marked — and
//! `snippet(body @@ q)` — its best stretch of words, marked.  Functions of
//! the condition, as `score` is, so the text is analyzed as `@@` analyzes
//! it: by the column's text index's analyzer, else the default.
//!
//! What is marked: each word an exact, prefix or fuzzy term of the query
//! finds, and each occurrence of a phrase as one stretch, from its first
//! word to its last (SQLite FTS5's `highlight`); nothing an exclusion
//! names.  Stretches that overlap are one.  The text between is kept as
//! it is — not escaped (PostgreSQL's `ts_headline` neither).
//!
//! The snippet is the window of `words` terms (PostgreSQL's `MaxWords`,
//! 35) holding the most distinct terms of the query, then the most marks
//! (FTS5's snippet: distinct phrases first), the earliest of equals —
//! moved to put its marks in the middle where they leave room — with an
//! ellipsis where the text goes on.  A text the query finds nothing in
//! gives its first `words` terms.

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use super::query::{self, Node, Query};
use super::{Analyzer, Token};
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{Arg, ArgInfo, CallContext, Returns, ScalarFunction, Signature, Volatility};
use crate::types::{DataType, Value};

/// The tags around a mark, by default.
const START: &str = "<b>";
const STOP: &str = "</b>";
/// Where a snippet leaves text out.
const ELLIPSIS: &str = "...";
/// A snippet's terms, by default.
const WORDS: i64 = 35;
/// A snippet's most terms.
const MOST_WORDS: i64 = 1000;

/// A text's terms and what the query marks in them.
struct Marked {
    tokens: Vec<Token>,
    /// Byte ranges marked, sorted, not overlapping.
    spans: Vec<(usize, usize)>,
    /// For each token, the leaves it is a term of (a bit each, the first
    /// 64), where the query marks it.
    leaves: Vec<u64>,
}

fn mark(q: &Query, a: &dyn Analyzer, text: &str) -> Marked {
    let tokens = super::tokens(a, text);
    let mut leaves = vec![0u64; tokens.len()];
    let mut spans = Vec::new();
    let Some(root) = &q.root else {
        return Marked {
            tokens,
            spans,
            leaves,
        };
    };
    // Each leaf's positions, and the tokens at a position.
    let mut hits = vec![Vec::new(); q.leaves.len()];
    let mut at: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, t) in tokens.iter().enumerate() {
        at.entry(t.position).or_default().push(i);
        for (l, leaf) in q.leaves.iter().enumerate() {
            if leaf.matches(&t.term) && hits[l].last() != Some(&t.position) {
                hits[l].push(t.position);
            }
        }
    }
    // The token at `p` that is a term of leaf `l`.
    let token = |l: usize, p: u32| {
        at.get(&p)?
            .iter()
            .copied()
            .find(|&i| q.leaves[l].matches(&tokens[i].term))
    };
    let mut walk = vec![root];
    while let Some(n) = walk.pop() {
        match n {
            Node::Leaf(l) => {
                for &p in &hits[*l] {
                    if let Some(i) = token(*l, p) {
                        leaves[i] |= bit(*l);
                        spans.push((tokens[i].start, tokens[i].end));
                    }
                }
            }
            Node::Phrase(ts, slop) => {
                query::phrase(ts, *slop, &|l| &hits[l], &mut |_, pos| {
                    let found: Vec<usize> = ts
                        .iter()
                        .zip(pos)
                        .filter_map(|(&(l, _), &p)| {
                            let i = token(l, p)?;
                            leaves[i] |= bit(l);
                            Some(i)
                        })
                        .collect();
                    if let (Some(&lo), Some(&hi)) = (found.iter().min(), found.iter().max()) {
                        spans.push((tokens[lo].start, tokens[hi].end));
                    }
                    true
                });
            }
            Node::Bool { must, should, .. } => walk.extend(must.iter().chain(should)),
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s < last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Marked {
        tokens,
        spans: merged,
        leaves,
    }
}

fn bit(l: usize) -> u64 {
    1u64 << l.min(63)
}

/// `text[from..to]` with the marks inside it tagged.
fn tagged(
    text: &str,
    spans: &[(usize, usize)],
    from: usize,
    to: usize,
    tags: (&str, &str),
) -> String {
    let mut out = String::with_capacity(to - from + 16);
    let mut at = from;
    for &(s, e) in spans {
        if s < from || e > to {
            continue;
        }
        out.push_str(&text[at..s]);
        out.push_str(tags.0);
        out.push_str(&text[s..e]);
        out.push_str(tags.1);
        at = e;
    }
    out.push_str(&text[at..to]);
    out
}

/// The text with every mark tagged.
pub(crate) fn highlight(q: &Query, a: &dyn Analyzer, text: &str, tags: (&str, &str)) -> String {
    let m = mark(q, a, text);
    tagged(text, &m.spans, 0, text.len(), tags)
}

/// The best window of `words` terms, tagged, with `ellipsis` where the
/// text goes on.
pub(crate) fn snippet(
    q: &Query,
    a: &dyn Analyzer,
    text: &str,
    tags: (&str, &str),
    ellipsis: &str,
    words: usize,
) -> String {
    let m = mark(q, a, text);
    let n = m.tokens.len();
    if n == 0 || words == 0 {
        return String::new();
    }
    let w = words.min(n);
    // Slide the window, counting each leaf's marked tokens in it.
    let mut count = [0u32; 64];
    let (mut distinct, mut marks) = (0u32, 0u32);
    let mut add = |i: usize, d: i32, distinct: &mut u32, marks: &mut u32| {
        let ls = m.leaves[i];
        if ls == 0 {
            return;
        }
        *marks = marks.wrapping_add_signed(d);
        let mut b = ls;
        while b != 0 {
            let l = b.trailing_zeros() as usize;
            b &= b - 1;
            let c = &mut count[l];
            if d > 0 {
                *c += 1;
                if *c == 1 {
                    *distinct += 1;
                }
            } else {
                *c -= 1;
                if *c == 0 {
                    *distinct -= 1;
                }
            }
        }
    };
    for i in 0..w {
        add(i, 1, &mut distinct, &mut marks);
    }
    let mut best = (distinct, marks, 0usize);
    for start in 1..=n - w {
        add(start - 1, -1, &mut distinct, &mut marks);
        add(start + w - 1, 1, &mut distinct, &mut marks);
        if (distinct, marks) > (best.0, best.1) {
            best = (distinct, marks, start);
        }
    }
    let mut start = best.2;
    // The marks in the middle, where they leave room.
    let marked: Vec<usize> = (start..start + w).filter(|&i| m.leaves[i] != 0).collect();
    if let (Some(&first), Some(&last)) = (marked.first(), marked.last()) {
        let room = w - (last - first + 1);
        start = first.saturating_sub(room / 2).min(n - w);
    }
    let end = start + w;
    let from = if start == 0 { 0 } else { m.tokens[start].start };
    let to = if end == n {
        text.len()
    } else {
        m.tokens[end - 1].end
    };
    let mut out = String::new();
    if start > 0 {
        out.push_str(ellipsis);
    }
    out.push_str(&tagged(text, &m.spans, from, to, tags));
    if end < n {
        out.push_str(ellipsis);
    }
    out
}

/// `highlight(col @@ q [, start, stop])` and `snippet(col @@ q [, start,
/// stop, ellipsis, words])`, as written: bound as [`TextHighlight`] /
/// [`TextSnippet`] of the condition's operands, analyzer and all; of
/// anything but `@@`, refused.
pub(crate) struct OfCondition {
    snippet: bool,
}

impl OfCondition {
    pub(crate) const HIGHLIGHT: OfCondition = OfCondition { snippet: false };
    pub(crate) const SNIPPET: OfCondition = OfCondition { snippet: true };
}

impl ScalarFunction for OfCondition {
    fn name(&self) -> &str {
        if self.snippet { "snippet" } else { "highlight" }
    }

    fn arity(&self) -> RangeInclusive<usize> {
        if self.snippet { 1..=5 } else { 1..=3 }
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Err(Error::feature(format!(
            "{}() takes a condition column @@ query first",
            self.name()
        )))
    }

    fn of_condition(&self) -> Option<&str> {
        Some("@@")
    }

    fn bind_condition(
        &self,
        operands: &[(Expr, Option<DataType>)],
        rest: &[(Expr, Option<DataType>)],
    ) -> Result<(Arc<dyn ScalarFunction>, Vec<(Expr, Option<DataType>)>)> {
        let mut args = operands.to_vec();
        if args.len() == 2 {
            args.push((
                Expr::Literal(Value::Text(super::DEFAULT.into())),
                Some(DataType::Text),
            ));
        }
        args.extend_from_slice(rest);
        let f: Arc<dyn ScalarFunction> = if self.snippet {
            Arc::new(TextSnippet)
        } else {
            Arc::new(TextHighlight)
        };
        Ok((f, args))
    }

    fn call(&self, _: &CallContext<'_>, _: &[Value]) -> Result<Value> {
        Err(Error::internal("highlight() bound as its condition's"))
    }
}

/// The text, query and analyzer of a call's first three arguments.
fn parts(args: &[Value]) -> Result<(&str, Arc<Query>, Arc<dyn Analyzer>)> {
    let (Value::Text(text), Value::Text(q), Value::Text(analyzer)) = (&args[0], &args[1], &args[2])
    else {
        return Err(Error::internal(
            "highlight of a text, a query and an analyzer",
        ));
    };
    let a = super::named(analyzer)?;
    let q = query::cached(q, &a)?;
    Ok((text, q, a))
}

fn text_at<'v>(args: &'v [Value], i: usize, default: &'v str) -> Result<&'v str> {
    match args.get(i) {
        None => Ok(default),
        Some(Value::Text(t)) => Ok(t),
        Some(_) => Err(Error::internal("highlight's tags of texts")),
    }
}

/// `text_highlight(text, query, analyzer [, start, stop])`.
pub(crate) struct TextHighlight;

impl ScalarFunction for TextHighlight {
    fn name(&self) -> &str {
        "text_highlight"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        3..=5
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); args.len()],
            Returns::Type(Some(DataType::Text)),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (text, q, a) = parts(args)?;
        let tags = (text_at(args, 3, START)?, text_at(args, 4, STOP)?);
        Ok(Value::Text(highlight(&q, a.as_ref(), text, tags)))
    }
}

/// `text_snippet(text, query, analyzer [, start, stop, ellipsis, words])`.
pub(crate) struct TextSnippet;

impl ScalarFunction for TextSnippet {
    fn name(&self) -> &str {
        "text_snippet"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        3..=7
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut sig = vec![Arg::Convert(DataType::Text); args.len().min(6)];
        if args.len() == 7 {
            sig.push(Arg::Convert(DataType::BigInt));
        }
        Ok(Signature::new(sig, Returns::Type(Some(DataType::Text))))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (text, q, a) = parts(args)?;
        let tags = (text_at(args, 3, START)?, text_at(args, 4, STOP)?);
        let ellipsis = text_at(args, 5, ELLIPSIS)?;
        let words = match args.get(6) {
            None => WORDS,
            Some(Value::Int(n)) if (1..=MOST_WORDS).contains(n) => *n,
            Some(_) => {
                return Err(Error::invalid_parameter(format!(
                    "snippet(): words must be 1 to {MOST_WORDS}"
                )));
            }
        };
        Ok(Value::Text(snippet(
            &q,
            a.as_ref(),
            text,
            tags,
            ellipsis,
            words as usize,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(analyzer: &str, q: &str, text: &str) -> String {
        super::super::register_builtins();
        let a = super::super::named(analyzer).unwrap();
        let q = Query::parse(q, a.as_ref()).unwrap();
        highlight(&q, a.as_ref(), text, ("[", "]"))
    }

    fn s(q: &str, text: &str, words: usize) -> String {
        super::super::register_builtins();
        let a = super::super::named("simple").unwrap();
        let q = Query::parse(q, a.as_ref()).unwrap();
        snippet(&q, a.as_ref(), text, ("[", "]"), "…", words)
    }

    #[test]
    fn marks() {
        let t = "The quick brown fox jumps over the lazy dog.";
        assert_eq!(
            h("simple", "fox", t),
            "The quick brown [fox] jumps over the lazy dog."
        );
        assert_eq!(
            h("simple", "FOX OR dog", t),
            "The quick brown [fox] jumps over the lazy [dog]."
        );
        // A phrase is one mark; a word only in an exclusion none.
        assert_eq!(
            h("simple", "\"quick brown\" -cat", t),
            "The [quick brown] fox jumps over the lazy dog."
        );
        assert_eq!(h("simple", "\"brown quick\"", t), t);
        assert_eq!(
            h("simple", "\"quick fox\"~1", t),
            "The [quick brown fox] jumps over the lazy dog."
        );
        assert_eq!(
            h("simple", "fox -lazy", t),
            "The quick brown [fox] jumps over the lazy dog."
        );
        // Prefixes and fuzzy terms; stemmed words by the analyzer.
        assert_eq!(
            h("simple", "jum* lazzy~1", t),
            "The quick brown fox [jumps] over the [lazy] dog."
        );
        assert_eq!(
            h("porter", "jumping", "He jumps; they jumped."),
            "He [jumps]; they [jumped]."
        );
        // Overlapping marks are one; nothing found, the text.
        assert_eq!(
            h("simple", "\"quick brown\" brown", t),
            "The [quick brown] fox jumps over the lazy dog."
        );
        assert_eq!(h("simple", "cat", t), t);
        assert_eq!(h("simple", "é*", "Égés és él"), "[Égés] [és] [él]");
    }

    #[test]
    fn snippets() {
        let t = "one two three four five six seven eight nine ten";
        assert_eq!(s("five", t, 3), "…four [five] six…");
        assert_eq!(s("one", t, 3), "[one] two three…");
        assert_eq!(s("ten", t, 3), "…eight nine [ten]");
        assert_eq!(s("cat", t, 3), "one two three…");
        assert_eq!(s("cat", t, 100), t);
        // The most distinct terms win over more marks of one.
        let t = "fox fox fox x x x x x dog x cat x";
        assert_eq!(s("fox dog cat", t, 5), "…x [dog] x [cat] x");
        assert_eq!(s("fox", "", 5), "");
    }
}
