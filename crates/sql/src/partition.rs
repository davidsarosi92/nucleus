//! Declarative partitioning (PLAN.md §M14.20 V9): a table `PARTITION BY
//! RANGE | LIST | HASH (columns)` holds no rows of its own; its partitions
//! — tables of its columns, `PARTITION OF` it `FOR VALUES …` — do, each
//! stored as it chooses (`WITH (storage = …)`).
//!
//! A query of the partitioned table reads its partitions in turn (an
//! `Append`), those whose bounds its filters rule out left out — when the
//! plan is made, for constants, or when it runs, for parameters and outer
//! rows ([`guard`]).  A row written to it goes to the partition its key
//! falls in ([`route`]); one written to a partition directly must fall in
//! its bound.  Range partitions take `FROM (…) TO (…)` (lower bound
//! included, upper excluded, `MINVALUE` / `MAXVALUE` open), list partitions
//! `IN (…)`, hash partitions `WITH (MODULUS m, REMAINDER r)` (the key's
//! [`hash`] modulo `m`); a `DEFAULT` partition takes a range or list key no
//! other does.

use std::cmp::Ordering;

use crate::ast::BinaryOp;
use crate::catalog::Table;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::types::Value;

/// How a table is partitioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// By ranges of the key.
    Range,
    /// By lists of values (one column).
    List,
    /// By the key's hash.
    Hash,
}

/// A partitioned table's partitioning.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// The kind.
    pub kind: Kind,
    /// The key columns, by position.
    pub columns: Vec<usize>,
}

/// A partition's bound.
#[derive(Clone, Debug, PartialEq)]
pub enum PartBound {
    /// `FROM (…) TO (…)`: per key column, `None` for `MINVALUE` (from) or
    /// `MAXVALUE` (to).
    Range {
        /// The least key (included).
        from: Vec<Option<Value>>,
        /// The key it ends before.
        to: Vec<Option<Value>>,
    },
    /// `IN (…)`.
    List(Vec<Value>),
    /// `WITH (MODULUS m, REMAINDER r)`.
    Hash {
        /// m.
        modulus: u32,
        /// r.
        remainder: u32,
    },
    /// `DEFAULT`.
    Default,
}

impl Spec {
    /// Encodes it (a table's record).
    pub fn encode(&self, w: &mut Writer) {
        w.u8(match self.kind {
            Kind::Range => 0,
            Kind::List => 1,
            Kind::Hash => 2,
        })
        .u32(self.columns.len() as u32);
        for &c in &self.columns {
            w.u32(c as u32);
        }
    }

    /// Decodes one [`encode`](Self::encode)d.
    pub fn decode(r: &mut Reader) -> Result<Self> {
        let kind = match r.u8()? {
            0 => Kind::Range,
            1 => Kind::List,
            2 => Kind::Hash,
            _ => return Err(Error::internal("an unknown partitioning")),
        };
        let n = r.u32()? as usize;
        let columns = (0..n)
            .map(|_| r.u32().map(|c| c as usize))
            .collect::<Result<_>>()?;
        Ok(Spec { kind, columns })
    }
}

impl PartBound {
    /// Encodes it (a table's record).
    pub fn encode(&self, w: &mut Writer) {
        let opt = |w: &mut Writer, v: &Option<Value>| match v {
            None => {
                w.u8(0);
            }
            Some(v) => {
                w.u8(1).value(v);
            }
        };
        match self {
            PartBound::Range { from, to } => {
                w.u8(0).u32(from.len() as u32);
                for v in from.iter().chain(to) {
                    opt(w, v);
                }
            }
            PartBound::List(vals) => {
                w.u8(1).u32(vals.len() as u32);
                for v in vals {
                    w.value(v);
                }
            }
            PartBound::Hash { modulus, remainder } => {
                w.u8(2).u32(*modulus).u32(*remainder);
            }
            PartBound::Default => {
                w.u8(3);
            }
        }
    }

    /// Decodes one [`encode`](Self::encode)d.
    pub fn decode(r: &mut Reader) -> Result<Self> {
        let opt = |r: &mut Reader| -> Result<Option<Value>> {
            Ok(match r.u8()? {
                0 => None,
                _ => Some(r.value()?),
            })
        };
        Ok(match r.u8()? {
            0 => {
                let n = r.u32()? as usize;
                let from = (0..n).map(|_| opt(r)).collect::<Result<_>>()?;
                let to = (0..n).map(|_| opt(r)).collect::<Result<_>>()?;
                PartBound::Range { from, to }
            }
            1 => {
                let n = r.u32()? as usize;
                PartBound::List((0..n).map(|_| r.value()).collect::<Result<_>>()?)
            }
            2 => PartBound::Hash {
                modulus: r.u32()?,
                remainder: r.u32()?,
            },
            3 => PartBound::Default,
            _ => return Err(Error::internal("an unknown partition bound")),
        })
    }

