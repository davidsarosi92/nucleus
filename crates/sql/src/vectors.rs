//! `vector` (PLAN.md §M14.12): embeddings beside the data they describe —
//! a registered type ([`Value::Ext`]) whose canonical bytes are its
//! components as little-endian `f32`s, built on the capability model like
//! every other type: equality and hashing on those bytes, a total order
//! (component by component, then the shorter first) with an
//! order-preserving key encoding, the Euclidean distance as its distance
//! (`<->`), and the other distances as operators (`<=>` cosine, `<#>` the
//! negated inner product, `<+>` taxicab).  Distances are the batch kernels
//! of `nucleus-kernels`, chosen for the CPU at run time.  `vector(n)`
//! checks the dimensions on every write.

use std::cmp::Ordering;
use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, Arithmetic, CallContext, CanonicalBytes, Distance,
    Elements, Equality, Hashing, KeyEncoding, Registry, Returns, ScalarFunction, Signature,
    TotalOrder, Type, TypeRef, Volatility,
};
use crate::fsum::ExactSum;
use crate::types::{DataType, Value};

/// The most components a vector may have (pgvector's limit).
pub const MAX_DIMS: usize = 16_000;

/// The `vector` type.
pub struct VectorType;

/// The interned `vector` type (the built-in module registers it).
pub fn vector_type() -> TypeRef {
    TypeRef::named("vector").expect("the built-in module registers vector")
}

/// The type as a column's.
pub fn data_type() -> DataType {
    DataType::Ext(vector_type())
}

/// A vector value of `components` (finite: checked by the caller).
pub fn value(components: &[f32]) -> Value {
    let mut b = Vec::with_capacity(components.len() * 4);
    for &c in components {
        // One zero: `-0` and `0` are equal, so they must be the same bytes.
        let c = if c == 0.0 { 0.0f32 } else { c };
        b.extend_from_slice(&c.to_le_bytes());
    }
    Value::ext(vector_type(), b)
}

/// A vector's components.
pub fn components(v: &Value) -> Result<Vec<f32>> {
    let b = bytes(v)?;
    Ok(b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect())
}

fn bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Ext(t, b) if *t == vector_type() => Ok(b),
        _ => Err(Error::internal("not a vector")),
    }
}

/// A vector's dimensions.
pub fn dims(v: &Value) -> Result<usize> {
    Ok(bytes(v)?.len() / 4)
}

/// Components checked and made a value: at least one, at most
/// [`MAX_DIMS`], each finite.
pub fn checked(components: &[f32]) -> Result<Value> {
    if components.is_empty() {
        return Err(Error::invalid_parameter(
            "vector must have at least 1 dimension",
        ));
    }
    if components.len() > MAX_DIMS {
        return Err(Error::invalid_parameter(format!(
            "vector cannot have more than {MAX_DIMS} dimensions"
        )));
    }
    if let Some(c) = components.iter().find(|c| !c.is_finite()) {
        return Err(Error::out_of_range(if c.is_nan() {
            "NaN not allowed in vector".to_string()
        } else {
            "infinite value not allowed in vector".to_string()
        }));
    }
    Ok(value(components))
}

fn same_dims(a: &Value, b: &Value) -> Result<()> {
    let (x, y) = (dims(a)?, dims(b)?);
    if x != y {
        return Err(Error::data_exception(format!(
            "different vector dimensions {x} and {y}"
        )));
    }
    Ok(())
}

/// Parses `[1, 2.5, -3]`.
pub fn parse(text: &str) -> Result<Vec<f32>> {
    let bad = || Error::invalid_text(format!("invalid input syntax for type vector: \"{text}\""));
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(bad)?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|c| {
            let c = c.trim();
            if c.is_empty() {
                return Err(bad());
            }
            let f = c.parse::<f32>().map_err(|_| bad())?;
            // A finite literal out of `f32`'s range (`1e39`).
            if f.is_infinite() && !c.to_ascii_lowercase().contains("inf") {
                return Err(Error::out_of_range(format!(
                    "\"{c}\" is out of range for type vector"
                )));
            }
            Ok(f)
        })
        .collect()
}

