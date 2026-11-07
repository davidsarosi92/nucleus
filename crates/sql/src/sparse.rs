//! `sparsevec` (PLAN.md §M14.12 V6b): vectors of many dimensions of which
//! few are not zero — a learned sparse model's (SPLADE's) term weights, a
//! bag of words — stored as their non-zero components only.  A registered
//! type ([`Value::Ext`]) whose canonical bytes are its dimensions and its
//! non-zero components' count (little-endian `u32`s), their positions
//! (0-based, ascending, `u32`s) and their values (`f32`s); the text form
//! is pgvector's, `{1:0.5,4:2}/10` (positions from 1).  Equality and
//! hashing on those bytes; the Euclidean distance as the type's distance
//! (`<->`), the cosine distance, the negated inner product and the taxicab
//! distance as its operators (`<=>`, `<#>`, `<+>`) and through the vector
//! distance functions, over the positions both have, merged; casts to and
//! from `vector`.  `sparsevec(n)` checks the dimensions on every write.
//!
//! Searched exactly (`ORDER BY v <#> q LIMIT k`, a scan and a top-k); an
//! index for them is an inverted one, §M14.21's.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{CanonicalBytes, Distance, Equality, Hashing, Registry, Type, TypeRef};
use crate::types::{DataType, Value};
use crate::vectors::Metric;

/// The most dimensions (pgvector's).
pub const MAX_DIMS: u32 = 1_000_000_000;
/// The most non-zero components (pgvector's).
pub const MAX_NONZERO: usize = 16_000;

/// The `sparsevec` type.
pub struct SparseType;

/// The interned `sparsevec` type (the built-in module registers it).
pub fn sparse_type() -> TypeRef {
    TypeRef::named("sparsevec").expect("the built-in module registers sparsevec")
}

/// The type as a column's.
pub fn data_type() -> DataType {
    DataType::Ext(sparse_type())
}

/// A sparse vector: its dimensions, and its non-zero components'
/// positions (0-based, ascending) and values.
#[derive(Clone, Debug, PartialEq)]
pub struct Sparse {
    /// Its dimensions.
    pub dims: u32,
    /// The non-zero components' positions, ascending.
    pub at: Vec<u32>,
    /// Their values.
    pub values: Vec<f32>,
}

impl Sparse {
    /// From `(position, value)` pairs (0-based, any order) of a vector of
    /// `dims` dimensions, checked: at least one dimension and at most
    /// [`MAX_DIMS`], positions within them and each once, values finite;
    /// zeros dropped; at most [`MAX_NONZERO`] left.
    pub fn checked(dims: u64, mut pairs: Vec<(u64, f32)>) -> Result<Sparse> {
        if dims == 0 {
            return Err(Error::invalid_parameter(
                "sparsevec must have at least 1 dimension",
            ));
        }
        if dims > u64::from(MAX_DIMS) {
            return Err(Error::invalid_parameter(format!(
                "sparsevec cannot have more than {MAX_DIMS} dimensions"
            )));
        }
        pairs.sort_by_key(|p| p.0);
        if pairs.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(Error::data_exception(
                "sparsevec indices must not contain duplicates",
            ));
        }
        if let Some(p) = pairs.iter().find(|p| p.0 >= dims) {
            return Err(Error::data_exception(format!(
                "sparsevec index {} out of bounds for {dims} dimensions",
                p.0 + 1
            )));
        }
        if let Some(p) = pairs.iter().find(|p| !p.1.is_finite()) {
            return Err(Error::out_of_range(if p.1.is_nan() {
                "NaN not allowed in sparsevec".to_string()
            } else {
                "infinite value not allowed in sparsevec".to_string()
            }));
        }
        pairs.retain(|p| p.1 != 0.0);
        if pairs.len() > MAX_NONZERO {
            return Err(Error::invalid_parameter(format!(
                "sparsevec cannot have more than {MAX_NONZERO} non-zero elements"
            )));
        }
        Ok(Sparse {
            dims: dims as u32,
            at: pairs.iter().map(|p| p.0 as u32).collect(),
            values: pairs.iter().map(|p| p.1).collect(),
        })
    }

    /// As a value.
    pub fn value(&self) -> Value {
        let mut b = Vec::with_capacity(8 + 8 * self.at.len());
        b.extend_from_slice(&self.dims.to_le_bytes());
        b.extend_from_slice(&(self.at.len() as u32).to_le_bytes());
        for a in &self.at {
            b.extend_from_slice(&a.to_le_bytes());
        }
        for v in &self.values {
            b.extend_from_slice(&v.to_le_bytes());
        }
        Value::ext(sparse_type(), b)
    }

    /// Of a value of the type.
    pub fn of(v: &Value) -> Result<Sparse> {
        let b = match v {
            Value::Ext(t, b) if *t == sparse_type() => b,
            _ => return Err(Error::internal("not a sparsevec")),
        };
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (dims, n) = (word(0), word(4) as usize);
        Ok(Sparse {
            dims,
            at: (0..n).map(|i| word(8 + 4 * i)).collect(),
            values: (0..n)
                .map(|i| f32::from_bits(word(8 + 4 * n + 4 * i)))
                .collect(),
        })
    }

    /// Its components, every one (for a cast to `vector`).
    pub fn dense(&self) -> Vec<f32> {
        let mut out = vec![0.0; self.dims as usize];
        for (a, v) in self.at.iter().zip(&self.values) {
            out[*a as usize] = *v;
        }
        out
    }

    /// Its norm.
    pub fn norm(&self) -> f64 {
        self.values
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>()
            .sqrt()
    }

    /// Metric `m`'s distance to `o` (of the same dimensions).
    pub fn distance(&self, m: Metric, o: &Sparse) -> Result<f64> {
        distance(m, &self.value(), &o.value())
    }
}

