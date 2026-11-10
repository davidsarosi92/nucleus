//! BM25 (PLAN.md §M14.21 T5): how well a text matches a query, given the
//! statistics of the rows searched — Lucene's `BM25Similarity` (since 8.0:
//! no `k1 + 1` factor; its IDF, never negative) over exact lengths, where
//! Lucene keeps a byte a length (Kamphuis et al. 2020: no difference).
//!
//! A query's score is its tree's (Lucene's `BooleanQuery`): a term's
//! BM25; a phrase's BM25 of its frequency with its terms' IDFs summed
//! (`PhraseQuery`); a prefix or fuzzy term's 1 where it matches (Lucene's
//! constant-score rewrite); a boolean node's, the sum of its required and
//! matching optional children's — exclusions add nothing.  A text the
//! query does not match scores 0.
//!
//! Scores are added in fixed point, in units of 2⁻³² ([`fixed`]): integer
//! sums, the same whatever order the parts are added in — the index's
//! MaxScore adds a text's terms in its own (§M14.21 T5b) and still gives
//! the number a row's text gives (as PISA's and JASS's quantized scores,
//! here far finer than a ranking needs: 2.3 × 10⁻¹⁰).

use std::ops::RangeInclusive;
use std::sync::Arc;

use super::query::{self, Leaf, Node, Query};
use crate::error::{Error, Result};
use crate::expr::{Expr, SubKind, SubPlan};
use crate::ext::{
    Arg, ArgInfo, CallContext, FnRef, Returns, ScalarFunction, Signature, Volatility,
};
use crate::plan::Plan;
use crate::types::{DataType, Value};

/// The statistics a score reads: documents, their total length, and each
/// leaf's documents (exact terms'; 0 for prefixes and fuzzy terms).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stats {
    pub docs: f64,
    pub len: f64,
    pub df: Vec<f64>,
}

/// A score's units a 1 holds.
const UNIT: f64 = 4_294_967_296.0;

/// `x` in a score's units: rounded, so every way of computing a part
/// gives the same integer.
pub(crate) fn fixed(x: f64) -> i64 {
    (x * UNIT).round() as i64
}

/// A score in units as a number.
pub(crate) fn of_fixed(x: i64) -> f64 {
    x as f64 / UNIT
}

/// BM25's parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Params {
    pub k1: f64,
    pub b: f64,
}

/// Lucene's, Elasticsearch's and Tantivy's.
pub(crate) const DEFAULT: Params = Params { k1: 1.2, b: 0.75 };

impl Stats {
    /// [`TextStats`]'s value read back.
    pub(crate) fn of(v: &Value) -> Result<Stats> {
        let Value::Array(st) = v else {
            return Err(Error::internal("statistics of an array"));
        };
        let st: Vec<f64> = st
            .iter()
            .map(|v| match v {
                Value::Float(f) => Ok(*f),
                _ => Err(Error::internal("statistics of floats")),
            })
            .collect::<Result<_>>()?;
        let [docs, len, df @ ..] = st.as_slice() else {
            return Err(Error::internal("statistics of documents and length"));
        };
        Ok(Stats {
            docs: *docs,
            len: *len,
            df: df.to_vec(),
        })
    }

    /// Lucene's IDF: ln(1 + (N − n + ½) / (n + ½)).
    fn idf(&self, df: f64) -> f64 {
        (1.0 + (self.docs - df + 0.5) / (df + 0.5)).ln()
    }
}

/// A text as a score reads it: its length in terms, and where each of the
/// query's leaves is in it.
pub(crate) trait Text {
    /// Its length in terms.
    fn len(&self) -> f64;
    /// How often leaf `l` occurs (0: not at all).
    fn freq(&self, l: usize) -> u32;
    /// Where leaf `l` is, ascending (asked of a phrase's leaves only).
    fn positions(&self, l: usize) -> &[u32];
}

/// A text analyzed: its length, and each leaf's positions.
pub(crate) struct Matched<'a> {
    pub(crate) len: f64,
    pub(crate) hits: &'a [Vec<u32>],
}

impl Text for Matched<'_> {
    fn len(&self) -> f64 {
        self.len
    }
    fn freq(&self, l: usize) -> u32 {
        self.hits[l].len() as u32
    }
    fn positions(&self, l: usize) -> &[u32] {
        &self.hits[l]
    }
}