/// A component's shortest text that reads back as it: plain or with an
/// exponent, whichever is shorter (`1`, `2.5`, `1e-7`, `3.4028235e38`).
pub(crate) fn component_text(c: f32) -> String {
    let plain = format!("{c}");
    let exp = format!("{c:e}");
    if exp.len() < plain.len() { exp } else { plain }
}

/// The text form: `[1,2.5,-3]`.
pub fn format(components: &[f32]) -> String {
    let parts: Vec<String> = components.iter().map(|&c| component_text(c)).collect();
    format!("[{}]", parts.join(","))
}

/// Component by component, then the shorter first.
struct ByComponents;

impl TotalOrder for ByComponents {
    fn compare(&self, a: &Value, b: &Value) -> Result<Ordering> {
        let (x, y) = (components(a)?, components(b)?);
        for (p, q) in x.iter().zip(&y) {
            match p.total_cmp(q) {
                Ordering::Equal => {}
                o => return Ok(o),
            }
        }
        Ok(x.len().cmp(&y.len()))
    }
}

/// Each component as `01` and four bytes whose order is the float's, then
/// `00`: self-delimiting, and bytewise the order of [`ByComponents`].
struct Keyed;

/// An `f32`'s bits made to sort as the float does.
fn sortable(c: f32) -> u32 {
    let b = c.to_bits();
    if b >> 31 == 1 { !b } else { b ^ 0x8000_0000 }
}

fn unsortable(k: u32) -> f32 {
    f32::from_bits(if k >> 31 == 1 { k ^ 0x8000_0000 } else { !k })
}

impl KeyEncoding for Keyed {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        for c in components(v).unwrap_or_default() {
            out.push(1);
            out.extend_from_slice(&sortable(c).to_be_bytes());
        }
        out.push(0);
    }

    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        let bad = || Error::internal("malformed vector key");
        let mut at = 0;
        let mut cs = Vec::new();
        loop {
            match b.get(at) {
                Some(0) => break,
                Some(1) => {
                    let k = b.get(at + 1..at + 5).ok_or_else(bad)?;
                    cs.push(unsortable(u32::from_be_bytes([k[0], k[1], k[2], k[3]])));
                    at += 5;
                }
                _ => return Err(bad()),
            }
        }
        Ok((value(&cs), at + 1))
    }
}

/// Which distance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    /// Euclidean (`<->`).
    L2,
    /// One minus the cosine of the angle (`<=>`).
    Cosine,
    /// The inner product, negated so that nearer is smaller (`<#>`).
    NegInner,
    /// Taxicab (`<+>`).
    L1,
}

impl Metric {
    /// Its operator.
    pub fn operator(self) -> &'static str {
        match self {
            Metric::L2 => "<->",
            Metric::Cosine => "<=>",
            Metric::NegInner => "<#>",
            Metric::L1 => "<+>",
        }
    }

    /// The distance between two vectors' stored forms (of one dimension).
    pub fn between(self, a: &[u8], b: &[u8]) -> f64 {
        match self {
            Metric::L2 => nucleus_kernels::l2_squared(a, b).sqrt(),
            Metric::Cosine => {
                let (ab, aa, bb) = nucleus_kernels::dot_norms(a, b);
                // A zero vector has no direction: NaN, as pgvector's.
                let cos = ab / (aa * bb).sqrt();
                1.0 - cos.clamp(-1.0, 1.0)
            }
            Metric::NegInner => -nucleus_kernels::dot(a, b),
            Metric::L1 => nucleus_kernels::l1(a, b),
        }
    }

    /// The distance a call of `f` computes, if it is one of the vector
    /// distances: the type's (`<->`), its operators' and their functions'.
    pub fn of_function(f: &crate::ext::FnRef) -> Option<Metric> {
        Self::of_name(f.name())
    }

    /// The distance function `name` is, if one.
    pub fn of_name(name: &str) -> Option<Metric> {
        Some(match name {
            "<->(vector)" | "l2_distance" => Metric::L2,
            "cosine_distance" => Metric::Cosine,
            "negative_inner_product" => Metric::NegInner,
            "l1_distance" => Metric::L1,
            _ => return None,
        })
    }

    /// Between two values, checked to be of one dimension.
    pub fn of(self, a: &Value, b: &Value) -> Result<f64> {
        same_dims(a, b)?;
        Ok(self.between(bytes(a)?, bytes(b)?))
    }
}

