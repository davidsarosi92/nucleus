//! Text queries (PLAN.md §M14.21 T2): `body @@ 'nucleus AND (database OR
//! engine)'`, parsed once into leaves (the terms looked for) and a tree
//! over them, then matched against a text's terms.  The syntax is
//! Lucene's, the subset of it a single column needs, so what people and
//! models already write works:
//!
//! - `word` — the word, analyzed as the text is; a word the analyzer
//!   makes several terms of (`e-mail`, any word under `trigram`) is a
//!   phrase of them, a stop word is dropped.
//! - `"several words"` — a phrase: the terms at consecutive positions
//!   (a dropped stop word keeps its place).
//! - `word*` — any term starting so; `word~`, `word~1` — any term within
//!   two (one) edits, a transposition one edit (Lucene's `FuzzyQuery`).
//!   Neither is stemmed, only lower-cased, as Lucene normalizes them.
//! - `a b` — either; `+a` — required (then the others only rank),
//!   `-a`, `NOT a` — excluded; `a AND b`, `a OR b`, `( … )`.  `NOT`
//!   binds tightest, then `AND`, then `OR`, then words side by side.
//!
//! Side by side, words are OR'ed, as in Lucene, Tantivy and Elasticsearch:
//! ranked search (T5) asks for any of them; `AND` and `+` ask for all.
//! A query with only exclusions matches the texts without them.

use std::sync::Arc;

use super::Analyzer;
use crate::error::{Error, Result};

/// A term a query looks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Leaf {
    /// The term itself.
    Exact(String),
    /// A term starting with this.
    Prefix(String),
    /// A term within this many edits.
    Fuzzy(String, u8),
}

impl Leaf {
    pub(crate) fn matches(&self, term: &str) -> bool {
        match self {
            Leaf::Exact(t) => t == term,
            Leaf::Prefix(p) => term.starts_with(p.as_str()),
            Leaf::Fuzzy(t, k) => within(t, term, *k as usize),
        }
    }
}

/// The tree over the leaves (indexes into [`Query::leaves`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    /// A leaf anywhere.
    Leaf(usize),
    /// Leaves at these positions relative to one another.
    Phrase(Vec<(usize, u32)>),
    /// All of `must`, none of `not`, and one of `should` unless `must`
    /// holds something (then `should` only ranks).
    Bool {
        must: Vec<Node>,
        should: Vec<Node>,
        not: Vec<Node>,
    },
}

/// A parsed query: its distinct leaves and the tree; no tree when nothing
/// is left to look for (stop words only), which matches nothing.
#[derive(Debug)]
pub(crate) struct Query {
    pub(crate) leaves: Vec<Leaf>,
    pub(crate) root: Option<Node>,
}

/// Where each leaf occurs in one text: positions, ascending.
pub(crate) struct Hits(Vec<Vec<u32>>);

impl Query {
    /// `q` parsed, its words analyzed by `a`.
    pub(crate) fn parse(q: &str, a: &dyn Analyzer) -> Result<Query> {
        let mut p = Parser {
            src: q,
            at: 0,
            a,
            leaves: Vec::new(),
            depth: 0,
        };
        let root = p.list()?;
        if p.at < q.len() {
            return Err(p.error("unexpected )"));
        }
        Ok(Query {
            leaves: p.leaves,
            root,
        })
    }

    /// Scratch for [`Query::matches`], reused text after text.
    pub(crate) fn hits(&self) -> Hits {
        Hits(vec![Vec::new(); self.leaves.len()])
    }

    /// Whether `text`, analyzed by `a`, matches.
    pub(crate) fn matches(&self, a: &dyn Analyzer, text: &str, hits: &mut Hits) -> bool {
        let Some(root) = &self.root else {
            return false;
        };
        self.find(a, text, hits);
        eval(root, &hits.0)
    }

    /// Where each leaf occurs in `text`, analyzed by `a`, into `hits`; the
    /// text's length in terms.
    pub(crate) fn find(&self, a: &dyn Analyzer, text: &str, hits: &mut Hits) -> u32 {
        for h in &mut hits.0 {
            h.clear();
        }
        let mut len = 0;
        a.analyze(text, &mut |t| {
            len += 1;
            for (leaf, h) in self.leaves.iter().zip(hits.0.iter_mut()) {
                if leaf.matches(&t.term) {
                    h.push(t.position);
                }
            }
        });
        len
    }
}

