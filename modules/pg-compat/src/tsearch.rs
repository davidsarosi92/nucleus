//! PostgreSQL's text search in pg-compat (PLAN.md §M14.21 T8): the types
//! `tsvector` and `tsquery` with PostgreSQL's text forms, `to_tsvector`,
//! `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery`,
//! `websearch_to_tsquery` and `@@` between them — evaluated as
//! PostgreSQL's `TS_execute` evaluates them, phrases' positions and their
//! negations too.
//!
//! The text search configurations are nucleus's analyzers by name
//! (`english`, `hungarian`, `simple`, …; `pg_catalog.` dropped): a
//! `tsvector` holds the terms an analyzer gives, at its positions plus
//! one.  So what PostgreSQL clients write for a GIN index —
//! `to_tsvector('english', body) @@ to_tsquery('english', 'cat & dog')` —
//! is `body @@ '("cat" AND "dog")'` by the analyzer `english`, which the
//! text index serves where it analyzes so: `@@` rewrites itself
//! ([`ScalarFunction::rewrite`]) when its query is a literal the text
//! query syntax says exactly (no weights, phrases of words a step apart,
//! no match on a text without terms), and is computed on the values
//! otherwise.
//!
//! Departures from PostgreSQL: the analyzers are nucleus's (UAX #29 words,
//! Snowball's stemmers and stop lists — PostgreSQL's own parser splits
//! some words otherwise); positions are not capped at 16,383 nor 256 a
//! term (the caps lose phrase matches in long texts; the text index keeps
//! every position); the configuration without one named is `simple`.

use std::ops::RangeInclusive;
use std::sync::Arc;

use nucleus_sql::ext::text::{self as nt, Analyzer};
use nucleus_sql::ext::{
    Arg, ArgInfo, CallContext, CanonicalBytes, DataType, Equality, Error, Expr, FnRef, Hashing,
    Registry, Result, Returns, ScalarFunction, Signature, Type, TypeRef, Value, Volatility,
};

/// The configuration of `to_tsvector(text)` and `text @@ tsquery`.
pub(crate) const DEFAULT_CONFIG: &str = "simple";

/// The analyzer a configuration names.
pub(crate) fn config(name: &str) -> Result<Arc<dyn Analyzer>> {
    let n = name.trim();
    let n = n.strip_prefix("pg_catalog.").unwrap_or(n);
    nt::named(&n.to_lowercase()).map_err(|_| {
        Error::undefined_object(format!(
            "text search configuration \"{name}\" does not exist"
        ))
    })
}

// ---------------------------------------------------------------------------
// tsvector

/// A position's weight: A 3, B 2, C 1, D 0 (PostgreSQL's numbering).
pub(crate) type Weight = u8;

/// A `tsvector`: terms in byte order, each with its positions (from 1,
/// ascending, each once) and their weights; a term from a literal may have
/// none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TsVector(pub(crate) Vec<(String, Vec<(u32, Weight)>)>);

impl TsVector {
    /// Terms gathered: sorted, merged, positions sorted and unique (the
    /// greater weight kept where one repeats).
    fn of(mut items: Vec<(String, Vec<(u32, Weight)>)>) -> TsVector {
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out: Vec<(String, Vec<(u32, Weight)>)> = Vec::with_capacity(items.len());
        for (t, p) in items {
            match out.last_mut() {
                Some((last, ps)) if *last == t => ps.extend(p),
                _ => out.push((t, p)),
            }
        }
        for (_, ps) in &mut out {
            ps.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            ps.dedup_by_key(|p| p.0);
        }
        TsVector(out)
    }

    fn analyzed(a: &dyn Analyzer, text: &str) -> TsVector {
        let items = nt::tokens(a, text)
            .into_iter()
            .map(|t| (t.term, vec![(t.position + 1, 0)]))
            .collect();
        TsVector::of(items)
    }

    fn output(&self) -> String {
        let mut out = String::new();
        for (i, (t, ps)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            quote(t, &mut out);
            for (j, (p, w)) in ps.iter().enumerate() {
                out.push(if j == 0 { ':' } else { ',' });
                out.push_str(&p.to_string());
                if *w > 0 {
                    out.push((b'A' + (3 - w)) as char);
                }
            }
        }
        out
    }

    fn input(text: &str) -> Result<TsVector> {
        let bad = |why: &str| {
            Error::invalid_text(format!(
                "invalid input syntax for type tsvector: \"{text}\" ({why})"
            ))
        };
        let mut s = Scan::new(text);
        let mut items = Vec::new();
        loop {
            s.skip_space();
            if s.done() {
                break;
            }
            let word = s.word(|c| c.is_whitespace() || c == ':').map_err(bad)?;
            let mut ps = Vec::new();
            if s.eat(':') {
                loop {
                    let p = s.number().ok_or_else(|| bad("a position expected"))?;
                    if p == 0 {
                        return Err(bad("positions start at 1"));
                    }
                    let w = match s.peek().map(|c| c.to_ascii_uppercase()) {
                        Some(c @ 'A'..='D') => {
                            s.next();
                            b'D' - c as u8
                        }
                        _ => 0,
                    };
                    ps.push((p, w));
                    if !s.eat(',') {
                        break;
                    }
                }
            }
            items.push((word, ps));
        }
        Ok(TsVector::of(items))
    }

    /// The entries `word` names: it alone, or every term it is a prefix of.
    pub(crate) fn entries<'a>(
        &'a self,
        word: &str,
        prefix: bool,
    ) -> &'a [(String, Vec<(u32, Weight)>)] {
        let from = self.0.partition_point(|(t, _)| t.as_str() < word);
        let to = if prefix {
            from + self.0[from..].partition_point(|(t, _)| t.starts_with(word))
        } else {
            from + usize::from(self.0.get(from).is_some_and(|(t, _)| t == word))
        };
        &self.0[from..to]
    }
}

/// PostgreSQL's quoting of a term: `'…'`, `'` and `\` doubled.
fn quote(t: &str, out: &mut String) {
    out.push('\'');
    for c in t.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
}

/// A cursor over a text form.
struct Scan<'a> {
    s: &'a str,
    at: usize,
}

impl<'a> Scan<'a> {
    fn new(s: &'a str) -> Self {
        Scan { s, at: 0 }
    }
    fn peek(&self) -> Option<char> {
        self.s[self.at..].chars().next()
    }
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }
    fn done(&self) -> bool {
        self.at >= self.s.len()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += c.len_utf8();
            true
        } else {
            false
        }
    }
    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.next();
        }
    }
    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.next();
        }
        self.s[start..self.at].parse().ok()
    }
    /// A word: `'quoted'` (`''` a quote, `\x` an `x`) or up to `end`, with
    /// `\x` an `x`.
    fn word(&mut self, end: impl Fn(char) -> bool) -> std::result::Result<String, &'static str> {
        let mut w = String::new();
        if self.eat('\'') {
            loop {
                match self.next() {
                    None => return Err("unterminated quoted string"),
                    Some('\\') => w.push(self.next().ok_or("an escape at the end")?),
                    Some('\'') if self.peek() == Some('\'') => {
                        self.next();
                        w.push('\'');
                    }
                    Some('\'') => break,
                    Some(c) => w.push(c),
                }
            }
            return Ok(w);
        }
        while let Some(c) = self.peek() {
            if end(c) {
                break;
            }
            self.next();
            if c == '\\' {
                w.push(self.next().ok_or("an escape at the end")?);
            } else {
                w.push(c);
            }
        }
        if w.is_empty() {
            return Err("a word expected");
        }
        Ok(w)
    }
}

// ---------------------------------------------------------------------------
// tsquery