/// A value of the type read in place: its dimensions, its positions' and
/// its values' bytes — a distance allocates nothing.
struct View<'a> {
    dims: u32,
    at: &'a [[u8; 4]],
    values: &'a [[u8; 4]],
}

impl<'a> View<'a> {
    fn of(v: &'a Value) -> Result<View<'a>> {
        let b = match v {
            Value::Ext(t, b) if *t == sparse_type() => b,
            _ => return Err(Error::internal("not a sparsevec")),
        };
        let words = b.as_chunks::<4>().0;
        let n = u32::from_le_bytes(words[1]) as usize;
        Ok(View {
            dims: u32::from_le_bytes(words[0]),
            at: &words[2..2 + n],
            values: &words[2 + n..2 + 2 * n],
        })
    }

    fn at(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.at[i])
    }

    fn value(&self, i: usize) -> f64 {
        f64::from(f32::from_le_bytes(self.values[i]))
    }

    /// The sum of `term(x, y)` over the positions either has (the other's
    /// component zero where it has none), merged.
    fn merged(&self, o: &View, term: impl Fn(f64, f64) -> f64) -> f64 {
        let (n, m) = (self.at.len(), o.at.len());
        let (mut i, mut j, mut sum) = (0, 0, 0.0);
        while i < n || j < m {
            let order = if i == n {
                Ordering::Greater
            } else if j == m {
                Ordering::Less
            } else {
                self.at(i).cmp(&o.at(j))
            };
            let (x, y) = match order {
                Ordering::Less => (self.value(i), 0.0),
                Ordering::Greater => (0.0, o.value(j)),
                Ordering::Equal => (self.value(i), o.value(j)),
            };
            i += usize::from(order.is_le());
            j += usize::from(order.is_ge());
            sum += term(x, y);
        }
        sum
    }

    /// The inner product (over the positions both have).
    fn dot(&self, o: &View) -> f64 {
        let (n, m) = (self.at.len(), o.at.len());
        let (mut i, mut j, mut sum) = (0, 0, 0.0);
        while i < n && j < m {
            let (a, b) = (self.at(i), o.at(j));
            if a == b {
                sum += self.value(i) * o.value(j);
            }
            i += usize::from(a <= b);
            j += usize::from(a >= b);
        }
        sum
    }

    fn norm(&self) -> f64 {
        (0..self.values.len())
            .map(|i| self.value(i) * self.value(i))
            .sum::<f64>()
            .sqrt()
    }
}