/// The Euclidean distance as the type's distance.
struct Euclidean;

impl Distance for Euclidean {
    fn operator(&self) -> &str {
        "<->"
    }
    fn distance(&self, a: &Value, b: &Value) -> Result<f64> {
        Metric::L2.of(a, b)
    }
}

/// Component-wise `+`, `-`, `*`, and `||` (one after the other).
struct Elementwise;

impl Arithmetic for Elementwise {
    fn operators(&self) -> &[&str] {
        &["+", "-", "*", "||"]
    }

    fn apply(&self, op: &str, a: &Value, b: &Value) -> Result<Value> {
        let (x, y) = (components(a)?, components(b)?);
        if op == "||" {
            return checked(&[x, y].concat());
        }
        same_dims(a, b)?;
        let out: Vec<f32> = x
            .iter()
            .zip(&y)
            .map(|(p, q)| match op {
                "+" => p + q,
                "-" => p - q,
                _ => p * q,
            })
            .collect();
        if out.iter().any(|c| !c.is_finite()) {
            return Err(Error::out_of_range("value out of range: overflow"));
        }
        Ok(value(&out))
    }
}

impl Type for VectorType {
    fn name(&self) -> &str {
        "vector"
    }
    fn category(&self) -> &str {
        "vector"
    }
    // A search reads the rows it finds for their vectors (PLAN.md §M14.12
    // V7): in the row, one read.
    fn stored_with_row(&self) -> bool {
        true
    }
    fn input(&self, text: &str) -> Result<Value> {
        checked(&parse(text)?)
    }
    fn output(&self, v: &Value) -> String {
        components(v).map_or_else(|_| "?".into(), |c| format(&c))
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&ByComponents)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Keyed)
    }
    fn distance(&self) -> Option<&dyn Distance> {
        Some(&Euclidean)
    }
    fn arithmetic(&self) -> Option<&dyn Arithmetic> {
        Some(&Elementwise)
    }
    fn elements(&self) -> Option<&dyn Elements> {
        Some(self)
    }
    fn modifier(&self, n: u32) -> Result<()> {
        if n == 0 || n as usize > MAX_DIMS {
            return Err(Error::invalid_parameter(format!(
                "dimensions for type vector must be between 1 and {MAX_DIMS}"
            )));
        }
        Ok(())
    }
    fn fit(&self, v: Value, n: u32) -> Result<Value> {
        let d = dims(&v)?;
        if d != n as usize {
            return Err(Error::data_exception(format!(
                "expected {n} dimensions, not {d}"
            )));
        }
        Ok(v)
    }
    fn cast_from(&self, v: &Value) -> Option<Result<Value>> {
        let Value::Array(items) = v else { return None };
        Some((|| {
            let mut cs = Vec::with_capacity(items.len());
            for i in items {
                if i.is_null() {
                    return Err(Error::not_null_violation("array must not contain nulls"));
                }
                match i.clone().cast(DataType::Real)? {
                    Value::Real(f) => cs.push(f),
                    _ => return Err(Error::internal("a real")),
                }
            }
            checked(&cs)
        })())
    }
    fn cast_to(&self, v: &Value, to: DataType) -> Option<Result<Value>> {
        match to {
            DataType::Array(DataType::Real) => Some(
                components(v).map(|cs| Value::Array(cs.into_iter().map(Value::Real).collect())),
            ),
            DataType::Array(&t) if t.is_number() => Some(components(v).and_then(|cs| {
                cs.into_iter()
                    .map(|c| Value::Real(c).cast(t))
                    .collect::<Result<Vec<_>>>()
                    .map(Value::Array)
            })),
            _ => None,
        }
    }
    fn aggregate(&self, name: &str) -> Option<Arc<dyn Aggregate>> {
        match name {
            "sum" => Some(Arc::new(VectorAgg { avg: false })),
            "avg" => Some(Arc::new(VectorAgg { avg: true })),
            _ => None,
        }
    }
    fn examples(&self) -> Vec<Value> {
        [
            vec![0.0],
            vec![1.0, 2.0, 3.0],
            vec![-1.5, 2.0],
            vec![1.0, 2.0],
            vec![1e-30, -3e38, 3.4028235e38],
            vec![-0.25; 17],
        ]
        .iter()
        .map(|c| value(c))
        .collect()
    }
}