    /// Its SQL (`FOR VALUES …`, `DEFAULT`), as `nc_catalog` shows it.
    pub fn render(&self) -> String {
        let bound = |vs: &[Option<Value>], open: &str| {
            vs.iter()
                .map(|v| v.as_ref().map_or(open.to_string(), Value::render_sql))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            PartBound::Range { from, to } => format!(
                "FOR VALUES FROM ({}) TO ({})",
                bound(from, "MINVALUE"),
                bound(to, "MAXVALUE")
            ),
            PartBound::List(vals) => format!(
                "FOR VALUES IN ({})",
                vals.iter()
                    .map(Value::render_sql)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PartBound::Hash { modulus, remainder } => {
                format!("FOR VALUES WITH (MODULUS {modulus}, REMAINDER {remainder})")
            }
            PartBound::Default => "DEFAULT".into(),
        }
    }
}

/// A key's hash, for hash partitioning: of its values' key encoding, so a
/// value hashes the same in every build.
pub fn hash(key: &[Value]) -> u64 {
    let bytes = crate::encoding::encode_key(key);
    let mut h = 0x6e75_636c_6575_7332u64;
    for c in bytes.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = crate::kernels::mix(h ^ u64::from_le_bytes(w)).wrapping_add(c.len() as u64);
    }
    crate::kernels::mix(h)
}