/// A `tsquery`'s tree.  Built from a function's text, a leaf holds the
/// text's operand until it is analyzed (and `Stop` stands for one that
/// gave no term, until PostgreSQL's clean-up takes it out).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Q {
    Lex {
        word: String,
        prefix: bool,
        /// A bit a weight (`1 << w`); 0: any.
        weights: u8,
    },
    Stop,
    Not(Box<Q>),
    And(Box<Q>, Box<Q>),
    Or(Box<Q>, Box<Q>),
    /// The right this far after the left.
    Phrase(Box<Q>, Box<Q>, u32),
}

/// PostgreSQL's operator priorities.
fn priority(q: &Q) -> i32 {
    match q {
        Q::Not(_) => 4,
        Q::Phrase(..) => 3,
        Q::And(..) => 2,
        Q::Or(..) => 1,
        Q::Lex { .. } | Q::Stop => 5,
    }
}

/// The greatest distance PostgreSQL's `<N>` takes (`MAXENTRYPOS`).
const MAX_DISTANCE: u32 = 16_384;

fn infix(q: &Q, parent: i32, right_phrase: bool, out: &mut String) {
    match q {
        Q::Lex {
            word,
            prefix,
            weights,
        } => {
            quote(word, out);
            if *prefix || *weights != 0 {
                out.push(':');
                if *prefix {
                    out.push('*');
                }
                for (bit, c) in [(3, 'A'), (2, 'B'), (1, 'C'), (0, 'D')] {
                    if weights & (1 << bit) != 0 {
                        out.push(c);
                    }
                }
            }
        }
        Q::Stop => {}
        Q::Not(x) => {
            let p = priority(q);
            if p < parent {
                out.push_str("( ");
            }
            out.push('!');
            infix(x, p, false, out);
            if p < parent {
                out.push_str(" )");
            }
        }
        Q::And(l, r) | Q::Or(l, r) | Q::Phrase(l, r, _) => {
            let p = priority(q);
            let paren = p < parent || (matches!(q, Q::Phrase(..)) && right_phrase);
            if paren {
                out.push_str("( ");
            }
            infix(l, p, false, out);
            match q {
                Q::And(..) => out.push_str(" & "),
                Q::Or(..) => out.push_str(" | "),
                Q::Phrase(_, _, 1) => out.push_str(" <-> "),
                Q::Phrase(_, _, d) => out.push_str(&format!(" <{d}> ")),
                _ => unreachable!(),
            }
            infix(r, p, matches!(q, Q::Phrase(..)), out);
            if paren {
                out.push_str(" )");
            }
        }
    }
}

/// A query's text form; the empty query's is empty.
fn output(q: Option<&Q>) -> String {
    let mut out = String::new();
    if let Some(q) = q {
        infix(q, -1, false, &mut out);
    }
    out
}

/// The syntax a query's text is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Syntax {
    /// `to_tsquery`'s and a `tsquery` literal's: `!`, `&`, `|`, `<->`,
    /// `<N>`, `( )`, `:*` and weights.
    Standard,
    /// `websearch_to_tsquery`'s: words, `"phrases"`, `or`, `-`.
    Websearch,
}

fn is_operator(c: char) -> bool {
    matches!(c, '!' | '&' | '|' | '(' | ')' | '<')
}

/// A query's text parsed (PostgreSQL's `makepol`): operands as written.
struct Parser<'a> {
    s: Scan<'a>,
    depth: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn error(&self) -> Error {
        Error::syntax(format!("syntax error in tsquery: \"{}\"", self.text))
    }

    fn parse(text: &str, syntax: Syntax) -> Result<Option<Q>> {
        let mut p = Parser {
            s: Scan::new(text),
            depth: 0,
            text,
        };
        p.s.skip_space();
        if p.s.done() {
            return Ok(None);
        }
        let q = match syntax {
            Syntax::Standard => p.or()?,
            Syntax::Websearch => p.web_or()?,
        };
        p.s.skip_space();
        if !p.s.done() {
            return Err(p.error());
        }
        Ok(q)
    }

    // Standard: or := and ('|' and)*; and := phrase ('&' phrase)*;
    // phrase := not (<N> not)*; not := '!' not | atom.

    fn or(&mut self) -> Result<Option<Q>> {
        let mut l = self.and()?;
        loop {
            self.s.skip_space();
            if !self.s.eat('|') {
                return Ok(l);
            }
            let r = self.and()?;
            l = bin(l, r, Q::Or);
        }
    }

    fn and(&mut self) -> Result<Option<Q>> {
        let mut l = self.phrase()?;
        loop {
            self.s.skip_space();
            if !self.s.eat('&') {
                return Ok(l);
            }
            let r = self.phrase()?;
            l = bin(l, r, Q::And);
        }
    }

    fn phrase(&mut self) -> Result<Option<Q>> {
        let mut l = self.not()?;
        loop {
            self.s.skip_space();
            let Some(d) = self.distance()? else {
                return Ok(l);
            };
            let r = self.not()?;
            l = bin(l, r, |a, b| Q::Phrase(a, b, d));
        }
    }

    /// `<->` or `<N>` (consumed), else nothing consumed.
    fn distance(&mut self) -> Result<Option<u32>> {
        let rest = &self.s.s[self.s.at..];
        let Some(inner) = rest.strip_prefix('<') else {
            return Ok(None);
        };
        if let Some(after) = inner.strip_prefix("->") {
            self.s.at = self.s.s.len() - after.len();
            return Ok(Some(1));
        }
        let digits = inner.len() - inner.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 || !inner[digits..].starts_with('>') {
            return Ok(None);
        }
        let d: u64 = inner[..digits].parse().unwrap_or(u64::MAX);
        if d > u64::from(MAX_DISTANCE) {
            return Err(Error::invalid_parameter(format!(
                "distance in phrase operator must be an integer value between zero and {MAX_DISTANCE} inclusive"
            )));
        }
        self.s.at += 1 + digits + 1;
        Ok(Some(d as u32))
    }

    fn not(&mut self) -> Result<Option<Q>> {
        self.s.skip_space();
        if self.s.eat('!') {
            return Ok(self.not()?.map(|x| Q::Not(Box::new(x))));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Option<Q>> {
        self.s.skip_space();
        if self.s.eat('(') {
            self.depth += 1;
            if self.depth > 1000 {
                return Err(self.error());
            }
            let q = self.or()?;
            self.s.skip_space();
            if !self.s.eat(')') {
                return Err(self.error());
            }
            self.depth -= 1;
            return Ok(q);
        }
        if self.s.done() || self.s.peek().is_some_and(|c| is_operator(c) || c == ':') {
            return Err(self.error());
        }
        let word = self
            .s
            .word(|c| c.is_whitespace() || is_operator(c) || c == ':')
            .map_err(|_| self.error())?;
        let (mut prefix, mut weights) = (false, 0u8);
        if self.s.eat(':') {
            while let Some(c) = self.s.peek() {
                match c.to_ascii_uppercase() {
                    '*' => prefix = true,
                    'A' => weights |= 1 << 3,
                    'B' => weights |= 1 << 2,
                    'C' => weights |= 1 << 1,
                    'D' => weights |= 1,
                    _ => break,
                }
                self.s.next();
            }
        }
        Ok(Some(Q::Lex {
            word,
            prefix,
            weights,
        }))
    }

    // Websearch: or := and ('or' and)*; and := not (not)*; not := '-' not
    // | operand — other operators' characters skipped.

    fn web_or(&mut self) -> Result<Option<Q>> {
        let mut l = self.web_and()?;
        loop {
            if !self.web_or_operator() {
                return Ok(l);
            }
            let r = self.web_and()?;
            l = bin(l, r, Q::Or);
        }
    }

    /// PostgreSQL's `parse_or_operator`: `or`, not part of a word, and an
    /// operand somewhere after it.
    fn web_or_operator(&mut self) -> bool {
        self.web_skip();
        let rest = &self.s.s[self.s.at..];
        if !rest
            .as_bytes()
            .get(..2)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"or"))
        {
            return false;
        }
        let after = &rest[2..];
        match after.chars().next() {
            None => return false,
            Some(c) if c == '-' || c == '_' || c.is_alphanumeric() => return false,
            _ => {}
        }
        if after.trim_start().is_empty() {
            return false;
        }
        self.s.at += 2;
        true
    }

    /// Skips spaces and the operator characters websearch ignores.
    fn web_skip(&mut self) {
        while self
            .s
            .peek()
            .is_some_and(|c| c.is_whitespace() || is_operator(c))
        {
            self.s.next();
        }
    }

    fn web_and(&mut self) -> Result<Option<Q>> {
        let mut l = self.web_not()?;
        loop {
            self.web_skip();
            let at = self.s.at;
            if self.s.done() || self.web_or_operator() {
                self.s.at = at;
                return Ok(l);
            }
            let r = self.web_not()?;
            l = bin(l, r, Q::And);
        }
    }

    fn web_not(&mut self) -> Result<Option<Q>> {
        self.web_skip();
        if self.s.eat('-') {
            return Ok(self.web_not()?.map(|x| Q::Not(Box::new(x))));
        }
        if self.s.done() {
            return Ok(None);
        }
        if self.s.eat('"') {
            let rest = &self.s.s[self.s.at..];
            let len = rest.find('"').unwrap_or(rest.len());
            let word = rest[..len].to_string();
            self.s.at += len;
            self.s.eat('"');
            return Ok(Some(Q::Lex {
                word,
                prefix: false,
                weights: 0,
            }));
        }
        let rest = &self.s.s[self.s.at..];
        let len = rest
            .find(|c: char| c.is_whitespace() || is_operator(c))
            .unwrap_or(rest.len());
        let word = rest[..len].to_string();
        self.s.at += len;
        Ok(Some(Q::Lex {
            word,
            prefix: false,
            weights: 0,
        }))
    }
}