// ------------------------------------------------------------- functions

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Distance(Metric),
    /// The inner product itself (`inner_product`).
    Inner,
    Dims,
    Norm,
    Normalize,
    Subvector,
}

struct VectorFn(F, &'static str);

/// Metric `m`'s distance function under `name`, for vectors and sparse
/// vectors alike (an operator of either type).
pub(crate) fn distance_function(m: Metric, name: &'static str) -> Arc<dyn ScalarFunction> {
    Arc::new(VectorFn(F::Distance(m), name))
}

/// Whether the arguments are sparse vectors (any typed one is).
fn sparse_args(args: &[ArgInfo]) -> bool {
    args.iter()
        .any(|a| a.ty == Some(crate::sparse::data_type()))
}

fn is_sparse(v: &Value) -> bool {
    matches!(v, Value::Ext(t, _) if *t == crate::sparse::sparse_type())
}

impl ScalarFunction for VectorFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::Distance(_) | F::Inner => 2..=2,
            F::Dims | F::Norm | F::Normalize => 1..=1,
            F::Subvector => 3..=3,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        // The distances, the norm and normalizing take sparse vectors too.
        let v = if sparse_args(args)
            && matches!(self.0, F::Distance(_) | F::Inner | F::Norm | F::Normalize)
        {
            crate::sparse::data_type()
        } else {
            data_type()
        };
        Ok(match self.0 {
            F::Distance(_) | F::Inner => Signature::new(
                vec![Arg::Untyped(v), Arg::Untyped(v)],
                Returns::Type(Some(DataType::Float)),
            ),
            F::Dims => Signature::new(
                vec![Arg::Untyped(v)],
                Returns::Type(Some(DataType::Integer)),
            ),
            F::Norm => Signature::new(vec![Arg::Untyped(v)], Returns::Type(Some(DataType::Float))),
            F::Normalize => Signature::new(vec![Arg::Untyped(v)], Returns::Type(Some(v))),
            F::Subvector => Signature::new(
                vec![
                    Arg::Untyped(v),
                    Arg::Convert(DataType::BigInt),
                    Arg::Convert(DataType::BigInt),
                ],
                Returns::Type(Some(v)),
            ),
        })
    }

    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }

    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        if is_sparse(&args[0]) {
            use crate::sparse::{Sparse, distance};
            return Ok(match self.0 {
                F::Distance(m) => Value::Float(distance(m, &args[0], &args[1])?),
                F::Inner => Value::Float(-distance(Metric::NegInner, &args[0], &args[1])?),
                F::Norm => Value::Float(Sparse::of(&args[0])?.norm()),
                F::Normalize => {
                    let mut s = Sparse::of(&args[0])?;
                    let norm = s.norm();
                    if norm > 0.0 {
                        for v in &mut s.values {
                            *v = (f64::from(*v) / norm) as f32;
                        }
                        let pairs = s.at.iter().map(|&a| u64::from(a)).zip(s.values).collect();
                        s = Sparse::checked(u64::from(s.dims), pairs)?;
                    }
                    s.value()
                }
                F::Dims | F::Subvector => return Err(Error::internal("a vector function")),
            });
        }
        Ok(match self.0 {
            F::Distance(m) => Value::Float(m.of(&args[0], &args[1])?),
            F::Inner => Value::Float(-Metric::NegInner.of(&args[0], &args[1])?),
            F::Dims => Value::Int(dims(&args[0])? as i64),
            F::Norm => {
                let b = bytes(&args[0])?;
                Value::Float(nucleus_kernels::dot(b, b).sqrt())
            }
            F::Normalize => {
                let b = bytes(&args[0])?;
                let norm = nucleus_kernels::dot(b, b).sqrt();
                let cs = components(&args[0])?;
                if norm == 0.0 {
                    return Ok(args[0].clone());
                }
                value(
                    &cs.iter()
                        .map(|&c| (f64::from(c) / norm) as f32)
                        .collect::<Vec<_>>(),
                )
            }
            F::Subvector => {
                let cs = components(&args[0])?;
                let (Value::Int(start), Value::Int(count)) = (&args[1], &args[2]) else {
                    return Err(Error::internal("integers"));
                };
                // 1-based, as pgvector's: what of [start, start + count)
                // lies within the vector.
                let from = (*start).max(1);
                let to = start.saturating_add(*count).min(cs.len() as i64 + 1);
                if to <= from {
                    return Err(Error::invalid_parameter(
                        "vector must have at least 1 dimension",
                    ));
                }
                value(&cs[(from - 1) as usize..(to - 1) as usize])
            }
        })
    }
}