/// `a` against `b`, key column by key column (`None`: the open end, below
/// every value when `low`, above when not).
fn cmp_bound(a: &[Value], b: &[Option<Value>], low: bool) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = match y {
            None if low => Ordering::Greater,
            None => Ordering::Less,
            Some(y) => x.sort_cmp(y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Whether `key` falls in `bound` (a default never matches here: it takes
/// what no other does).
pub fn contains(bound: &PartBound, key: &[Value]) -> bool {
    match bound {
        PartBound::Range { from, to } => {
            // A NULL key falls in no range (as PostgreSQL).
            !key.iter().any(Value::is_null)
                && cmp_bound(key, from, true) != Ordering::Less
                && cmp_bound(key, to, false) == Ordering::Less
        }
        PartBound::List(vals) => vals.iter().any(|v| {
            (v.is_null() && key[0].is_null())
                || (!key[0].is_null() && v.sort_cmp(&key[0]) == Ordering::Equal)
        }),
        PartBound::Hash { modulus, remainder } => {
            hash(key) % u64::from(*modulus) == u64::from(*remainder)
        }
        PartBound::Default => false,
    }
}

/// The partition of `partitions` (of a table partitioned by `spec`) a row
/// goes to.
pub fn route<'a>(spec: &Spec, partitions: &'a [Table], row: &[Value]) -> Result<&'a Table> {
    let key: Vec<Value> = spec.columns.iter().map(|&c| row[c].clone()).collect();
    fn bound(t: &Table) -> Option<&PartBound> {
        t.partition_of.as_ref().map(|(_, b)| b)
    }
    partitions
        .iter()
        .find(|t| bound(t).is_some_and(|b| contains(b, &key)))
        .or_else(|| {
            partitions
                .iter()
                .find(|t| matches!(bound(t), Some(PartBound::Default)))
        })
        .ok_or_else(|| {
            Error::check_violation(format!(
                "no partition of relation found for row (key {})",
                key.iter().map(Value::render).collect::<Vec<_>>().join(", ")
            ))
        })
}

/// Whether `bound` and `other` (of one partitioning) can hold the same key.
pub fn overlaps(spec: &Spec, bound: &PartBound, other: &PartBound) -> bool {
    match (bound, other) {
        (PartBound::Default, PartBound::Default) => true,
        (PartBound::Default, _) | (_, PartBound::Default) => false,
        (PartBound::Range { from, to }, PartBound::Range { from: f2, to: t2 }) => {
            // [from, to) and [f2, t2) meet unless one ends before the other
            // starts.
            let before = |end: &[Option<Value>], start: &[Option<Value>]| {
                // end <= start, column by column with open ends.
                for (e, s) in end.iter().zip(start) {
                    let o = match (e, s) {
                        (None, _) => Ordering::Greater, // MAXVALUE
                        (_, None) => Ordering::Greater, // start MINVALUE
                        (Some(e), Some(s)) => e.sort_cmp(s),
                    };
                    if o != Ordering::Equal {
                        return o == Ordering::Less;
                    }
                }
                true
            };
            !(before(to, f2) || before(t2, from))
        }
        (PartBound::List(a), PartBound::List(b)) => a
            .iter()
            .any(|x| b.iter().any(|y| x.sort_cmp(y) == Ordering::Equal)),
        (
            PartBound::Hash {
                modulus: m1,
                remainder: r1,
            },
            PartBound::Hash {
                modulus: m2,
                remainder: r2,
            },
        ) => {
            // Moduli must divide one another (as PostgreSQL); then they
            // meet when the remainders agree modulo the smaller.
            let (small, big) = if m1 <= m2 { (*m1, *m2) } else { (*m2, *m1) };
            big % small != 0 || r1 % small == r2 % small
        }
        _ => {
            let _ = spec;
            true // kinds differ: refused elsewhere
        }
    }
}

/// Of `preds` (over the table's row), what a partition with `bound` must
/// satisfy to hold a matching row, as an expression over no row: `None`
/// when nothing narrows it.  Conservative: true whenever unsure.
pub fn guard(spec: &Spec, bound: &PartBound, preds: &[Expr]) -> Option<Expr> {
    let mut out: Vec<Expr> = Vec::new();
    let free = |e: &Expr| e.columns().is_empty() && !e.has_subquery() && !volatile(e);
    let lit = |v: Option<&Value>| v.map(|v| Expr::Literal(v.clone()));
    let cmp = |op, a: Expr, b: Expr| Expr::Binary(op, Box::new(a), Box::new(b));
    for p in preds {
        let Expr::Binary(op, l, r) = p else {
            continue;
        };
        let (col, x, op) = match (&**l, &**r) {
            (Expr::Column(c), x) if free(x) => (*c, x.clone(), *op),
            (x, Expr::Column(c)) if free(x) => (*c, x.clone(), flip(*op)),
            _ => continue,
        };
        // Only the first key column narrows a range (lexicographic order).
        match bound {
            PartBound::Range { from, to } if spec.columns.first() == Some(&col) => {
                let (lo, hi) = (lit(from[0].as_ref()), lit(to[0].as_ref()));
                // Some key in [lo, hi) relates to x by op.
                match op {
                    BinaryOp::Eq => {
                        if let Some(lo) = lo {
                            out.push(cmp(BinaryOp::GtEq, x.clone(), lo));
                        }
                        if let Some(hi) = hi {
                            // (A key of more columns may equal hi in its
                            // first and be below it in the rest.)
                            let op = if spec.columns.len() > 1 {
                                BinaryOp::LtEq
                            } else {
                                BinaryOp::Lt
                            };
                            out.push(cmp(op, x, hi));
                        }
                    }
                    BinaryOp::Lt | BinaryOp::LtEq => {
                        if let Some(lo) = lo {
                            let op = if op == BinaryOp::Lt {
                                BinaryOp::Gt
                            } else {
                                BinaryOp::GtEq
                            };
                            out.push(cmp(op, x, lo));
                        }
                    }
                    BinaryOp::Gt | BinaryOp::GtEq => {
                        if let Some(hi) = hi {
                            out.push(cmp(BinaryOp::Lt, x, hi));
                        }
                    }
                    _ => {}
                }
            }
            PartBound::List(vals) if spec.columns == [col] && op == BinaryOp::Eq => {
                out.push(Expr::InList(
                    Box::new(x),
                    vals.iter().map(|v| Expr::Literal(v.clone())).collect(),
                    false,
                ));
            }
            PartBound::Hash { modulus, remainder }
                if spec.columns == [col] && op == BinaryOp::Eq =>
            {
                out.push(cmp(
                    BinaryOp::Eq,
                    Expr::Call(
                        partition_hash_fn(),
                        vec![x, Expr::Literal(Value::Int(i64::from(*modulus)))],
                    ),
                    Expr::Literal(Value::Int(i64::from(*remainder))),
                ));
            }
            _ => {}
        }
    }
    crate::binder::conjunction(out)
}

fn volatile(e: &Expr) -> bool {
    let mut v = false;
    e.walk(&mut |x| {
        if let Expr::Call(f, _) = x
            && f.volatility() == crate::ext::Volatility::Volatile
        {
            v = true;
        }
    });
    v
}

fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        op => op,
    }
}