/// The weights of a query's leaves and BM25's parameters, for a snapshot's
/// statistics: what scores every text of a statement.
pub(crate) struct Scorer<'q> {
    q: &'q Query,
    /// Each leaf's IDF (an exact term's; 0 for others).
    idf: Vec<f64>,
    avgdl: f64,
    p: Params,
}

impl<'q> Scorer<'q> {
    pub(crate) fn new(q: &'q Query, st: &Stats, p: Params) -> Self {
        let avgdl = if st.docs > 0.0 { st.len / st.docs } else { 1.0 };
        Scorer {
            q,
            idf: st.df.iter().map(|&df| st.idf(df)).collect(),
            avgdl: avgdl.max(f64::MIN_POSITIVE),
            p,
        }
    }

    /// The score of a text (0 where the query does not match it).
    pub(crate) fn score(&self, t: &dyn Text) -> f64 {
        let Some(root) = &self.q.root else { return 0.0 };
        of_fixed(self.node(root, t, self.norm(t.len())).unwrap_or(0))
    }

    /// BM25's length normalization of a text of `len` terms.
    pub(crate) fn norm(&self, len: f64) -> f64 {
        self.p.k1 * (1.0 - self.p.b + self.p.b * len / self.avgdl)
    }

    /// Leaf `l`'s score in a text where it occurs `f` times (`f` > 0),
    /// normalized by `norm`, in a score's units.
    pub(crate) fn leaf(&self, l: usize, f: u32, norm: f64) -> i64 {
        fixed(self.leaf_f64(l, f, norm))
    }

    /// [`Scorer::leaf`] before it is rounded to units: what bounds of it
    /// are computed from.
    pub(crate) fn leaf_f64(&self, l: usize, f: u32, norm: f64) -> f64 {
        match self.q.leaves[l] {
            Leaf::Exact(_) => {
                let f = f64::from(f);
                self.idf[l] * f / (f + norm)
            }
            Leaf::Prefix(_) | Leaf::Fuzzy(..) => 1.0,
        }
    }