/// Metric `m`'s distance between two values of the type (of the same
/// dimensions).
pub fn distance(m: Metric, a: &Value, b: &Value) -> Result<f64> {
    let (a, b) = (View::of(a)?, View::of(b)?);
    if a.dims != b.dims {
        return Err(Error::data_exception(format!(
            "different sparsevec dimensions {} and {}",
            a.dims, b.dims
        )));
    }
    Ok(match m {
        Metric::L2 => a.merged(&b, |x, y| (x - y) * (x - y)).sqrt(),
        Metric::Cosine => {
            // A zero vector has no direction: NaN, as the vector's.
            let cos = a.dot(&b) / (a.norm() * b.norm());
            1.0 - cos.clamp(-1.0, 1.0)
        }
        Metric::NegInner => -a.dot(&b),
        Metric::L1 => a.merged(&b, |x, y| (x - y).abs()),
    })
}

/// Parses `{1:0.5,4:2}/10`.
pub fn parse(text: &str) -> Result<Sparse> {
    let bad = || {
        Error::invalid_text(format!(
            "invalid input syntax for type sparsevec: \"{text}\""
        ))
    };
    let (body, dims) = text.trim().rsplit_once('/').ok_or_else(bad)?;
    let dims: u64 = dims.trim().parse().map_err(|_| bad())?;
    let inner = body
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(bad)?;
    let mut pairs = Vec::new();
    if !inner.trim().is_empty() {
        for p in inner.split(',') {
            let (at, v) = p.split_once(':').ok_or_else(bad)?;
            let at: u64 = at.trim().parse().map_err(|_| bad())?;
            if at == 0 {
                return Err(Error::data_exception(
                    "sparsevec index 0 out of bounds (they start at 1)",
                ));
            }
            let v = v.trim();
            let f = v.parse::<f32>().map_err(|_| bad())?;
            if f.is_infinite() && !v.to_ascii_lowercase().contains("inf") {
                return Err(Error::out_of_range(format!(
                    "\"{v}\" is out of range for type sparsevec"
                )));
            }
            pairs.push((at - 1, f));
        }
    }
    Sparse::checked(dims, pairs)
}

/// The text form, `{1:0.5,4:2}/10`.
pub fn format(s: &Sparse) -> String {
    let parts: Vec<String> =
        s.at.iter()
            .zip(&s.values)
            .map(|(a, v)| format!("{}:{}", a + 1, crate::vectors::component_text(*v)))
            .collect();
    format!("{{{}}}/{}", parts.join(","), s.dims)
}

/// The Euclidean distance as the type's distance.
struct Euclidean;

impl Distance for Euclidean {
    fn operator(&self) -> &str {
        "<->"
    }
    fn distance(&self, a: &Value, b: &Value) -> Result<f64> {
        distance(Metric::L2, a, b)
    }
}