fn bin(l: Option<Q>, r: Option<Q>, f: impl Fn(Box<Q>, Box<Q>) -> Q) -> Option<Q> {
    match (l, r) {
        (Some(l), Some(r)) => Some(f(Box::new(l), Box::new(r))),
        (l, r) => l.or(r),
    }
}

/// A term chain: the terms an operand's text gives, each after the one
/// before it by their positions' difference (PostgreSQL joins an
/// operand's lexemes so); `Stop` for none.
fn chain(terms: Vec<nt::Token>, prefix: bool, weights: u8) -> Q {
    let mut it = terms.into_iter();
    let Some(first) = it.next() else {
        return Q::Stop;
    };
    let mut last = first.position;
    let mut q = Q::Lex {
        word: first.term,
        prefix,
        weights,
    };
    for t in it {
        let d = t.position - last;
        last = t.position;
        q = Q::Phrase(
            Box::new(q),
            Box::new(Q::Lex {
                word: t.term,
                prefix,
                weights,
            }),
            d,
        );
    }
    q
}

/// Each operand analyzed by `a`.
fn analyze(q: &Q, a: &dyn Analyzer) -> Q {
    match q {
        Q::Lex {
            word,
            prefix,
            weights,
        } => chain(nt::tokens(a, word), *prefix, *weights),
        Q::Stop => Q::Stop,
        Q::Not(x) => Q::Not(Box::new(analyze(x, a))),
        Q::And(l, r) => Q::And(Box::new(analyze(l, a)), Box::new(analyze(r, a))),
        Q::Or(l, r) => Q::Or(Box::new(analyze(l, a)), Box::new(analyze(r, a))),
        Q::Phrase(l, r, d) => Q::Phrase(Box::new(analyze(l, a)), Box::new(analyze(r, a)), *d),
    }
}

/// PostgreSQL's `clean_stopword_intree`: the operands that gave no term
/// taken out, the phrase distances across them kept.  Returns the tree
/// (none: nothing left) and the distances to add on its left and right.
fn clean(q: Q) -> (Option<Q>, u32, u32) {
    let (a, b, phrase) = match q {
        Q::Stop => return (None, 0, 0),
        Q::Lex { .. } => return (Some(q), 0, 0),
        Q::Not(x) => {
            let (x, l, r) = clean(*x);
            return (x.map(|x| Q::Not(Box::new(x))), l, r);
        }
        Q::And(a, b) => (a, b, None),
        Q::Or(a, b) => (a, b, Some(None)),
        Q::Phrase(a, b, d) => (a, b, Some(Some(d))),
    };
    // `phrase`: None an AND, Some(None) an OR, Some(Some(d)) a phrase.
    let d = phrase.flatten();
    let (l, lladd, lradd) = clean(*a);
    let (r, rladd, rradd) = clean(*b);
    match (l, r, d) {
        (None, None, Some(d)) => {
            let x = lladd + d + rladd;
            (None, x, x)
        }
        (None, None, None) => {
            let x = lladd.max(rladd);
            (None, x, x)
        }
        (None, Some(r), Some(d)) => (Some(r), lladd + d + rladd, rradd),
        (None, Some(r), None) => (Some(r), rladd, rradd),
        (Some(l), None, Some(d)) => (Some(l), lladd, lradd + d + rradd),
        (Some(l), None, None) => (Some(l), lladd, lradd),
        (Some(l), Some(r), d) => {
            let (l, r) = (Box::new(l), Box::new(r));
            match (phrase, d) {
                (_, Some(d)) => (
                    // PostgreSQL's clean-up adds to the distance unchecked
                    // (`cat <16384> the <-> dog` is `<16385>`), in an int16.
                    Some(Q::Phrase(l, r, (d + lradd + rladd).min(i16::MAX as u32))),
                    lladd,
                    rradd,
                ),
                (None, None) => (Some(Q::And(l, r)), 0, 0),
                (Some(_), None) => (Some(Q::Or(l, r)), 0, 0),
            }
        }
    }
}

/// The functions making queries from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Maker {
    ToTsquery,
    Plain,
    Phrase,
    Websearch,
}

impl Maker {
    fn name(self) -> &'static str {
        match self {
            Maker::ToTsquery => "to_tsquery",
            Maker::Plain => "plainto_tsquery",
            Maker::Phrase => "phraseto_tsquery",
            Maker::Websearch => "websearch_to_tsquery",
        }
    }

    /// The query as written, its operands not yet analyzed (`plainto`'s:
    /// the terms, already).
    fn written(self, a: &dyn Analyzer, text: &str) -> Result<Option<Q>> {
        let lex = |word: String| Q::Lex {
            word,
            prefix: false,
            weights: 0,
        };
        Ok(match self {
            Maker::ToTsquery => Parser::parse(text, Syntax::Standard)?,
            Maker::Websearch => Parser::parse(text, Syntax::Websearch)?,
            Maker::Phrase => Some(lex(text.to_string())),
            Maker::Plain => nt::tokens(a, text)
                .into_iter()
                .map(|t| lex(text[t.start..t.end].to_string()))
                .reduce(|l, r| Q::And(Box::new(l), Box::new(r))),
        })
    }

    /// The query, as PostgreSQL builds it: written, analyzed, cleaned.
    fn query(self, a: &dyn Analyzer, text: &str) -> Result<Option<Q>> {
        if self == Maker::Plain {
            // All the text's terms, each required.
            return Ok(nt::tokens(a, text)
                .into_iter()
                .map(|t| Q::Lex {
                    word: t.term,
                    prefix: false,
                    weights: 0,
                })
                .reduce(|l, r| Q::And(Box::new(l), Box::new(r))));
        }
        Ok(self.written(a, text)?.and_then(|q| clean(analyze(&q, a)).0))
    }
}

/// A query's text form read: its operands as written (a `tsquery`
/// literal's are terms already).
fn query_input(text: &str) -> Result<Option<Q>> {
    Parser::parse(text, Syntax::Standard)
}

// ---------------------------------------------------------------------------
// Evaluation: PostgreSQL's TS_execute.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tri {
    No,
    Yes,
    Maybe,
}