impl Hits {
    /// Each leaf's positions.
    pub(crate) fn of(&self) -> &[Vec<u32>] {
        &self.0
    }
}

/// Whether `n` holds where each leaf is at `hits[leaf]` (ascending).
pub(crate) fn eval(n: &Node, hits: &[Vec<u32>]) -> bool {
    match n {
        Node::Leaf(i) => !hits[*i].is_empty(),
        Node::Phrase(terms) => {
            let (first, base) = terms[0];
            hits[first].iter().any(|&p| {
                terms[1..].iter().all(|&(i, off)| {
                    let want = p as u64 + off as u64 - base as u64;
                    hits[i].binary_search(&(want as u32)).is_ok()
                })
            })
        }
        Node::Bool { must, should, not } => {
            must.iter().all(|n| eval(n, hits))
                && !not.iter().any(|n| eval(n, hits))
                && (!must.is_empty()
                    || should.is_empty() && !not.is_empty()
                    || should.iter().any(|n| eval(n, hits)))
        }
    }
}

/// Whether `a` and `b` are within `k` edits — insertions, deletions,
/// substitutions and transpositions of neighbours, over characters
/// (the optimal string alignment distance, as Lucene's automata count).
/// ASCII is compared as bytes, and rows of short terms stay on the stack:
/// a fuzzy term is checked against every term of every text.
pub(crate) fn within(a: &str, b: &str, k: usize) -> bool {
    if a == b {
        return true;
    }
    if k == 0 {
        return false;
    }
    if a.is_ascii() && b.is_ascii() {
        return osa(a.as_bytes(), b.as_bytes(), k);
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    osa(&a, &b, k)
}

fn osa<T: PartialEq>(a: &[T], b: &[T], k: usize) -> bool {
    if a.len().abs_diff(b.len()) > k {
        return false;
    }
    const STACK: usize = 48;
    let w = b.len() + 1;
    let mut stack = [[0usize; STACK]; 3];
    let mut heap;
    let [two, one, row]: [&mut [usize]; 3] = if w <= STACK {
        let [x, y, z] = &mut stack;
        [&mut x[..w], &mut y[..w], &mut z[..w]]
    } else {
        heap = vec![vec![0usize; w]; 3];
        let [x, y, z] = &mut heap[..] else {
            unreachable!()
        };
        [&mut x[..], &mut y[..], &mut z[..]]
    };
    let (mut two, mut one, mut row) = (two, one, row);
    for (j, c) in one.iter_mut().enumerate() {
        *c = j;
    }
    for i in 1..=a.len() {
        row[0] = i;
        let mut best = i;
        for j in 1..w {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (one[j] + 1).min(row[j - 1] + 1).min(one[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(two[j - 2] + 1);
            }
            row[j] = d;
            best = best.min(d);
        }
        if best > k {
            return false;
        }
        std::mem::swap(&mut two, &mut one);
        std::mem::swap(&mut one, &mut row);
    }
    one[b.len()] <= k
}

/// How a clause takes part in the list around it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Occur {
    Should,
    Must,
    Not,
}

struct Parser<'a> {
    src: &'a str,
    at: usize,
    a: &'a dyn Analyzer,
    leaves: Vec<Leaf>,
    depth: usize,
}

/// Characters that end a word.
fn special(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '"')
}