impl Type for SparseType {
    fn name(&self) -> &str {
        "sparsevec"
    }
    fn category(&self) -> &str {
        "vector"
    }
    fn input(&self, text: &str) -> Result<Value> {
        Ok(parse(text)?.value())
    }
    fn output(&self, v: &Value) -> String {
        Sparse::of(v).map_or_else(|_| "?".into(), |s| format(&s))
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn distance(&self) -> Option<&dyn Distance> {
        Some(&Euclidean)
    }
    fn modifier(&self, n: u32) -> Result<()> {
        if n == 0 || n > MAX_DIMS {
            return Err(Error::invalid_parameter(format!(
                "dimensions for type sparsevec must be between 1 and {MAX_DIMS}"
            )));
        }
        Ok(())
    }
    fn fit(&self, v: Value, n: u32) -> Result<Value> {
        let d = Sparse::of(&v)?.dims;
        if d != n {
            return Err(Error::data_exception(format!(
                "expected {n} dimensions, not {d}"
            )));
        }
        Ok(v)
    }
    fn cast_from(&self, v: &Value) -> Option<Result<Value>> {
        match v {
            Value::Ext(t, _) if *t == crate::vectors::vector_type() => {
                Some(crate::vectors::components(v).and_then(|cs| {
                    let pairs = cs.iter().enumerate().map(|(i, &c)| (i as u64, c)).collect();
                    Ok(Sparse::checked(cs.len() as u64, pairs)?.value())
                }))
            }
            _ => None,
        }
    }
    fn cast_to(&self, v: &Value, to: DataType) -> Option<Result<Value>> {
        (to == crate::vectors::data_type()).then(|| {
            let s = Sparse::of(v)?;
            if s.dims as usize > crate::vectors::MAX_DIMS {
                return Err(Error::invalid_parameter(format!(
                    "vector cannot have more than {} dimensions",
                    crate::vectors::MAX_DIMS
                )));
            }
            crate::vectors::checked(&s.dense())
        })
    }
    fn examples(&self) -> Vec<Value> {
        [
            (1, vec![]),
            (3, vec![(0, 1.0), (2, -2.5)]),
            (3, vec![(2, -2.5), (0, 1.0)]),
            (5, vec![(4, 3.4028235e38)]),
            (MAX_DIMS as u64, vec![(MAX_DIMS as u64 - 1, 1e-30)]),
            (17, vec![(1, 0.0), (3, 0.25)]),
        ]
        .into_iter()
        .map(|(d, p)| Sparse::checked(d, p).expect("an example").value())
        .collect()
    }
}

/// Registers the type and its operators (part of the built-in module; the
/// distance functions are the vector's, which take either).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(SparseType))?;
    let s = data_type();
    for (m, name) in [
        (Metric::Cosine, "cosine_distance"),
        (Metric::NegInner, "negative_inner_product"),
        (Metric::L1, "l1_distance"),
    ] {
        r.add_operator(
            m.operator(),
            s,
            s,
            crate::vectors::distance_function(m, name),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() {
        let _ = Registry::shared_builtin();
    }

    #[test]
    fn text_forms() {
        reg();
        for (text, want) in [
            ("{1:1,3:2.5}/5", "{1:1,3:2.5}/5"),
            (" { 3 : 2.5 , 1 : 1 } / 5 ", "{1:1,3:2.5}/5"),
            ("{}/3", "{}/3"),
            ("{2:0,1:-0.5}/2", "{1:-0.5}/2"),
            ("{1:1e-7}/1000000000", "{1:1e-7}/1000000000"),
        ] {
            let v = SparseType.input(text).unwrap();
            assert_eq!(SparseType.output(&v), want, "{text}");
            assert_eq!(SparseType.input(want).unwrap(), v);
        }
        for bad in [
            "",
            "{}",
            "{1:1}",
            "{}/0",
            "{0:1}/3",
            "{4:1}/3",
            "{1:1,1:2}/3",
            "{1:NaN}/3",
            "{1:inf}/3",
            "{1:1e39}/3",
            "{a:1}/3",
            "{1:1/3",
            "{}/1000000001",
        ] {
            assert!(SparseType.input(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn distances() {
        reg();
        let a = parse("{1:1,2:2,3:3}/4").unwrap();
        let b = parse("{1:4,2:6,4:3}/4").unwrap();
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(close(a.distance(Metric::L2, &b).unwrap(), 43f64.sqrt()));
        assert!(close(a.distance(Metric::L1, &b).unwrap(), 13.0));
        assert!(close(a.distance(Metric::NegInner, &b).unwrap(), -16.0));
        let c = a.distance(Metric::Cosine, &b).unwrap();
        assert!(close(c, 1.0 - 16.0 / (14f64.sqrt() * 61f64.sqrt())), "{c}");
        assert!(a.distance(Metric::L2, &parse("{}/5").unwrap()).is_err());
        assert!(
            a.distance(Metric::Cosine, &parse("{}/4").unwrap())
                .unwrap()
                .is_nan()
        );
        // As the dense vectors' distances.
        let (da, db) = (
            crate::vectors::value(&a.dense()),
            crate::vectors::value(&b.dense()),
        );
        for m in [Metric::L2, Metric::L1, Metric::NegInner, Metric::Cosine] {
            assert!(close(a.distance(m, &b).unwrap(), m.of(&da, &db).unwrap()));
        }
    }
}