/// Positions a phrase's part matches at (`negate`: where it does not),
/// and the match's width less one.
#[derive(Default)]
pub(crate) struct Phr {
    pub(crate) pos: Vec<u32>,
    pub(crate) negate: bool,
    pub(crate) width: u32,
}

const L_ONLY: u8 = 1;
const R_ONLY: u8 = 2;
const BOTH: u8 = 4;

/// PostgreSQL's `TS_phrase_output`: the two position lists merged as
/// `emit` says, each moved by its offset.
pub(crate) fn output_positions(
    l: &Phr,
    r: &Phr,
    emit: u8,
    lo: u32,
    ro: u32,
    out: Option<&mut Phr>,
) -> Tri {
    let (mut i, mut j) = (0, 0);
    let mut got = Vec::new();
    while i < l.pos.len() || j < r.pos.len() {
        let lp = match l.pos.get(i) {
            Some(p) => u64::from(p + lo),
            None if emit & R_ONLY == 0 => break,
            None => u64::MAX,
        };
        let rp = match r.pos.get(j) {
            Some(p) => u64::from(p + ro),
            None if emit & L_ONLY == 0 => break,
            None => u64::MAX,
        };
        let mut o = 0;
        if lp < rp {
            if emit & L_ONLY != 0 {
                o = lp;
            }
            i += 1;
        } else if lp == rp {
            if emit & BOTH != 0 {
                o = rp;
            }
            i += 1;
            j += 1;
        } else {
            if emit & R_ONLY != 0 {
                o = rp;
            }
            j += 1;
        }
        if o > 0 {
            if out.is_none() {
                return Tri::Yes;
            }
            got.push(o as u32);
        }
    }
    let yes = !got.is_empty();
    if let Some(out) = out {
        out.pos = got;
    }
    if yes { Tri::Yes } else { Tri::No }
}

/// PostgreSQL's `checkcondition_str`: whether `v` has the term (with
/// `data`: at which positions).
fn check(v: &TsVector, word: &str, prefix: bool, weights: u8, data: Option<&mut Phr>) -> Tri {
    let entries = v.entries(word, prefix);
    match data {
        None => {
            let yes = entries.iter().any(|(_, ps)| {
                ps.is_empty() || weights == 0 || ps.iter().any(|(_, w)| weights & (1 << w) != 0)
            });
            if yes { Tri::Yes } else { Tri::No }
        }
        Some(data) => {
            let mut pos = Vec::new();
            let mut found = false;
            for (_, ps) in entries {
                if ps.is_empty() {
                    return Tri::Maybe;
                }
                let before = pos.len();
                pos.extend(
                    ps.iter()
                        .filter(|(_, w)| weights == 0 || weights & (1 << w) != 0)
                        .map(|(p, _)| *p),
                );
                found |= pos.len() > before;
            }
            if !found {
                return Tri::No;
            }
            pos.sort_unstable();
            pos.dedup();
            data.pos = pos;
            Tri::Yes
        }
    }
}

/// Where a query's operands are looked up: a vector, or the part of a
/// text in view (a ranking's cover, a headline's).
pub(crate) trait Source {
    /// PostgreSQL's `TSExecuteCallback`: whether the operand `op` (a
    /// [`Q::Lex`] of the query) is there — with `data`, at which positions.
    fn check(&self, op: &Q, data: Option<&mut Phr>) -> Tri;
}

impl Source for TsVector {
    fn check(&self, op: &Q, data: Option<&mut Phr>) -> Tri {
        match op {
            Q::Lex {
                word,
                prefix,
                weights,
            } => check(self, word, *prefix, *weights, data),
            _ => Tri::No,
        }
    }
}

/// PostgreSQL's `TS_phrase_execute`.
pub(crate) fn phrase(q: &Q, v: &dyn Source, mut data: Option<&mut Phr>) -> Tri {
    match q {
        Q::Lex { .. } => v.check(q, data),
        Q::Stop => Tri::No,
        Q::Not(x) => {
            let d = data.expect("a NOT under a phrase has its data");
            match phrase(x, v, Some(d)) {
                Tri::No => {
                    d.negate = true;
                    Tri::Yes
                }
                Tri::Yes if !d.pos.is_empty() => {
                    d.negate = !d.negate;
                    Tri::Yes
                }
                Tri::Yes => {
                    d.negate = false;
                    Tri::No
                }
                Tri::Maybe => Tri::Maybe,
            }
        }
        Q::Phrase(l, r, _) | Q::And(l, r) => {
            let (mut ld, mut rd) = (Phr::default(), Phr::default());
            let lm = phrase(l, v, Some(&mut ld));
            if lm == Tri::No {
                return Tri::No;
            }
            let rm = phrase(r, v, Some(&mut rd));
            if rm == Tri::No {
                return Tri::No;
            }
            if lm == Tri::Maybe || rm == Tri::Maybe {
                return Tri::Maybe;
            }
            let (lo, ro) = match q {
                Q::Phrase(_, _, d) => {
                    if let Some(out) = data.as_deref_mut() {
                        out.width = d + ld.width + rd.width;
                    }
                    (d + rd.width, 0)
                }
                _ => {
                    let w = ld.width.max(rd.width);
                    if let Some(out) = data.as_deref_mut() {
                        out.width = w;
                    }
                    (w - ld.width, w - rd.width)
                }
            };
            match (ld.negate, rd.negate) {
                (true, true) => {
                    output_positions(
                        &ld,
                        &rd,
                        BOTH | L_ONLY | R_ONLY,
                        lo,
                        ro,
                        data.as_deref_mut(),
                    );
                    if let Some(out) = data {
                        out.negate = true;
                    }
                    Tri::Yes
                }
                (true, false) => output_positions(&ld, &rd, R_ONLY, lo, ro, data),
                (false, true) => output_positions(&ld, &rd, L_ONLY, lo, ro, data),
                (false, false) => output_positions(&ld, &rd, BOTH, lo, ro, data),
            }
        }
        Q::Or(l, r) => {
            let (mut ld, mut rd) = (Phr::default(), Phr::default());
            let lm = phrase(l, v, Some(&mut ld));
            let rm = phrase(r, v, Some(&mut rd));
            if lm == Tri::No && rm == Tri::No {
                return Tri::No;
            }
            if lm == Tri::Maybe || rm == Tri::Maybe {
                return Tri::Maybe;
            }
            if lm == Tri::No {
                ld.width = 0;
            }
            if rm == Tri::No {
                rd.width = 0;
            }
            let w = ld.width.max(rd.width);
            let (lo, ro) = (w - ld.width, w - rd.width);
            let out = data.expect("an OR under a phrase has its data");
            out.width = w;
            let (emit, negate) = match (ld.negate, rd.negate) {
                (true, true) => (BOTH, true),
                (true, false) => (L_ONLY, true),
                (false, true) => (R_ONLY, true),
                (false, false) => (BOTH | L_ONLY | R_ONLY, false),
            };
            let t = output_positions(&ld, &rd, emit, lo, ro, Some(out));
            if negate {
                out.negate = true;
                Tri::Yes
            } else {
                t
            }
        }
    }
}

/// PostgreSQL's `TS_execute_recurse`.
pub(crate) fn execute(q: &Q, v: &dyn Source) -> Tri {
    match q {
        Q::Lex { .. } => v.check(q, None),
        Q::Stop => Tri::No,
        Q::Not(x) => match execute(x, v) {
            Tri::No => Tri::Yes,
            Tri::Yes => Tri::No,
            Tri::Maybe => Tri::Maybe,
        },
        Q::And(l, r) => {
            let lm = execute(l, v);
            if lm == Tri::No {
                return Tri::No;
            }
            match execute(r, v) {
                Tri::No => Tri::No,
                Tri::Yes => lm,
                Tri::Maybe => Tri::Maybe,
            }
        }
        Q::Or(l, r) => {
            let lm = execute(l, v);
            if lm == Tri::Yes {
                return Tri::Yes;
            }
            match execute(r, v) {
                Tri::No => lm,
                Tri::Yes => Tri::Yes,
                Tri::Maybe => Tri::Maybe,
            }
        }
        Q::Phrase(..) => match phrase(q, v, None) {
            Tri::Yes => Tri::Yes,
            _ => Tri::No,
        },
    }
}