impl Parser<'_> {
    fn error(&self, what: &str) -> Error {
        Error::syntax(format!(
            "text query: {what} at character {}",
            self.src[..self.at].chars().count() + 1
        ))
    }

    fn skip_space(&mut self) {
        let rest = &self.src[self.at..];
        self.at += rest.len() - rest.trim_start().len();
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.src[self.at..].chars().next()
    }

    /// The keyword `kw` (upper case, a word of its own) next.
    fn keyword(&mut self, kw: &str) -> bool {
        self.skip_space();
        let rest = &self.src[self.at..];
        if rest.starts_with(kw) && rest[kw.len()..].chars().next().is_none_or(special) {
            self.at += kw.len();
            true
        } else {
            false
        }
    }

    fn leaf(&mut self, l: Leaf) -> usize {
        if let Some(i) = self.leaves.iter().position(|x| *x == l) {
            return i;
        }
        self.leaves.push(l);
        self.leaves.len() - 1
    }

    /// Clauses side by side, up to `)` or the end.
    fn list(&mut self) -> Result<Option<Node>> {
        let mut clauses = Vec::new();
        while !matches!(self.peek(), None | Some(')')) {
            if self.at_operator() {
                return Err(self.error("AND or OR without a term before it"));
            }
            clauses.push(self.or()?);
        }
        Ok(combine(clauses))
    }

    fn or(&mut self) -> Result<(Occur, Option<Node>)> {
        let mut items = vec![self.and()?];
        while self.keyword("OR") {
            items.push(self.and()?);
        }
        if items.len() == 1 {
            return Ok(items.pop().unwrap());
        }
        // `a OR -b`: either a, or not b.
        let should = items
            .into_iter()
            .filter_map(|(o, n)| {
                n.map(|n| match o {
                    Occur::Not => Node::Bool {
                        must: vec![],
                        should: vec![],
                        not: vec![n],
                    },
                    _ => n,
                })
            })
            .collect();
        Ok((Occur::Should, combine_should(should)))
    }

    fn and(&mut self) -> Result<(Occur, Option<Node>)> {
        let mut items = vec![self.unary()?];
        while self.keyword("AND") {
            items.push(self.unary()?);
        }
        if items.len() == 1 {
            return Ok(items.pop().unwrap());
        }
        let (mut must, mut not) = (vec![], vec![]);
        for (o, n) in items {
            match (o, n) {
                (_, None) => {}
                (Occur::Not, Some(n)) => not.push(n),
                (_, Some(n)) => must.push(n),
            }
        }
        let node = match (must.len(), not.len()) {
            (0, 0) => None,
            (1, 0) => must.pop(),
            _ => Some(Node::Bool {
                must,
                should: vec![],
                not,
            }),
        };
        Ok((Occur::Must, node))
    }

    /// An atom after any `+`, `-` and `NOT`s (an odd number of
    /// exclusions excludes it, a `+` otherwise requires it).
    fn unary(&mut self) -> Result<(Occur, Option<Node>)> {
        let (mut not, mut must) = (false, false);
        loop {
            if self.keyword("NOT") {
                not = !not;
                continue;
            }
            match self.peek() {
                Some(c @ ('+' | '-'))
                    if self.src[self.at + 1..]
                        .chars()
                        .next()
                        .is_some_and(|c| !c.is_whitespace() && c != ')') =>
                {
                    self.at += 1;
                    if c == '+' {
                        must = true;
                    } else {
                        not = !not;
                    }
                }
                _ => break,
            }
        }
        let occur = match (not, must) {
            (true, _) => Occur::Not,
            (false, true) => Occur::Must,
            (false, false) => Occur::Should,
        };
        Ok((occur, self.atom()?))
    }

    /// Whether `AND` or `OR` comes next (not consumed).
    fn at_operator(&mut self) -> bool {
        let at = self.at;
        let found = self.keyword("AND") || self.keyword("OR");
        self.at = at;
        found
    }

    fn atom(&mut self) -> Result<Option<Node>> {
        if self.at_operator() {
            return Err(self.error("a term expected"));
        }
        match self.peek() {
            None => Err(self.error("a term expected")),
            Some(')') => Err(self.error("a term expected")),
            Some('(') => {
                self.at += 1;
                self.depth += 1;
                if self.depth > 100 {
                    return Err(self.error("parentheses nested too deep"));
                }
                if self.peek() == Some(')') {
                    return Err(self.error("empty parentheses"));
                }
                let n = self.list()?;
                if self.peek() != Some(')') {
                    return Err(self.error("missing )"));
                }
                self.at += 1;
                self.depth -= 1;
                Ok(n)
            }
            Some('"') => {
                let start = self.at;
                let Some(len) = self.src[start + 1..].find('"') else {
                    return Err(self.error("unterminated phrase"));
                };
                self.at = start + 1 + len + 1;
                Ok(self.analyzed(&self.src[start + 1..start + 1 + len]))
            }
            Some(_) => {
                let rest = &self.src[self.at..];
                let len = rest.find(special).unwrap_or(rest.len());
                let word = &rest[..len];
                self.at += len;
                if let Some(head) = word.strip_suffix('*')
                    && !head.is_empty()
                {
                    let p = self.a.normalize(head);
                    return Ok((!p.is_empty()).then(|| Node::Leaf(self.leaf(Leaf::Prefix(p)))));
                }
                if let Some((head, edits)) = word.rsplit_once('~')
                    && !head.is_empty()
                {
                    let k = match edits {
                        "" => 2,
                        "0" | "1" | "2" => edits.parse().unwrap(),
                        _ => return Err(self.error("a fuzzy term takes ~, ~0, ~1 or ~2")),
                    };
                    let t = self.a.normalize(head);
                    return Ok((!t.is_empty()).then(|| Node::Leaf(self.leaf(Leaf::Fuzzy(t, k)))));
                }
                Ok(self.analyzed(word))
            }
        }
    }

    /// A word or a phrase: its terms, one a leaf, several a phrase.
    fn analyzed(&mut self, text: &str) -> Option<Node> {
        let terms = super::tokens(self.a, text);
        match terms.len() {
            0 => None,
            1 => Some(Node::Leaf(
                self.leaf(Leaf::Exact(terms.into_iter().next().unwrap().term)),
            )),
            _ => {
                let base = terms[0].position;
                Some(Node::Phrase(
                    terms
                        .into_iter()
                        .map(|t| (self.leaf(Leaf::Exact(t.term)), t.position - base))
                        .collect(),
                ))
            }
        }
    }
}