/// The partitions of `parent`, in bound order (ranges by their lower
/// bounds, the default last).
pub fn partitions(txn: &dyn nucleus_kv::KvTxn, parent: &Table) -> Result<Vec<Table>> {
    let mut out: Vec<Table> = crate::catalog::tables(txn)?
        .into_iter()
        .filter(|t| {
            t.partition_of
                .as_ref()
                .is_some_and(|(p, _)| *p == parent.name)
        })
        .collect();
    out.sort_by(|a, b| {
        let key = |t: &Table| match &t.partition_of {
            Some((_, PartBound::Default)) => (1, Vec::new(), t.name.clone()),
            Some((_, PartBound::Range { from, .. })) => (0, from.clone(), t.name.clone()),
            _ => (0, Vec::new(), t.name.clone()),
        };
        let (ka, kb) = (key(a), key(b));
        ka.0.cmp(&kb.0)
            .then_with(|| {
                for (x, y) in ka.1.iter().zip(&kb.1) {
                    let o = match (x, y) {
                        (None, None) => Ordering::Equal,
                        (None, _) => Ordering::Less,
                        (_, None) => Ordering::Greater,
                        (Some(x), Some(y)) => x.sort_cmp(y),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            })
            .then_with(|| ka.2.cmp(&kb.2))
    });
    Ok(out)
}

/// `partition_hash(key, modulus)`: the key's [`hash`] modulo `modulus` —
/// which hash partition (of that modulus) it falls in.
struct PartitionHash;

impl crate::ext::ScalarFunction for PartitionHash {
    fn name(&self) -> &str {
        "partition_hash"
    }

    fn arity(&self) -> std::ops::RangeInclusive<usize> {
        2..=2
    }

    fn resolve(&self, args: &[crate::ext::ArgInfo]) -> Result<crate::ext::Signature> {
        let _ = args;
        Ok(crate::ext::Signature::new(
            vec![
                crate::ext::Arg::Keep,
                crate::ext::Arg::Untyped(crate::types::DataType::BigInt),
            ],
            crate::ext::Returns::Type(Some(crate::types::DataType::BigInt)),
        ))
    }

    fn call(&self, _: &crate::ext::CallContext<'_>, args: &[Value]) -> Result<Value> {
        match &args[1] {
            Value::Int(m) if *m > 0 => Ok(Value::Int((hash(&args[..1]) % *m as u64) as i64)),
            _ => Err(Error::invalid_parameter(
                "partition_hash takes a positive modulus",
            )),
        }
    }
}

/// The `partition_hash` function.
pub fn partition_hash_arc() -> std::sync::Arc<dyn crate::ext::ScalarFunction> {
    static F: std::sync::OnceLock<std::sync::Arc<dyn crate::ext::ScalarFunction>> =
        std::sync::OnceLock::new();
    F.get_or_init(|| std::sync::Arc::new(PartitionHash)).clone()
}

/// [`partition_hash_arc`] as a plan holds it.
pub fn partition_hash_fn() -> crate::ext::FnRef {
    crate::ext::FnRef::new(partition_hash_arc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(a: Option<i64>, b: Option<i64>) -> PartBound {
        PartBound::Range {
            from: vec![a.map(Value::Int)],
            to: vec![b.map(Value::Int)],
        }
    }

    #[test]
    fn keys_fall_in_bounds_and_bounds_overlap_as_they_should() {
        let spec = Spec {
            kind: Kind::Range,
            columns: vec![0],
        };
        let b = range(Some(10), Some(20));
        assert!(contains(&b, &[Value::Int(10)]));
        assert!(!contains(&b, &[Value::Int(20)]));
        assert!(!contains(&b, &[Value::Null]));
        assert!(contains(&range(None, Some(0)), &[Value::Int(i64::MIN)]));
        assert!(contains(&range(Some(0), None), &[Value::Int(i64::MAX)]));
        assert!(!overlaps(
            &spec,
            &range(Some(0), Some(10)),
            &range(Some(10), Some(20))
        ));
        assert!(overlaps(
            &spec,
            &range(Some(0), Some(11)),
            &range(Some(10), Some(20))
        ));
        assert!(overlaps(
            &spec,
            &range(None, Some(5)),
            &range(None, Some(1))
        ));
        assert!(!overlaps(
            &spec,
            &range(None, Some(5)),
            &range(Some(5), None)
        ));
        let l = PartBound::List(vec![Value::Int(1), Value::Null]);
        assert!(contains(&l, &[Value::Null]));
        assert!(contains(&l, &[Value::Int(1)]));
        assert!(!contains(&l, &[Value::Int(2)]));
        let h = |m, r| PartBound::Hash {
            modulus: m,
            remainder: r,
        };
        assert!(!overlaps(&spec, &h(4, 1), &h(4, 2)));
        assert!(overlaps(&spec, &h(4, 1), &h(2, 1)));
        assert!(!overlaps(&spec, &h(4, 1), &h(2, 0)));
        let hits = (0..1000)
            .filter(|i| contains(&h(4, 3), &[Value::Int(*i)]))
            .count();
        assert!((150..350).contains(&hits), "{hits}");
        let mut w = Writer::default();
        for b in [range(None, Some(3)), l.clone(), h(8, 5), PartBound::Default] {
            w.0.clear();
            b.encode(&mut w);
            assert_eq!(PartBound::decode(&mut Reader::new(&w.0)).unwrap(), b);
        }
    }
}