/// `v @@ q` as PostgreSQL's `ts_match_vq`: the empty query matches
/// nothing; a query of exclusions matches the empty vector.
fn matches(v: &TsVector, q: Option<&Q>) -> bool {
    q.is_some_and(|q| execute(q, v) != Tri::No)
}

// ---------------------------------------------------------------------------
// The text query a call of a maker is, for the text index.

/// Characters the text query syntax gives a meaning to inside a phrase.
fn plain_phrase(s: &str) -> String {
    s.replace(['"', '\\'], " ")
}

/// Whether `a` gives `text` the terms at the positions `want` (relative
/// to the first).
fn analyzes_as(a: &dyn Analyzer, text: &str, want: &[(String, u32)]) -> bool {
    let got = nt::tokens(a, text);
    let base = got.first().map_or(0, |t| t.position);
    got.len() == want.len()
        && got
            .iter()
            .zip(want)
            .all(|(g, (t, p))| g.term == *t && g.position - base == *p)
}

/// The text query (nucleus's syntax) that finds exactly the texts the
/// written query `q` matches by `a`, where it says it: `None` where it
/// cannot (weights; phrases over anything but words a step apart).
fn text_query(q: &Q, a: &dyn Analyzer) -> Option<String> {
    match q {
        Q::Lex {
            word,
            prefix: false,
            weights: 0,
        } => words(&[word.as_str()], a),
        Q::Lex {
            word,
            prefix: true,
            weights: 0,
        } => {
            let t = nt::tokens(a, word);
            match t.as_slice() {
                // A stop word: dropped by both.
                [] => words(&[word.as_str()], a),
                [t] if a.normalize(&t.term) == t.term
                    && !t.term.is_empty()
                    && !t
                        .term
                        .chars()
                        .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\\')) =>
                {
                    Some(format!("{}*", t.term))
                }
                _ => None,
            }
        }
        Q::Lex { .. } | Q::Stop => None,
        Q::Not(x) => Some(format!("(NOT {})", text_query(x, a)?)),
        Q::And(l, r) => Some(format!("({} AND {})", text_query(l, a)?, text_query(r, a)?)),
        Q::Or(l, r) => Some(format!("({} OR {})", text_query(l, a)?, text_query(r, a)?)),
        Q::Phrase(..) => {
            let mut ws = Vec::new();
            flat_phrase(q, &mut ws)?;
            words(&ws, a)
        }
    }
}

/// A phrase's words in order, every distance 1 and every operand a word.
fn flat_phrase<'q>(q: &'q Q, out: &mut Vec<&'q str>) -> Option<()> {
    match q {
        Q::Lex {
            word,
            prefix: false,
            weights: 0,
        } => {
            out.push(word);
            Some(())
        }
        Q::Phrase(l, r, 1) => {
            flat_phrase(l, out)?;
            flat_phrase(r, out)
        }
        _ => None,
    }
}

/// Words in a row as a quoted phrase of the text query syntax — where the
/// analyzer gives the phrase the terms PostgreSQL's chain of them has, at
/// the same distances (a word after the one before by one position more
/// than that one's span, a word with no term one position).
fn words(ws: &[&str], a: &dyn Analyzer) -> Option<String> {
    let mut want = Vec::new();
    let mut at = 0u32;
    for w in ws {
        let t = nt::tokens(a, w);
        let base = t.first().map_or(0, |t| t.position);
        let span = t.last().map_or(0, |t| t.position - base);
        want.extend(t.into_iter().map(|t| (t.term, at + t.position - base)));
        at += span + 1;
    }
    // Relative to the first term, as the analyzer of the phrase gives them.
    let first = want.first().map_or(0, |t| t.1);
    for t in &mut want {
        t.1 -= first;
    }
    let text = plain_phrase(&ws.join(" "));
    analyzes_as(a, &text, &want).then(|| format!("\"{text}\""))
}

// ---------------------------------------------------------------------------
// Types

pub(crate) fn tsvector_type() -> TypeRef {
    TypeRef::named("tsvector").expect("pg-compat registered tsvector")
}

pub(crate) fn tsquery_type() -> TypeRef {
    TypeRef::named("tsquery").expect("pg-compat registered tsquery")
}

fn vector_value(v: &TsVector) -> Value {
    Value::ext(tsvector_type(), v.output().into_bytes())
}

fn query_value(q: Option<&Q>) -> Value {
    Value::ext(tsquery_type(), output(q).into_bytes())
}

fn text_of(v: &Value) -> Result<&str> {
    v.ext_bytes()
        .and_then(|b| std::str::from_utf8(b).ok())
        .ok_or_else(|| Error::internal("a text search value's bytes"))
}

pub(crate) fn vector_of(v: &Value) -> Result<TsVector> {
    TsVector::input(text_of(v)?)
}

pub(crate) fn query_of(v: &Value) -> Result<Option<Q>> {
    query_input(text_of(v)?)
}

// ---------------------------------------------------------------------------
// Binary forms: PostgreSQL's `tsvectorsend` / `tsvectorrecv` and
// `tsquerysend` / `tsqueryrecv` (its wire protocol's binary format).

/// The greatest position PostgreSQL's form holds (`MAXENTRYPOS - 1`), the
/// most positions a term (`MAXNUMPOS`), the longest term (`MAXSTRLEN`).
const MAX_POSITION: u32 = 16_383;
const MAX_POSITIONS: usize = 256;
const MAX_TERM: usize = 2047;
/// The deepest query a binary form may give (the value is a tree the
/// server walks).
const MAX_DEPTH: usize = 10_000;

fn bad_binary(what: impl Into<String>) -> Error {
    Error {
        code: "22P03",
        message: what.into(),
        hint: None,
    }
}

/// A cursor over a binary form.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (a, b) = self
            .0
            .split_at_checked(n)
            .ok_or_else(|| bad_binary("insufficient data left in message"))?;
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    /// A string ended by a zero byte.
    fn cstring(&mut self) -> Result<String> {
        let n = self
            .0
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| bad_binary("invalid string in message"))?;
        let s = std::str::from_utf8(&self.0[..n])
            .map_err(|_| bad_binary("invalid string in message"))?
            .to_string();
        self.0 = &self.0[n + 1..];
        Ok(s)
    }
}

/// A `tsvector`'s binary form: the term count, then each term — its bytes
/// and a zero, its position count, each position with its weight in the
/// top two bits.  PostgreSQL's values hold positions up to 16,383 and 256
/// a term; nucleus's hold any (DOCS.md §8.22): past those, a value is sent
/// as PostgreSQL would have kept it — positions capped, each once (the
/// greater weight), the first 256.
pub(crate) fn vector_send(v: &Value) -> Result<Vec<u8>> {
    Ok(vector_bytes(&vector_of(v)?))
}

fn vector_bytes(tv: &TsVector) -> Vec<u8> {
    let mut out = (tv.0.len() as u32).to_be_bytes().to_vec();
    for (t, ps) in &tv.0 {
        out.extend_from_slice(t.as_bytes());
        out.push(0);
        let mut kept: Vec<(u32, Weight)> = Vec::with_capacity(ps.len().min(MAX_POSITIONS));
        for &(p, w) in ps {
            let p = p.min(MAX_POSITION);
            let full = kept.len() == MAX_POSITIONS;
            match kept.last_mut() {
                Some(last) if last.0 == p => last.1 = last.1.max(w),
                _ if full => break,
                _ => kept.push((p, w)),
            }
        }
        out.extend_from_slice(&(kept.len() as u16).to_be_bytes());
        for (p, w) in kept {
            out.extend_from_slice(&((u16::from(w) << 14) | p as u16).to_be_bytes());
        }
    }
    out
}