/// `sum` and `avg` of vectors: each component's sum kept exactly (so the
/// result is the same in any order of the rows), rounded to `f32` once.
struct VectorAgg {
    avg: bool,
}

impl Aggregate for VectorAgg {
    fn name(&self) -> &str {
        if self.avg { "avg" } else { "sum" }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn order_free(&self) -> bool {
        true
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Untyped(data_type())],
            Returns::Type(Some(data_type())),
        ))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(VectorAcc {
            avg: self.avg,
            n: 0,
            sums: Vec::new(),
        })
    }
}

struct VectorAcc {
    avg: bool,
    n: u64,
    sums: Vec<ExactSum>,
}

impl Accumulator for VectorAcc {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        let cs = components(&args[0])?;
        if self.n == 0 {
            self.sums = vec![ExactSum::new(); cs.len()];
        } else if cs.len() != self.sums.len() {
            return Err(Error::data_exception(format!(
                "different vector dimensions {} and {}",
                self.sums.len(),
                cs.len()
            )));
        }
        for (s, c) in self.sums.iter_mut().zip(cs) {
            s.add(f64::from(c));
        }
        self.n += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        if self.n == 0 {
            return Ok(Value::Null);
        }
        let out: Vec<f32> = self
            .sums
            .iter()
            .map(|s| {
                let total = s.total().unwrap_or(f64::INFINITY);
                (if self.avg {
                    total / self.n as f64
                } else {
                    total
                }) as f32
            })
            .collect();
        if out.iter().any(|c| !c.is_finite()) {
            return Err(Error::out_of_range("value out of range: overflow"));
        }
        Ok(value(&out))
    }
}

impl Elements for VectorType {
    fn element_type(&self) -> DataType {
        DataType::Real
    }

    fn count(&self, v: &Value) -> Result<usize> {
        dims(v)
    }

    fn get(&self, v: &Value, i: usize) -> Result<Value> {
        let b = bytes(v)?;
        let c = b
            .get(i * 4..i * 4 + 4)
            .ok_or_else(|| Error::internal("a component out of range"))?;
        Ok(Value::Real(f32::from_le_bytes(c.try_into().unwrap())))
    }

    fn slice(&self, v: &Value, from: usize, to: usize) -> Result<Value> {
        let c = components(v)?;
        checked(&c[from..to])
    }