    /// The leaves of a query that is one leaf or any of some (no phrase,
    /// nothing required or excluded) — a bag of words, its score the sum
    /// of theirs in this order — for a score summed leaf by leaf.
    pub(crate) fn flat(&self) -> Option<Vec<usize>> {
        match self.q.root.as_ref()? {
            Node::Leaf(l) => Some(vec![*l]),
            Node::Bool { must, should, not } if must.is_empty() && not.is_empty() => should
                .iter()
                .map(|c| match c {
                    Node::Leaf(l) => Some(*l),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }

    fn node(&self, n: &Node, t: &dyn Text, norm: f64) -> Option<i64> {
        let bm25 = |idf: f64, f: f64| idf * f / (f + norm);
        match n {
            Node::Leaf(i) => {
                let f = t.freq(*i);
                if f == 0 {
                    return None;
                }
                Some(self.leaf(*i, f, norm))
            }
            Node::Phrase(ts) => {
                let (first, base) = ts[0];
                let freq = t
                    .positions(first)
                    .iter()
                    .filter(|&&p| {
                        ts[1..].iter().all(|&(l, off)| {
                            let want = u64::from(p) + u64::from(off) - u64::from(base);
                            u32::try_from(want)
                                .is_ok_and(|w| t.positions(l).binary_search(&w).is_ok())
                        })
                    })
                    .count();
                if freq == 0 {
                    return None;
                }
                let idf: f64 = ts.iter().map(|&(l, _)| self.idf[l]).sum();
                Some(fixed(bm25(idf, freq as f64)))
            }
            Node::Bool { must, should, not } => {
                if not.iter().any(|c| self.node(c, t, norm).is_some()) {
                    return None;
                }
                let mut sum = 0i64;
                for c in must {
                    sum += self.node(c, t, norm)?;
                }
                let mut any = false;
                for c in should {
                    if let Some(s) = self.node(c, t, norm) {
                        sum += s;
                        any = true;
                    }
                }
                (any || !must.is_empty() || should.is_empty() && !not.is_empty()).then_some(sum)
            }
        }
    }
}

/// The score of a text whose leaves match at `m.hits`.
#[cfg(test)]
fn score(q: &Query, m: &Matched, st: &Stats, p: Params) -> f64 {
    Scorer::new(q, st, p).score(m)
}

/// `score(column @@ query)` where no text index computes it: refused when
/// the statement is bound — BM25 reads an index's statistics.
pub(crate) struct Score;

impl ScalarFunction for Score {
    fn name(&self) -> &str {
        "score"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Err(Error::feature(
            "score() takes a condition column @@ query on a column with a text index",
        ))
    }

    fn call(&self, _: &CallContext<'_>, _: &[Value]) -> Result<Value> {
        Err(Error::internal("score() bound by its index"))
    }
}

/// `score(body @@ q)` as a text index binds it: a [`TextScore`] of the row,
/// and the statistics of the index — a [`TextStats`] in a subquery of no
/// row, read once per statement.  The query must be the same for every
/// row.
pub(crate) fn bind(
    t: &crate::catalog::Table,
    i: usize,
    arg: &Expr,
    p: Params,
) -> Result<Option<(Expr, DataType)>> {
    let Expr::Call(f, args) = arg else {
        return Ok(None);
    };
    let [col, q, analyzer] = args.as_slice() else {
        return Ok(None);
    };
    if f.name() != "@@" {
        return Ok(None);
    }
    if !q.columns().is_empty() {
        return Err(Error::feature(
            "score(): the query must be the same for every row",
        ));
    }
    let text = |s: &str| Expr::Literal(Value::Text(s.into()));
    let stats = Expr::Subquery(Box::new(SubPlan {
        kind: SubKind::Scalar,
        plan: Plan::Project {
            input: Box::new(Plan::Values {
                rows: vec![vec![]],
                width: 0,
            }),
            exprs: vec![Expr::Call(
                FnRef::new(Arc::new(TextStats)),
                vec![text(&t.name), text(&t.indexes[i].name), q.clone()],
            )],
        },
        correlated: false,
    }));
    let score = Expr::Call(
        FnRef::new(Arc::new(TextScore)),
        vec![
            col.clone(),
            q.clone(),
            analyzer.clone(),
            Expr::Literal(Value::Float(p.k1)),
            Expr::Literal(Value::Float(p.b)),
            stats,
        ],
    );
    Ok(Some((score, DataType::Float)))
}

/// `text_stats(table, index, query)`: what BM25 reads of a text index, as
/// the statement's transaction sees it — `[documents, their total length,
/// each leaf's documents…]` (0 for a prefix's or a fuzzy term's).
struct TextStats;

impl ScalarFunction for TextStats {
    fn name(&self) -> &str {
        "text_stats"
    }

    fn arity(&self) -> RangeInclusive<usize> {
        3..=3
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Convert(DataType::Text); 3],
            Returns::Type(Some(DataType::Array(&DataType::Float))),
        ))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let [Value::Text(table), Value::Text(index), Value::Text(q)] = args else {
            return Err(Error::internal("text_stats of texts"));
        };
        let t = crate::catalog::get_table(cx.txn(), table)?.ok_or_else(|| {
            Error::undefined_table(format!("relation \"{table}\" does not exist"))
        })?;
        let i = t
            .indexes
            .iter()
            .position(|ix| &ix.name == index)
            .ok_or_else(|| Error::undefined_object(format!("index \"{index}\" does not exist")))?;
        let a = super::named(super::index::analyzer_of(&t.indexes[i]).unwrap_or(super::DEFAULT))?;
        let q = query::cached(q, &a)?;
        let terms: Vec<&str> = q
            .leaves
            .iter()
            .map(|l| match l {
                Leaf::Exact(t) => t.as_str(),
                Leaf::Prefix(_) | Leaf::Fuzzy(..) => "",
            })
            .collect();
        let (docs, len, df) = super::index::stats(cx.txn(), &t, i, &terms)?;
        let mut out = vec![Value::Float(docs as f64), Value::Float(len as f64)];
        out.extend(
            terms
                .iter()
                .zip(df)
                .map(|(t, n)| Value::Float(if t.is_empty() { 0.0 } else { n as f64 })),
        );
        Ok(Value::Array(out))
    }
}

/// The name [`TextScore`] goes by.
pub(crate) const TEXT_SCORE: &str = "text_score";

/// `text_score(text, query, analyzer, k1, b, stats)`: the text's BM25 for
/// the query, given [`TextStats`]'s statistics — 0 where it does not
/// match (a `NULL` text too).
pub(crate) struct TextScore;

impl ScalarFunction for TextScore {
    fn name(&self) -> &str {
        TEXT_SCORE
    }