/// A `tsvector` from its binary form, checked as PostgreSQL checks it; its
/// terms sorted and merged.
pub(crate) fn vector_recv(b: &[u8]) -> Result<Value> {
    Ok(vector_value(&vector_from(b)?))
}

fn vector_from(b: &[u8]) -> Result<TsVector> {
    let mut r = Bytes(b);
    let n = r.u32()?;
    if n > (1 << 20) - 1 {
        return Err(bad_binary("invalid size of tsvector"));
    }
    let mut items = Vec::with_capacity((n as usize).min(1024));
    for _ in 0..n {
        let term = r.cstring()?;
        if term.is_empty() {
            return Err(bad_binary("invalid tsvector: empty lexeme"));
        }
        if term.len() > MAX_TERM {
            return Err(bad_binary("invalid tsvector: lexeme too long"));
        }
        let np = r.u16()?;
        if usize::from(np) > MAX_POSITIONS {
            return Err(bad_binary("unexpected number of tsvector positions"));
        }
        let mut ps: Vec<(u32, Weight)> = Vec::with_capacity(usize::from(np));
        for _ in 0..np {
            let x = r.u16()?;
            let p = u32::from(x & 0x3fff);
            if ps.last().is_some_and(|l| p <= l.0) {
                return Err(bad_binary("position information is misordered"));
            }
            // nucleus's positions start at 1, as its text form's do.
            if p == 0 {
                return Err(bad_binary("invalid tsvector: position 0"));
            }
            ps.push((p, (x >> 14) as u8));
        }
        items.push((term, ps));
    }
    if !r.0.is_empty() {
        return Err(bad_binary("incorrect binary data format"));
    }
    Ok(TsVector::of(items))
}

/// A `tsquery`'s binary form: the item count, then the items in prefix
/// order — an operator before its right operand, then its left —, an
/// operand `1`, its weights (a bit each: A 8, B 4, C 2, D 1), whether a
/// prefix, its bytes and a zero; an operator `2`, `!` 1, `&` 2, `|` 3,
/// `<N>` 4 and its distance.
pub(crate) fn query_send(v: &Value) -> Result<Vec<u8>> {
    Ok(query_bytes(query_of(v)?.as_ref()))
}

fn query_bytes(q: Option<&Q>) -> Vec<u8> {
    fn items(q: &Q, n: &mut u32, out: &mut Vec<u8>) {
        *n += 1;
        match q {
            Q::Lex {
                word,
                prefix,
                weights,
            } => {
                out.extend_from_slice(&[1, *weights, u8::from(*prefix)]);
                out.extend_from_slice(word.as_bytes());
                out.push(0);
            }
            // Never in a value: the clean-up takes it out.
            Q::Stop => *n -= 1,
            Q::Not(x) => {
                out.extend_from_slice(&[2, 1]);
                items(x, n, out);
            }
            Q::And(l, r) | Q::Or(l, r) | Q::Phrase(l, r, _) => {
                match q {
                    Q::And(..) => out.extend_from_slice(&[2, 2]),
                    Q::Or(..) => out.extend_from_slice(&[2, 3]),
                    Q::Phrase(_, _, d) => {
                        out.extend_from_slice(&[2, 4]);
                        out.extend_from_slice(&(*d as u16).to_be_bytes());
                    }
                    _ => unreachable!(),
                }
                items(r, n, out);
                items(l, n, out);
            }
        }
    }
    let (mut n, mut body) = (0, Vec::new());
    if let Some(q) = q {
        items(q, &mut n, &mut body);
    }
    let mut out = n.to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// A `tsquery` from its binary form, checked as PostgreSQL checks it.
pub(crate) fn query_recv(b: &[u8]) -> Result<Value> {
    Ok(query_value(query_from(b)?.as_ref()))
}

fn query_from(b: &[u8]) -> Result<Option<Q>> {
    enum Item {
        Operand(String, u8, bool),
        Operator(u8, u32),
    }
    let mut r = Bytes(b);
    let size = r.u32()? as usize;
    let mut items = Vec::with_capacity(size.min(1024));
    for i in 0..size {
        items.push(match r.u8()? {
            1 => {
                let (weights, prefix, word) = (r.u8()?, r.u8()?, r.cstring()?);
                if weights > 0xF {
                    return Err(bad_binary("invalid tsquery: invalid weight bitmap"));
                }
                if word.is_empty() {
                    return Err(bad_binary("invalid tsquery: empty operand"));
                }
                if word.len() > MAX_TERM {
                    return Err(bad_binary("invalid tsquery: operand too long"));
                }
                Item::Operand(word, weights, prefix != 0)
            }
            2 => {
                let op = r.u8()?;
                if !(1..=4).contains(&op) {
                    return Err(bad_binary(format!(
                        "invalid tsquery: unrecognized operator type {op}"
                    )));
                }
                if i == size - 1 {
                    return Err(bad_binary("invalid pointer to right operand"));
                }
                let d = if op == 4 { u32::from(r.u16()?) } else { 0 };
                if d > MAX_DISTANCE {
                    return Err(bad_binary(format!(
                        "invalid tsquery: invalid phrase distance {d}"
                    )));
                }
                Item::Operator(op, d)
            }
            t => {
                return Err(bad_binary(format!("unrecognized tsquery node type: {t}")));
            }
        });
    }
    if !r.0.is_empty() {
        return Err(bad_binary("incorrect binary data format"));
    }
    // Prefix order read backwards: each operand's tree on a stack, each
    // operator taking its right operand, then its left, off it — no
    // recursion however deep the query.
    let mut stack: Vec<(Q, usize)> = Vec::new();
    let pop = |stack: &mut Vec<(Q, usize)>| {
        stack
            .pop()
            .ok_or_else(|| bad_binary("malformed tsquery: operand not found"))
    };
    for item in items.into_iter().rev() {
        let node = match item {
            Item::Operand(word, weights, prefix) => (
                Q::Lex {
                    word,
                    prefix,
                    weights,
                },
                1,
            ),
            Item::Operator(1, _) => {
                let (x, d) = pop(&mut stack)?;
                (Q::Not(Box::new(x)), d + 1)
            }
            Item::Operator(op, dist) => {
                let (r, dr) = pop(&mut stack)?;
                let (l, dl) = pop(&mut stack)?;
                let (l, r) = (Box::new(l), Box::new(r));
                let q = match op {
                    2 => Q::And(l, r),
                    3 => Q::Or(l, r),
                    _ => Q::Phrase(l, r, dist),
                };
                (q, dl.max(dr) + 1)
            }
        };
        if node.1 > MAX_DEPTH {
            return Err(bad_binary("invalid tsquery: nested too deeply"));
        }
        stack.push(node);
    }
    if stack.len() > 1 {
        return Err(bad_binary("malformed tsquery: extra nodes"));
    }
    Ok(stack.pop().map(|(q, _)| q))
}

/// `tsvector`.
struct TsVectorType;

impl Type for TsVectorType {
    fn name(&self) -> &str {
        "tsvector"
    }
    fn category(&self) -> &str {
        "text search"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Ok(vector_value(&TsVector::input(text)?))
    }
    fn output(&self, v: &Value) -> String {
        text_of(v).unwrap_or_default().to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn examples(&self) -> Vec<Value> {
        ["", "a", "'a b':1,3A 'it''s' z:2B,7C", "'\\\\x'"]
            .iter()
            .map(|t| self.input(t).expect("an example parses"))
            .collect()
    }
}

/// `tsquery`.
struct TsQueryType;

impl Type for TsQueryType {
    fn name(&self) -> &str {
        "tsquery"
    }
    fn category(&self) -> &str {
        "text search"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Ok(query_value(query_input(text)?.as_ref()))
    }
    fn output(&self, v: &Value) -> String {
        text_of(v).unwrap_or_default().to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn examples(&self) -> Vec<Value> {
        [
            "",
            "a",
            "'a' & !( 'b' | 'c':*AB )",
            "( 'a' <-> 'b' ) <2> 'c' & 'd' <-> ( 'e' <-> 'f' )",
        ]
        .iter()
        .map(|t| self.input(t).expect("an example parses"))
        .collect()
    }
}

// ---------------------------------------------------------------------------
// Functions

/// `to_tsvector([cfg,] text)`.
struct ToTsvector;

impl ScalarFunction for ToTsvector {
    fn name(&self) -> &str {
        "to_tsvector"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=2
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); args.len()],
            Returns::Type(Some(DataType::Ext(tsvector_type()))),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (cfg, text) = config_and_text(args)?;
        Ok(vector_value(&TsVector::analyzed(
            config(cfg)?.as_ref(),
            text,
        )))
    }
}

/// `tsvectorsend(tsvector)`, `tsquerysend(tsquery)`: a value's binary
/// form, as PostgreSQL's wire protocol carries it.
struct SendFn {
    query: bool,
}

impl ScalarFunction for SendFn {
    fn name(&self) -> &str {
        if self.query {
            "tsquerysend"
        } else {
            "tsvectorsend"
        }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        let ty = if self.query {
            tsquery_type()
        } else {
            tsvector_type()
        };
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Ext(ty))],
            Returns::Type(Some(DataType::Bytea)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let b = if self.query {
            query_send(&args[0])?
        } else {
            vector_send(&args[0])?
        };
        Ok(Value::Bytea(b.into()))
    }
}

