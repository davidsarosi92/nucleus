//! PostgreSQL's `ts_rank` and `ts_rank_cd` (PLAN.md §M14.21 T8b), as its
//! `tsrank.c` computes them — in `real` where it does, in `double`
//! where C's promotions put it, so the values are PostgreSQL's to the
//! bit.
//!
//! - `ts_rank([weights,] vector, query [, normalization])`: a query
//!   whose root is `&` or a phrase by its terms' proximity (each pair of
//!   positions of two different terms weighing `sqrt(w1 · w2 ·
//!   word_distance(d))`, combined as `1 − Π(1 − x)`), any other by each
//!   term's occurrences (weights over `1 / i²`).
//! - `ts_rank_cd(…)`: cover density — the shortest stretches of the text
//!   satisfying the query (`Cover`, evaluated by `TS_execute` on the
//!   positions inside), each weighing its terms' count over the sum of
//!   their inverse weights, over one plus the other words in it.
//!
//! Weights `{D, C, B, A}`, by default `{0.1, 0.2, 0.4, 1.0}`; the
//! normalization bits 1 (log length), 2 (length), 4 (mean distance of
//! covers, `ts_rank_cd` only), 8 (unique terms), 16 (log unique terms),
//! 32 (`rank / (rank + 1)`).

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use nucleus_sql::ext::{
    Arg, ArgInfo, CallContext, DataType, Error, Registry, Result, Returns, ScalarFunction,
    Signature, Value, Volatility,
};

use crate::tsearch::{
    Phr, Q, Source, Tri, TsVector, execute, query_of, tsquery_type, tsvector_type, vector_of,
};

const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];
/// PostgreSQL's greatest position (`MAXENTRYPOS - 1` stands in for a term
/// without positions in `ts_rank`'s proximity).
const MAXENTRYPOS: u32 = (1 << 14) - 1;

const NORM_LOGLENGTH: i64 = 0x01;
const NORM_LENGTH: i64 = 0x02;
const NORM_EXTDIST: i64 = 0x04;
const NORM_UNIQ: i64 = 0x08;
const NORM_LOGUNIQ: i64 = 0x10;
const NORM_RDIVRPLUS1: i64 = 0x20;

/// `word_distance`: the weight of two terms `w` positions apart.
fn word_distance(w: i32) -> f32 {
    if w > 100 {
        return 1e-30;
    }
    (1.0 / (1.005 + 0.05 * (f64::from(w as f32) / 1.5 - 2.0).exp())) as f32
}

/// `cnt_length`: the positions, a term without any counting one.
fn cnt_length(t: &TsVector) -> usize {
    t.0.iter().map(|(_, ps)| ps.len().max(1)).sum()
}

/// The query's operands in PostgreSQL's array order: an operator, then
/// its right operand, then its left.
fn operands<'q>(q: &'q Q, out: &mut Vec<&'q Q>) {
    match q {
        Q::Lex { .. } => out.push(q),
        Q::Stop => {}
        Q::Not(x) => operands(x, out),
        Q::And(l, r) | Q::Or(l, r) | Q::Phrase(l, r, _) => {
            operands(r, out);
            operands(l, out);
        }
    }
}

fn word(q: &Q) -> &str {
    match q {
        Q::Lex { word, .. } => word,
        _ => "",
    }
}

/// `SortAndUniqItems`: the operands by their words, each word once — the
/// first of equals after PostgreSQL's `qsort` (which of `a` and `a:*`
/// stays depends on it: its sort is not stable past six items).
fn sort_and_uniq(q: &Q) -> Vec<&Q> {
    let mut items = Vec::new();
    operands(q, &mut items);
    pg_qsort(&mut items, &|a: &&Q, b: &&Q| {
        word(a).as_bytes().cmp(word(b).as_bytes())
    });
    items.dedup_by(|b, a| word(a) == word(b));
    items
}