    fn arity(&self) -> RangeInclusive<usize> {
        6..=6
    }

    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![
                Arg::Convert(DataType::Text),
                Arg::Convert(DataType::Text),
                Arg::Convert(DataType::Text),
                Arg::Convert(DataType::Float),
                Arg::Convert(DataType::Float),
                Arg::Convert(DataType::Array(&DataType::Float)),
            ],
            Returns::Type(Some(DataType::Float)),
        ))
    }

    fn strict(&self) -> bool {
        false
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let text = match &args[0] {
            Value::Text(t) => t,
            Value::Null => return Ok(Value::Float(0.0)),
            _ => return Err(Error::internal("text_score of a text")),
        };
        let (Value::Text(q), Value::Text(analyzer), Value::Float(k1), Value::Float(b)) =
            (&args[1], &args[2], &args[3], &args[4])
        else {
            return match args[1] {
                Value::Null => Ok(Value::Null),
                _ => Err(Error::internal("text_score's arguments")),
            };
        };
        let stats = Stats::of(&args[5])?;
        let a = super::named(analyzer)?;
        let q = query::cached(q, &a)?;
        if stats.df.len() != q.leaves.len() {
            return Err(Error::internal("statistics of every leaf"));
        }
        let mut hits = q.hits();
        let n = q.find(a.as_ref(), text, &mut hits);
        let m = Matched {
            len: f64::from(n),
            hits: hits.of(),
        };
        let s = Scorer::new(&q, &stats, Params { k1: *k1, b: *b });
        Ok(Value::Float(s.score(&m)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(docs: f64, len: f64, df: &[f64]) -> Stats {
        Stats {
            docs,
            len,
            df: df.to_vec(),
        }
    }

    #[test]
    fn lucenes_formula() {
        super::super::register_builtins();
        let a = super::super::named("simple").unwrap();
        let q = Query::parse("fox", a.as_ref()).unwrap();
        // N 10, n 2: idf ln(1 + 8.5 / 2.5); tf 2, dl 5, avgdl 10.
        let hits = vec![vec![1, 3]];
        let s = score(
            &q,
            &Matched {
                len: 5.0,
                hits: &hits,
            },
            &st(10.0, 100.0, &[2.0]),
            Params { k1: 1.2, b: 0.75 },
        );
        let idf = (1.0f64 + 8.5 / 2.5).ln();
        let want = idf * 2.0 / (2.0 + 1.2 * (1.0 - 0.75 + 0.75 * 0.5));
        assert!((s - want).abs() < 1e-9, "{s} {want}");
        // No match, nothing; an exclusion matched, nothing.
        let none = vec![vec![]];
        assert_eq!(
            score(
                &q,
                &Matched {
                    len: 5.0,
                    hits: &none
                },
                &st(10.0, 100.0, &[2.0]),
                DEFAULT
            ),
            0.0
        );
    }

    #[test]
    fn trees_add_up() {
        super::super::register_builtins();
        let a = super::super::named("simple").unwrap();
        let stats = st(100.0, 1000.0, &[10.0, 20.0, 0.0]);
        let one = |q: &str, hits: &[Vec<u32>]| {
            let q = Query::parse(q, a.as_ref()).unwrap();
            score(&q, &Matched { len: 10.0, hits }, &stats, DEFAULT)
        };
        let a1 = one("a", &[vec![0]]);
        let b1 = one("a b", &[vec![], vec![1]]);
        assert!(b1 > 0.0 && b1 < a1, "the rarer term weighs more");
        assert!((one("a b", &[vec![0], vec![1]]) - (a1 + b1)).abs() < 1e-9);
        assert!((one("a b", &[vec![0], vec![]]) - a1).abs() < 1e-9);
        assert!((one("+a b", &[vec![0], vec![1]]) - (a1 + b1)).abs() < 1e-9);
        assert_eq!(one("+a b", &[vec![], vec![1]]), 0.0);
        assert_eq!(one("a -b", &[vec![0], vec![1]]), 0.0);
        assert!((one("a -b", &[vec![0], vec![]]) - a1).abs() < 1e-9);
        // A phrase: its frequency, its terms' IDFs summed.
        let p = one("\"a b\"", &[vec![0, 5], vec![1, 9]]);
        let idf = |n: f64| (1.0 + (100.0 - n + 0.5) / (n + 0.5)).ln();
        let norm = 1.2 * (1.0 - 0.75 + 0.75 * 1.0);
        assert!((p - (idf(10.0) + idf(20.0)) / (1.0 + norm)).abs() < 1e-9);
        assert_eq!(one("\"a b\"", &[vec![0], vec![2]]), 0.0);
        // A prefix: 1 where it matches.
        assert_eq!(one("x*", &[vec![3]]), 1.0);
    }
}