fn text(v: &Value) -> Result<&str> {
    match v {
        Value::Text(t) => Ok(t.as_str()),
        _ => Err(Error::internal("a text search function of texts")),
    }
}

fn config_and_text(args: &[Value]) -> Result<(&str, &str)> {
    match args {
        [t] => Ok((DEFAULT_CONFIG, text(t)?)),
        [c, t] => Ok((text(c)?, text(t)?)),
        _ => Err(Error::internal("a text search function's arguments")),
    }
}

/// `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery`,
/// `websearch_to_tsquery` (`[cfg,] text`).
struct MakeQuery(Maker);

impl ScalarFunction for MakeQuery {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=2
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); args.len()],
            Returns::Type(Some(DataType::Ext(tsquery_type()))),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (cfg, text) = config_and_text(args)?;
        let q = self.0.query(config(cfg)?.as_ref(), text)?;
        Ok(query_value(q.as_ref()))
    }
}

/// Which side of `@@` is which.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operands {
    VectorQuery,
    QueryVector,
    TextQuery,
}

/// `@@` between a `tsvector` (or a text, by the default configuration)
/// and a `tsquery`.  With `text`: the text index's `@@`, which a call
/// rewrites itself to where it can.
struct Match {
    operands: Operands,
    text: Option<FnRef>,
}