    fn with(&self, v: &Value, i: usize, x: &Value) -> Result<Value> {
        let mut c = components(v)?;
        c[i] = match x {
            Value::Real(f) => *f,
            Value::Null => {
                return Err(Error::data_exception(
                    "a vector's component must not be NULL",
                ));
            }
            _ => return Err(Error::internal("a vector component not a real")),
        };
        checked(&c)
    }
}

/// Registers the type, its operators and functions (part of the built-in
/// module).
pub fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(VectorType))?;
    let v = data_type();
    for m in [Metric::Cosine, Metric::NegInner, Metric::L1] {
        let name = match m {
            Metric::Cosine => "cosine_distance",
            Metric::NegInner => "negative_inner_product",
            _ => "l1_distance",
        };
        r.add_operator(m.operator(), v, v, Arc::new(VectorFn(F::Distance(m), name)))?;
    }
    let functions: [(F, &[&str]); 9] = [
        (F::Distance(Metric::L2), &["l2_distance"]),
        (F::Distance(Metric::Cosine), &["cosine_distance"]),
        (F::Distance(Metric::L1), &["l1_distance"]),
        (F::Distance(Metric::NegInner), &["negative_inner_product"]),
        (F::Inner, &["inner_product"]),
        (F::Dims, &["vector_dims", "dims"]),
        (F::Norm, &["vector_norm", "l2_norm", "norm"]),
        (F::Normalize, &["l2_normalize", "normalize"]),
        (F::Subvector, &["subvector"]),
    ];
    for (f, names) in functions {
        r.add_function(names, Arc::new(VectorFn(f, names[0])))?;
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
            ("[1,2.5,-3]", "[1,2.5,-3]"),
            (" [ 1 , 2 ] ", "[1,2]"),
            ("[1e-7, -0, 3.4028235e38]", "[1e-7,0,3.4028235e38]"),
            ("[0.1]", "[0.1]"),
        ] {
            let v = VectorType.input(text).unwrap();
            assert_eq!(VectorType.output(&v), want, "{text}");
            assert_eq!(VectorType.input(want).unwrap(), v);
        }
        for bad in [
            "", "[]", "[1,,2]", "1,2", "[1,2", "[a]", "[NaN]", "[inf]", "[1e39]",
        ] {
            assert!(VectorType.input(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn keys_sort_as_vectors() {
        reg();
        let mut vs: Vec<Value> = [
            vec![1.0, 2.0],
            vec![1.0],
            vec![-3.0, 9.0],
            vec![1.0, -2.0],
            vec![0.0, 0.0, 0.0],
            vec![1.0, 2.0, 0.5],
            vec![-0.5],
        ]
        .iter()
        .map(|c| value(c))
        .collect();
        let key = |v: &Value| {
            let mut k = Vec::new();
            Keyed.encode(v, &mut k);
            k
        };
        for v in &vs {
            let k = key(v);
            assert_eq!(Keyed.decode(&k).unwrap(), (v.clone(), k.len()));
        }
        let mut by_key = vs.clone();
        by_key.sort_by_key(key);
        vs.sort_by(|a, b| ByComponents.compare(a, b).unwrap());
        assert_eq!(by_key, vs);
    }

    #[test]
    fn distances() {
        reg();
        let (a, b) = (value(&[1.0, 2.0, 3.0]), value(&[4.0, 6.0, 3.0]));
        assert_eq!(Metric::L2.of(&a, &b).unwrap(), 5.0);
        assert_eq!(Metric::L1.of(&a, &b).unwrap(), 7.0);
        assert_eq!(Metric::NegInner.of(&a, &b).unwrap(), -25.0);
        let c = Metric::Cosine.of(&a, &b).unwrap();
        let want = 1.0 - 25.0 / (14f64.sqrt() * 61f64.sqrt());
        assert!((c - want).abs() < 1e-7, "{c} {want}");
        assert!(Metric::L2.of(&a, &value(&[1.0])).is_err());
        assert!(
            Metric::Cosine
                .of(&a, &value(&[0.0, 0.0, 0.0]))
                .unwrap()
                .is_nan()
        );
    }
}