/// PostgreSQL's `qsort` (`sort_template.h`: Bentley and McIlroy's, an
/// insertion sort below seven items, a check for sorted input first), so
/// that equal items end where they end in PostgreSQL.
fn pg_qsort<T>(v: &mut [T], cmp: &dyn Fn(&T, &T) -> std::cmp::Ordering) {
    use std::cmp::Ordering;
    fn sign(o: Ordering) -> i32 {
        match o {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }
    fn sort<T>(v: &mut [T], mut a: usize, mut n: usize, cmp: &dyn Fn(&T, &T) -> Ordering) {
        let c = |v: &[T], i: usize, j: usize| sign(cmp(&v[i], &v[j]));
        let med3 = |v: &[T], x: usize, y: usize, z: usize| {
            if c(v, x, y) < 0 {
                if c(v, y, z) < 0 {
                    y
                } else if c(v, x, z) < 0 {
                    z
                } else {
                    x
                }
            } else if c(v, y, z) > 0 {
                y
            } else if c(v, x, z) < 0 {
                x
            } else {
                z
            }
        };
        let swapn = |v: &mut [T], x: usize, y: usize, k: usize| {
            for i in 0..k {
                v.swap(x + i, y + i);
            }
        };
        loop {
            if n < 7 {
                for pm in a + 1..a + n {
                    let mut pl = pm;
                    while pl > a && c(v, pl - 1, pl) > 0 {
                        v.swap(pl, pl - 1);
                        pl -= 1;
                    }
                }
                return;
            }
            if (a + 1..a + n).all(|pm| c(v, pm - 1, pm) <= 0) {
                return;
            }
            let mut pm = a + n / 2;
            if n > 7 {
                let mut pl = a;
                let mut pn = a + n - 1;
                if n > 40 {
                    let d = n / 8;
                    pl = med3(v, pl, pl + d, pl + 2 * d);
                    pm = med3(v, pm - d, pm, pm + d);
                    pn = med3(v, pn - 2 * d, pn - d, pn);
                }
                pm = med3(v, pl, pm, pn);
            }
            v.swap(a, pm);
            let (mut pa, mut pb) = (a + 1, a + 1);
            let (mut pc, mut pd) = ((a + n - 1) as isize, (a + n - 1) as isize);
            loop {
                while pb as isize <= pc {
                    let r = c(v, pb, a);
                    if r > 0 {
                        break;
                    }
                    if r == 0 {
                        v.swap(pa, pb);
                        pa += 1;
                    }
                    pb += 1;
                }
                while pb as isize <= pc {
                    let r = c(v, pc as usize, a);
                    if r < 0 {
                        break;
                    }
                    if r == 0 {
                        v.swap(pc as usize, pd as usize);
                        pd -= 1;
                    }
                    pc -= 1;
                }
                if pb as isize > pc {
                    break;
                }
                v.swap(pb, pc as usize);
                pb += 1;
                pc -= 1;
            }
            let pn = a + n;
            let (pc, pd) = (pc as usize, pd as usize);
            let d1 = (pa - a).min(pb - pa);
            swapn(v, a, pb - d1, d1);
            let d1 = (pd - pc).min(pn - pd - 1);
            swapn(v, pb, pn - d1, d1);
            let d1 = pb - pa;
            let d2 = pd - pc;
            if d1 <= d2 {
                if d1 > 1 {
                    sort(v, a, d1, cmp);
                }
                if d2 > 1 {
                    a = pn - d2;
                    n = d2;
                    continue;
                }
            } else {
                if d2 > 1 {
                    sort(v, pn - d2, d2, cmp);
                }
                if d1 > 1 {
                    n = d1;
                    continue;
                }
            }
            return;
        }
    }
    let n = v.len();
    sort(v, 0, n, cmp);
}

/// The tsvector entries an operand names (`find_wordentry`).
fn entries<'v>(t: &'v TsVector, op: &Q) -> &'v [(String, Vec<(u32, u8)>)] {
    match op {
        Q::Lex { word, prefix, .. } => t.entries(word, *prefix),
        _ => &[],
    }
}