impl Match {
    /// `to_tsvector([cfg,] e) @@ maker([cfg,] 'literal')` as `e @@ q` by
    /// the analyzer — the same rows, where `q` says the query exactly.
    fn as_text_match(&self, args: &[Expr]) -> Option<Expr> {
        let text = self.text.as_ref()?;
        let (v, q) = match (self.operands, args) {
            (Operands::VectorQuery, [v, q]) => (v, q),
            (Operands::QueryVector, [q, v]) => (v, q),
            _ => return None,
        };
        let literal = |e: &Expr| match e {
            Expr::Literal(Value::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let Expr::Call(f, vargs) = v else {
            return None;
        };
        let (vcfg, column) = match (f.name(), vargs.as_slice()) {
            ("to_tsvector", [c]) => (DEFAULT_CONFIG.to_string(), c),
            ("to_tsvector", [cfg, c]) => (literal(cfg)?, c),
            _ => return None,
        };
        let Expr::Call(g, qargs) = q else {
            return None;
        };
        let maker = [
            Maker::ToTsquery,
            Maker::Plain,
            Maker::Phrase,
            Maker::Websearch,
        ]
        .into_iter()
        .find(|m| m.name() == g.name())?;
        let (qcfg, qtext) = match qargs.as_slice() {
            [t] => (DEFAULT_CONFIG.to_string(), literal(t)?),
            [cfg, t] => (literal(cfg)?, literal(t)?),
            _ => return None,
        };
        let a = config(&vcfg).ok()?;
        if config(&qcfg).ok()?.name() != a.name() {
            return None;
        }
        // The empty query (stop words only) matches nothing; left to the
        // values.
        maker.query(a.as_ref(), &qtext).ok()??;
        let written = maker.written(a.as_ref(), &qtext).ok()??;
        let tq = text_query(&written, a.as_ref())?;
        Some(Expr::Call(
            text.clone(),
            vec![
                column.clone(),
                Expr::Literal(Value::Text(tq)),
                Expr::Literal(Value::Text(a.name().to_string())),
            ],
        ))
    }
}

impl ScalarFunction for Match {
    fn name(&self) -> &str {
        "@@"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        let (v, q) = (
            DataType::Ext(tsvector_type()),
            DataType::Ext(tsquery_type()),
        );
        let args = match self.operands {
            Operands::VectorQuery => vec![Arg::Untyped(v), Arg::Untyped(q)],
            Operands::QueryVector => vec![Arg::Untyped(q), Arg::Untyped(v)],
            Operands::TextQuery => vec![Arg::Convert(DataType::Text), Arg::Untyped(q)],
        };
        Ok(Signature::new(args, Returns::Type(Some(DataType::Bool))))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn rewrite(&self, args: &[Expr]) -> Option<Expr> {
        self.as_text_match(args)
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let (v, q) = match self.operands {
            Operands::VectorQuery => (vector_of(&args[0])?, &args[1]),
            Operands::QueryVector => (vector_of(&args[1])?, &args[0]),
            Operands::TextQuery => {
                let Value::Text(t) = &args[0] else {
                    return Err(Error::internal("text @@ tsquery of a text"));
                };
                (
                    TsVector::analyzed(config(DEFAULT_CONFIG)?.as_ref(), t),
                    &args[1],
                )
            }
        };
        Ok(Value::Bool(matches(&v, query_of(q)?.as_ref())))
    }
}

/// Registers the types, the functions and `@@` (pg-compat's
/// registration does).
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(TsVectorType))?;
    r.add_type(Arc::new(TsQueryType))?;
    r.add_alias("regconfig", DataType::Text)?;
    r.add_function(&["to_tsvector"], Arc::new(ToTsvector))?;
    r.add_function(&["tsvectorsend"], Arc::new(SendFn { query: false }))?;
    r.add_function(&["tsquerysend"], Arc::new(SendFn { query: true }))?;
    for m in [
        Maker::ToTsquery,
        Maker::Plain,
        Maker::Phrase,
        Maker::Websearch,
    ] {
        r.add_function(&[m.name()], Arc::new(MakeQuery(m)))?;
    }
    crate::tsrank::register(r)?;
    crate::tsheadline::register(r)?;
    let text = r.operator("@@", DataType::Text, DataType::Text).cloned();
    let (v, q) = (
        DataType::Ext(tsvector_type()),
        DataType::Ext(tsquery_type()),
    );
    for (operands, l, rt) in [
        (Operands::VectorQuery, v, q),
        (Operands::QueryVector, q, v),
        (Operands::TextQuery, DataType::Text, q),
    ] {
        r.add_operator(
            "@@",
            l,
            rt,
            Arc::new(Match {
                operands,
                text: text.clone(),
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple() -> Arc<dyn Analyzer> {
        nt::register_builtins();
        config("simple").unwrap()
    }

    fn english() -> Arc<dyn Analyzer> {
        nt::register_builtins();
        config("english").unwrap()
    }

    fn q(text: &str) -> String {
        output(query_input(text).unwrap().as_ref())
    }

    #[test]
    fn query_text_forms_as_postgresql_prints_them() {
        for (i, o) in [
            ("a & b & c", "'a' & 'b' & 'c'"),
            ("a & (b | c)", "'a' & ( 'b' | 'c' )"),
            ("(a | b) & c", "( 'a' | 'b' ) & 'c'"),
            ("!(a & b)", "!( 'a' & 'b' )"),
            ("!!a", "!!'a'"),
            ("a <-> (b <-> c)", "'a' <-> ( 'b' <-> 'c' )"),
            ("(a <-> b) <-> c", "'a' <-> 'b' <-> 'c'"),
            ("a <2> b", "'a' <2> 'b'"),
            ("a <0> b", "'a' <0> 'b'"),
            ("a & b <-> c", "'a' & 'b' <-> 'c'"),
            ("sup:*ab", "'sup':*AB"),
            ("'it''s' | x\\&y", "'it''s' | 'x&y'"),
            ("", ""),
        ] {
            assert_eq!(q(i), o, "{i}");
            assert_eq!(q(o), o, "{o}");
        }
        for bad in ["a b", "a &", "(a", "a)", "& a", ":a"] {
            assert!(query_input(bad).is_err(), "{bad}");
        }
    }

    /// PostgreSQL's binary forms both ways: what a value sends reads back
    /// as it; what `tsvectorrecv` / `tsqueryrecv` refuse is refused.
    #[test]
    fn binary_forms() {
        for t in ["", "a", "'a b':1,3A 'it''s' z:2B,7C", "x:1,2,3 y:16383D"] {
            let v = TsVector::input(t).unwrap();
            assert_eq!(vector_from(&vector_bytes(&v)).unwrap(), v, "{t}");
        }
        // Past PostgreSQL's limits: capped as it would have kept them.
        let v = TsVector::input("a:16383,16384,20000B").unwrap();
        assert_eq!(
            vector_from(&vector_bytes(&v)).unwrap(),
            TsVector::input("a:16383B").unwrap()
        );
        for t in [
            "",
            "a",
            "'a' & !( 'b' | 'c':*AB )",
            "( 'a' <-> 'b' ) <2> 'c' & 'd' <-> ( 'e' <-> 'f' )",
            "a <16384> b",
            "!!a",
        ] {
            let q = query_input(t).unwrap();
            assert_eq!(query_from(&query_bytes(q.as_ref())).unwrap(), q, "{t}");
        }
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        for bad in [
            "00000001",                   // a term missing
            "000000010000",               // an empty term
            "00000001610000020002000100", // positions misordered
            "0000000161000001000000",     // position 0
            "0000000161000001000100",     // trailing bytes
        ] {
            assert!(vector_from(&hex(bad)).is_err(), "{bad}");
        }
        for bad in [
            "000000010202",                         // an operator, no operands
            "000000010110006100",                   // weights past 0xF
            "0000000201000061000100006200",         // two operands, no operator
            "00000001020500",                       // no such operator
            "000000030204400101000061000100006200", // distance 16385
        ] {
            assert!(query_from(&hex(bad)).is_err(), "{bad}");
        }
        // As deep as the server walks, and no deeper.
        let deep = |n: usize| {
            let mut b = (n as u32 + 1).to_be_bytes().to_vec();
            for _ in 0..n {
                b.extend_from_slice(&[2, 1]);
            }
            b.extend_from_slice(&[1, 0, 0, b'a', 0]);
            b
        };
        assert!(query_from(&deep(MAX_DEPTH - 1)).is_ok());
        assert!(query_from(&deep(MAX_DEPTH)).is_err());
    }

    #[test]
    fn vector_text_forms() {
        let v = TsVector::input("b:3 a:2B,1 'it''s' a:2A").unwrap();
        assert_eq!(v.output(), "'a':1,2A 'b':3 'it''s'");
        assert_eq!(
            TsVector::analyzed(english().as_ref(), "The cats sat on the mat").output(),
            "'cat':2 'mat':6 'sat':3"
        );
    }

    #[test]
    fn stop_words_cleaned_as_postgresql_does() {
        let e = english();
        let mk = |m: Maker, t: &str| output(m.query(e.as_ref(), t).unwrap().as_ref());
        assert_eq!(mk(Maker::ToTsquery, "cats & the"), "'cat'");
        assert_eq!(
            mk(Maker::ToTsquery, "cat <-> the <-> mat"),
            "'cat' <2> 'mat'"
        );
        assert_eq!(
            mk(Maker::ToTsquery, "(cat <-> the) <-> mat"),
            "'cat' <2> 'mat'"
        );
        assert_eq!(mk(Maker::ToTsquery, "the & a"), "");
        assert_eq!(
            mk(Maker::Phrase, "the cat sat on the mat"),
            "'cat' <-> 'sat' <3> 'mat'"
        );
        assert_eq!(mk(Maker::Plain, "the cats sat"), "'cat' & 'sat'");
        assert_eq!(
            mk(Maker::Websearch, "\"cats sat\" -dogs or mice"),
            "'cat' <-> 'sat' & !'dog' | 'mice'"
        );
        assert_eq!(mk(Maker::ToTsquery, "Running:*"), "'run':*");
    }

    #[test]
    fn phrases_negations_and_prefixes_matched_as_postgresql_does() {
        let s = simple();
        let doc = TsVector::analyzed(s.as_ref(), "a b c a d");
        let yes = |t: &str| {
            matches(
                &doc,
                Maker::ToTsquery.query(s.as_ref(), t).unwrap().as_ref(),
            )
        };
        for (t, want) in [
            ("a <-> b", true),
            ("b <-> a", false),
            ("a <2> c", true),
            ("a <3> a", true),
            ("c <-> a <-> d", true),
            ("a <-> !b", true),
            ("b <-> !c", false),
            ("!a <-> b", false),
            ("(a | c) <-> d", true),
            ("(a & b) <-> c", false),
            ("x:*", false),
            ("a:*", true),
            ("!x", true),
            ("!a", false),
            ("a:A", false),
            ("a <-> (b <-> c)", true),
            ("a <-> (c <-> d)", false),
        ] {
            assert_eq!(yes(t), want, "{t}");
        }
        // PostgreSQL 13 on: exclusions match the empty vector.
        assert!(matches(
            &TsVector::default(),
            query_input("!x").unwrap().as_ref()
        ));
        assert!(!matches(&doc, None));
    }

    /// Found by fuzzing (PLAN.md §M14.29 S2): `or` was looked for by
    /// slicing two bytes, inside a character that starts with more.
    #[test]
    fn websearch_takes_any_text_after_a_word() {
        for text in [
            "\u{6771}\n\u{4eac}\u{306f}\u{65e5}f",
            "a 京",
            "a oré b",
            "a or 京",
            "or",
        ] {
            let _ = Parser::parse(text, Syntax::Websearch).unwrap();
        }
    }

    #[test]
    fn text_queries_say_what_can_be_said() {
        let e = english();
        let tq = |m: Maker, t: &str| text_query(&m.written(e.as_ref(), t).unwrap()?, e.as_ref());
        assert_eq!(
            tq(Maker::ToTsquery, "cats & (dogs | !mice)").as_deref(),
            Some("(\"cats\" AND (\"dogs\" OR (NOT \"mice\")))")
        );
        assert_eq!(
            tq(Maker::ToTsquery, "cat <-> the <-> mat").as_deref(),
            Some("\"cat the mat\"")
        );
        assert_eq!(tq(Maker::ToTsquery, "running:*").as_deref(), Some("run*"));
        assert_eq!(tq(Maker::ToTsquery, "cat <2> mat"), None);
        assert_eq!(tq(Maker::ToTsquery, "cat:A"), None);
        assert_eq!(tq(Maker::ToTsquery, "(a | b) <-> c"), None);
        assert_eq!(
            tq(Maker::Phrase, "the \"cat\" sat").as_deref(),
            Some("\"the  cat  sat\"")
        );
    }
}