/// Clauses side by side as one node: Lucene's boolean clauses.
fn combine(clauses: Vec<(Occur, Option<Node>)>) -> Option<Node> {
    let (mut must, mut should, mut not) = (vec![], vec![], vec![]);
    for (o, n) in clauses {
        if let Some(n) = n {
            match o {
                Occur::Must => must.push(n),
                Occur::Should => should.push(n),
                Occur::Not => not.push(n),
            }
        }
    }
    match (must.len(), should.len(), not.len()) {
        (0, 0, 0) => None,
        (0, 1, 0) => should.pop(),
        (1, 0, 0) => must.pop(),
        _ => Some(Node::Bool { must, should, not }),
    }
}

fn combine_should(mut should: Vec<Node>) -> Option<Node> {
    match should.len() {
        0 => None,
        1 => should.pop(),
        _ => Some(Node::Bool {
            must: vec![],
            should,
            not: vec![],
        }),
    }
}

/// The last query parsed on this thread, by analyzer and text: a query
/// evaluated row by row is parsed once, not once a row.
pub(crate) fn cached(q: &str, a: &Arc<dyn Analyzer>) -> Result<Arc<Query>> {
    use std::cell::RefCell;
    thread_local! {
        static LAST: RefCell<Option<(String, String, Arc<Query>)>> = const { RefCell::new(None) };
    }
    LAST.with(|last| {
        if let Some((name, text, query)) = &*last.borrow()
            && name == a.name()
            && text == q
        {
            return Ok(query.clone());
        }
        let query = Arc::new(Query::parse(q, a.as_ref())?);
        *last.borrow_mut() = Some((a.name().to_string(), q.to_string(), query.clone()));
        Ok(query)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(analyzer: &str, q: &str, text: &str) -> bool {
        super::super::register_builtins();
        let a = super::super::named(analyzer).unwrap();
        let q = Query::parse(q, a.as_ref()).unwrap();
        let mut h = q.hits();
        q.matches(a.as_ref(), text, &mut h)
    }

    #[test]
    fn terms_and_operators() {
        let t = "The quick brown fox jumps over the lazy dog";
        assert!(m("simple", "fox", t));
        assert!(m("simple", "FOX", t));
        assert!(!m("simple", "cat", t));
        assert!(m("simple", "cat fox", t));
        assert!(!m("simple", "cat AND fox", t));
        assert!(m("simple", "fox AND dog", t));
        assert!(m("simple", "+fox cat", t));
        assert!(!m("simple", "+cat fox", t));
        assert!(!m("simple", "fox -dog", t));
        assert!(!m("simple", "fox NOT dog", t));
        assert!(m("simple", "fox AND NOT cat", t));
        assert!(m("simple", "-cat", t));
        assert!(!m("simple", "-fox", t));
        assert!(m("simple", "(cat OR dog) AND fox", t));
        assert!(!m("simple", "(cat OR bird) AND fox", t));
        assert!(m("simple", "cat OR -bird", t));
        // AND binds tighter than OR.
        assert!(m("simple", "cat AND bird OR fox", t));
        assert!(!m("simple", "cat AND (bird OR fox)", t));
    }

    #[test]
    fn phrases() {
        let t = "The quick brown fox jumps over the lazy dog";
        assert!(m("simple", "\"quick brown\"", t));
        assert!(!m("simple", "\"brown quick\"", t));
        assert!(m("simple", "\"lazy dog\" -cat", t));
        // A stop word keeps its place: `over the lazy` is not `over lazy`.
        assert!(m("porter", "\"jumps over the lazy\"", t));
        assert!(m("porter", "\"jumping over a lazy\"", t));
        assert!(!m("porter", "\"over lazy\"", t));
        // A word the analyzer splits is a phrase of its parts.
        assert!(m("simple", "e-mail", "send an e-mail now"));
        assert!(!m("simple", "e-mail", "mail an e now"));
        // Trigrams: a word is found as a substring.
        assert!(m("trigram", "atab", "the database"));
        assert!(!m("trigram", "atbb", "the database"));
    }

    #[test]
    fn prefixes_and_fuzzy_terms() {
        let t = "Databases and their engines";
        assert!(m("simple", "data*", t));
        assert!(m("simple", "DATA*", t));
        assert!(!m("simple", "date*", t));
        assert!(m("simple", "databsaes~1", t));
        assert!(m("simple", "databasse~", t));
        assert!(!m("simple", "databsaes~0", t));
        assert!(!m("simple", "dbases~1", t));
        assert!(m("simple", "dtbases~2", t));
        // Not stemmed: `engin*` finds `engines`'s stem `engin`.
        assert!(m("english", "engin*", t));
        assert!(m("english", "engines", t));
        assert!(!m("english", "engines*", t));
    }

    #[test]
    fn stop_words_drop_out() {
        assert!(m("porter", "the AND fox", "a fox"));
        assert!(!m("porter", "the", "the fox"));
        assert!(!m("porter", "", "the fox"));
        assert!(m("porter", "\"the fox\"", "a fox"));
    }

    #[test]
    fn plain_text_is_a_query() {
        // What a person or a model types: punctuation is a word break.
        let t = "What's the P/E ratio of C++ shops? It's 12.5, e.g. high.";
        assert!(m("simple", "what's the P/E ratio?", t));
        assert!(m("simple", "C++ e.g.", t));
        assert!(m("simple", "a - b ratio", t));
    }

    #[test]
    fn syntax_errors() {
        super::super::register_builtins();
        let a = super::super::named("simple").unwrap();
        for q in [
            "\"open",
            "(a b",
            "a b)",
            "a AND",
            "AND a",
            "a OR OR b",
            "x~3",
            "()",
        ] {
            assert!(Query::parse(q, a.as_ref()).is_err(), "{q}");
        }
        let deep = "(".repeat(200) + "a" + &")".repeat(200);
        assert!(Query::parse(&deep, a.as_ref()).is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(3000))]

        /// Any string is a query or a syntax error, never a panic; a
        /// query matches any text without one.
        #[test]
        fn any_query(
            q in proptest::string::string_regex(
                "([a-zé ()\"*~+\\-]|AND|OR|NOT|~[0-3]){0,30}"
            ).unwrap(),
            text in "[a-zé .-]{0,40}",
        ) {
            super::super::register_builtins();
            for name in ["simple", "porter", "trigram", "hungarian"] {
                let a = super::super::named(name).unwrap();
                if let Ok(q) = Query::parse(&q, a.as_ref()) {
                    let mut h = q.hits();
                    q.matches(a.as_ref(), &text, &mut h);
                }
            }
        }
    }

    #[test]
    fn edits() {
        assert!(within("abc", "abc", 0));
        assert!(within("abc", "acb", 1));
        assert!(within("abc", "ab", 1));
        assert!(within("abc", "xabc", 1));
        assert!(!within("abc", "cab", 1));
        assert!(within("abc", "cab", 2));
        assert!(within("ñandú", "nandu", 2));
        assert!(!within("ñandú", "nandu", 1));
        assert!(!within("a", "abcd", 2));
        // Longer than the rows kept on the stack.
        let long = "a".repeat(60);
        assert!(within(&long, &format!("{}b", &long[1..]), 1));
        assert!(!within(&long, &format!("{}bc", &long[2..]), 1));
    }
}