/// `calc_rank_and`.
fn rank_and(w: &[f32; 4], t: &TsVector, q: &Q) -> f32 {
    let items = sort_and_uniq(q);
    if items.len() < 2 {
        return rank_or(w, t, q);
    }
    const POSNULL: &[(u32, u8)] = &[(MAXENTRYPOS - 1, 0)];
    let mut pos: Vec<Option<&[(u32, u8)]>> = vec![None; items.len()];
    let mut res: f32 = -1.0;
    for i in 0..items.len() {
        for (_, ps) in entries(t, items[i]) {
            let post: &[(u32, u8)] = if ps.is_empty() { POSNULL } else { ps };
            pos[i] = Some(post);
            let null_i = ps.is_empty();
            for ct in pos[..i].iter().flatten() {
                let ct = *ct;
                let null_k = std::ptr::eq(ct, POSNULL);
                for &(pl, wl) in post {
                    for &(pp, wp) in ct {
                        let mut dist = (pl as i32 - pp as i32).abs();
                        if dist != 0 || null_i || null_k {
                            if dist == 0 {
                                dist = MAXENTRYPOS as i32;
                            }
                            let x = w[usize::from(wl)] * w[usize::from(wp)] * word_distance(dist);
                            let curw = f64::from(x).sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                (1.0 - (1.0 - f64::from(res)) * (1.0 - f64::from(curw))) as f32
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

/// `calc_rank_or`.
fn rank_or(w: &[f32; 4], t: &TsVector, q: &Q) -> f32 {
    let items = sort_and_uniq(q);
    const POSNULL: &[(u32, u8)] = &[(0, 0)];
    let mut res: f32 = 0.0;
    for item in &items {
        for (_, ps) in entries(t, item) {
            let post: &[(u32, u8)] = if ps.is_empty() { POSNULL } else { ps };
            let (mut resj, mut wjm, mut jm): (f32, f32, usize) = (0.0, -1.0, 0);
            for (j, &(_, wt)) in post.iter().enumerate() {
                let wj = w[usize::from(wt)];
                resj += wj / ((j + 1) * (j + 1)) as f32;
                if wj > wjm {
                    wjm = wj;
                    jm = j;
                }
            }
            let x = wjm + resj - wjm / ((jm + 1) * (jm + 1)) as f32;
            res = (f64::from(res) + f64::from(x) / 1.64493406685) as f32;
        }
    }
    if !items.is_empty() {
        res /= items.len() as f32;
    }
    res
}

/// `calc_rank`.
fn rank(w: &[f32; 4], t: &TsVector, q: Option<&Q>, method: i64) -> f32 {
    let Some(q) = q else { return 0.0 };
    if t.0.is_empty() {
        return 0.0;
    }
    let mut res = match q {
        Q::And(..) | Q::Phrase(..) => rank_and(w, t, q),
        _ => rank_or(w, t, q),
    };
    if res < 0.0 {
        res = 1e-20;
    }
    let size = t.0.len();
    if method & NORM_LOGLENGTH != 0 {
        res = (f64::from(res) / ((cnt_length(t) as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(t);
        if len > 0 {
            res /= len as f32;
        }
    }
    if method & NORM_UNIQ != 0 {
        res /= size as f32;
    }
    if method & NORM_LOGUNIQ != 0 {
        res = (f64::from(res) / ((size as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        res /= res + 1.0;
    }
    res
}

// ---------------------------------------------------------------------------
// Cover density.

/// A position of the text and the operands found there (`DocRepresentation`).
struct DocRep<'q> {
    pos: u32,
    weight: u8,
    items: Vec<&'q Q>,
}

/// `get_docrep`: every position of the query's operands' terms, in order,
/// the operands at each together.
fn docrep<'q>(t: &TsVector, q: &'q Q) -> Vec<DocRep<'q>> {
    let mut ops = Vec::new();
    operands(q, &mut ops);
    // (pos, weight, entry, operand)
    let mut raw: Vec<(u32, u8, usize, &'q Q)> = Vec::new();
    for op in ops {
        let Q::Lex {
            word,
            prefix,
            weights,
        } = op
        else {
            continue;
        };
        let from = t.0.partition_point(|(w, _)| w.as_str() < word.as_str());
        let es = t.entries(word, *prefix);
        for (k, (_, ps)) in es.iter().enumerate() {
            for &(p, wt) in ps {
                if *weights == 0 || weights & (1 << wt) != 0 {
                    raw.push((p, wt, from + k, op));
                }
            }
        }
    }
    raw.sort_by_key(|a| (a.0, a.1, a.2));
    let mut out: Vec<DocRep<'q>> = Vec::new();
    let mut last: Option<(u32, u8, usize)> = None;
    for (p, wt, e, op) in raw {
        if last == Some((p, wt, e)) {
            out.last_mut().expect("a group").items.push(op);
        } else {
            out.push(DocRep {
                pos: p,
                weight: wt,
                items: vec![op],
            });
            last = Some((p, wt, e));
        }
    }
    out
}

/// The positions in view for each operand (`QueryRepresentation`).
#[derive(Default)]
struct Window {
    found: HashMap<*const Q, Vec<u32>>,
    reverse: bool,
}

impl Window {
    fn reset(&mut self, reverse: bool) {
        self.found.clear();
        self.reverse = reverse;
    }

    /// `fillQueryRepresentationData`.
    fn fill(&mut self, d: &DocRep<'_>) {
        for op in &d.items {
            let ps = self.found.entry(std::ptr::from_ref(*op)).or_default();
            if self.reverse {
                if ps.first() != Some(&d.pos) {
                    ps.insert(0, d.pos);
                }
            } else if ps.last() != Some(&d.pos) {
                ps.push(d.pos);
            }
        }
    }
}

impl Source for Window {
    fn check(&self, op: &Q, data: Option<&mut Phr>) -> Tri {
        let Some(ps) = self.found.get(&std::ptr::from_ref(op)) else {
            return Tri::No;
        };
        if let Some(d) = data {
            d.pos = ps.clone();
        }
        Tri::Yes
    }
}

/// `Cover`'s state (`CoverExt`).
struct Ext {
    pos: usize,
    p: u32,
    q: u32,
    begin: usize,
    end: usize,
}

/// `Cover`: the next shortest stretch from `ext.pos` satisfying the query.
fn cover(doc: &[DocRep<'_>], q: &Q, w: &mut Window, ext: &mut Ext) -> bool {
    loop {
        let mut lastpos = ext.pos;
        let mut found = false;
        w.reset(false);
        ext.p = u32::MAX;
        ext.q = 0;
        let mut i = ext.pos;
        while i < doc.len() {
            w.fill(&doc[i]);
            if execute(q, w) != Tri::No {
                if doc[i].pos > ext.q {
                    ext.q = doc[i].pos;
                    ext.end = i;
                    lastpos = i;
                    found = true;
                }
                break;
            }
            i += 1;
        }
        if !found {
            return false;
        }
        w.reset(true);
        let mut i = lastpos as isize;
        while i >= ext.pos as isize {
            let d = &doc[i as usize];
            w.fill(d);
            if execute(q, w) != Tri::No {
                if d.pos < ext.p {
                    ext.begin = i as usize;
                    ext.p = d.pos;
                }
                break;
            }
            i -= 1;
        }
        if ext.p <= ext.q {
            ext.pos = (i + 1) as usize;
            return true;
        }
        ext.pos += 1;
    }
}

/// `calc_rank_cd`.
fn rank_cd(weights: &[f32; 4], t: &TsVector, q: Option<&Q>, method: i64) -> f32 {
    let Some(q) = q else { return 0.0 };
    let invws: Vec<f64> = weights.iter().map(|&x| 1.0 / f64::from(x)).collect();
    let doc = docrep(t, q);
    if doc.is_empty() {
        return 0.0;
    }
    let mut w = Window::default();
    let mut ext = Ext {
        pos: 0,
        p: 0,
        q: 0,
        begin: 0,
        end: 0,
    };
    let (mut wdoc, mut sum_dist, mut prev_ext_pos) = (0.0f64, 0.0f64, 0.0f64);
    let mut n_extent = 0;
    while cover(&doc, q, &mut w, &mut ext) {
        let inv_sum: f64 = doc[ext.begin..=ext.end]
            .iter()
            .map(|d| invws[usize::from(d.weight)])
            .sum();
        let cpos = (ext.end - ext.begin + 1) as f64 / inv_sum;
        let mut n_noise = (ext.q as i64 - ext.p as i64) - (ext.end as i64 - ext.begin as i64);
        if n_noise < 0 {
            n_noise = (ext.end as i64 - ext.begin as i64) / 2;
        }
        wdoc += cpos / (1 + n_noise) as f64;
        let cur_ext_pos = (f64::from(ext.q) + f64::from(ext.p)) / 2.0;
        if n_extent > 0 && cur_ext_pos > prev_ext_pos {
            sum_dist += 1.0 / (cur_ext_pos - prev_ext_pos);
        }
        prev_ext_pos = cur_ext_pos;
        n_extent += 1;
    }
    let size = t.0.len();
    if method & NORM_LOGLENGTH != 0 && size > 0 {
        wdoc /= (cnt_length(t) as f64 + 1.0).ln();
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(t);
        if len > 0 {
            wdoc /= len as f64;
        }
    }
    if method & NORM_EXTDIST != 0 && n_extent > 0 && sum_dist > 0.0 {
        wdoc /= f64::from(n_extent) / sum_dist;
    }
    if method & NORM_UNIQ != 0 && size > 0 {
        wdoc /= size as f64;
    }
    if method & NORM_LOGUNIQ != 0 && size > 0 {
        wdoc /= (size as f64 + 1.0).ln() / 2f64.ln();
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        wdoc /= wdoc + 1.0;
    }
    wdoc as f32
}

// ---------------------------------------------------------------------------

/// `getWeights`: four, none `NULL`, each at most 1, a negative one the
/// default.
fn weights_of(v: &Value) -> Result<[f32; 4]> {
    let Value::Array(xs) = v else {
        return Err(Error::internal("ts_rank's weights an array"));
    };
    if xs.len() < 4 {
        return Err(Error::invalid_parameter("array of weight is too short"));
    }
    let mut w = [0f32; 4];
    for (i, x) in xs.iter().take(4).enumerate() {
        let x = match x {
            Value::Real(x) => *x,
            Value::Float(x) => *x as f32,
            Value::Null => {
                return Err(Error::invalid_parameter(
                    "array of weight must not contain nulls",
                ));
            }
            _ => return Err(Error::internal("ts_rank's weights real")),
        };
        w[i] = if x >= 0.0 { x } else { DEFAULT_WEIGHTS[i] };
        if w[i] > 1.0 {
            return Err(Error::invalid_parameter("weight out of range"));
        }
    }
    Ok(w)
}

/// `ts_rank` or `ts_rank_cd`.
struct Rank {
    cover_density: bool,
}

impl ScalarFunction for Rank {
    fn name(&self) -> &str {
        if self.cover_density {
            "ts_rank_cd"
        } else {
            "ts_rank"
        }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=4
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let (v, q) = (
            DataType::Ext(tsvector_type()),
            DataType::Ext(tsquery_type()),
        );
        let w = DataType::Real.array_of().expect("real[]");
        let weighted = match args.len() {
            4 => true,
            3 => args[0].ty != Some(v),
            _ => false,
        };
        let mut sig = Vec::new();
        if weighted {
            sig.push(Arg::Convert(w));
        }
        sig.push(Arg::Untyped(v));
        sig.push(Arg::Untyped(q));
        if sig.len() < args.len() {
            sig.push(Arg::Convert(DataType::Integer));
        }
        Ok(Signature::new(sig, Returns::Type(Some(DataType::Real))))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let weighted = matches!(args[0], Value::Array(_));
        let (w, rest) = if weighted {
            (weights_of(&args[0])?, &args[1..])
        } else {
            (DEFAULT_WEIGHTS, args)
        };
        let t = vector_of(&rest[0])?;
        let q = query_of(&rest[1])?;
        let method = match rest.get(2) {
            Some(Value::Int(m)) => *m,
            _ => 0,
        };
        Ok(Value::Real(if self.cover_density {
            rank_cd(&w, &t, q.as_ref(), method)
        } else {
            rank(&w, &t, q.as_ref(), method)
        }))
    }
}

/// Registers `ts_rank` and `ts_rank_cd`.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    r.add_function(
        &["ts_rank"],
        Arc::new(Rank {
            cover_density: false,
        }),
    )?;
    r.add_function(
        &["ts_rank_cd"],
        Arc::new(Rank {
            cover_density: true,
        }),
    )
}
