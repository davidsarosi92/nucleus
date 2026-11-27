//! Multi-valued index parts (PLAN.md §M15 D5): an entry per element.
//!
//! A B-tree index's key part may be the elements of an array — `CREATE
//! INDEX ON t ((json_array_elements(doc -> 'tags')))`, `((unnest(tags)))`
//! for an SQL array, or on a collection the path `tags[*]` —, optionally
//! each element's field by a chain of keys (`((json_array_elements(doc ->
//! 'items') ->> 'sku'))`, `items[*].sku`).  A row has an entry per
//! distinct value (MySQL's multi-valued index, MongoDB's multikey one);
//! a value that is no array is its one element, and a row with none (no
//! array, an empty one, `NULL`) has no entry — no condition looks up a
//! `NULL` element.  One such part an index, not `UNIQUE`.
//!
//! Bound, the part is a call of [`Each`] on the array: the writer
//! ([`crate::dml::IndexKeys`]) expands it into the entries, the scan
//! reads a row once however many of its entries match, and the optimizer
//! finds a value to look up in a condition on the array ([`probe`]: `a
//! @> c`, `c = ANY(a)`) — keeping the condition as the filter (an entry
//! says the row may match).  An element's field that holds an array
//! gives its elements (as MongoDB's multikey index does), so a
//! containment through the field finds the row.

use std::ops::RangeInclusive;
use std::sync::Arc;

use crate::ast;
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{ArgInfo, FnRef, Returns, ScalarFunction, Signature, Volatility};
use crate::types::{DataType, Value};

/// What a part's array is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A json array; its elements json (`text`: as text, the `_text`
    /// functions).
    Json {
        /// The elements as text (`json_array_elements_text`).
        text: bool,
    },
    /// An SQL array.
    Array,
}

/// A multi-valued part bound: the call `Each(array)`.
pub struct Each {
    /// Its name, as `EXPLAIN` and the catalog show it.
    name: String,
    /// What its array is.
    pub kind: Kind,
    /// Each element's field by these keys (json only).
    pub keys: Vec<String>,
    /// The last key's value as text (`->>`).
    pub text: bool,
    /// The values' type.
    pub ty: DataType,
    /// A lax path's values instead (`json_path_query(x, '$.k…[*]')`,
    /// PLAN.md §M16 Q2): its keys and the path.  Its entries are the items
    /// the path finds — what a lax comparison `@.k… op c` compares, arrays
    /// unwrapped as lax mode does.
    pub lax: Option<(Vec<String>, Box<crate::json::path::Path>)>,
}

impl ScalarFunction for Each {
    fn name(&self) -> &str {
        &self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let _ = args;
        Ok(Signature::new(
            vec![crate::ext::Arg::Keep],
            Returns::Type(Some(self.ty)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn as_each(&self) -> Option<&Each> {
        Some(self)
    }
    fn call(&self, _: &crate::ext::CallContext<'_>, _: &[Value]) -> Result<Value> {
        Err(Error::feature(
            "a multi-valued index part is a set of values, not one",
        ))
    }
}

impl Each {
    /// The values of `array` this part indexes: each element's (by the
    /// keys; `NULL` where an element has no such field).  Not
    /// deduplicated.
    pub fn values(&self, array: &Value) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        if let Some((_, path)) = &self.lax {
            if array.is_null() {
                return Ok(out);
            }
            use crate::json::path::eval::{self, Fail};
            let mut found = Vec::new();
            match eval::run(
                path,
                crate::json::bytes(array)?,
                None,
                false,
                Some(&mut found),
            ) {
                Ok(_) | Err(Fail::Soft(_)) => {}
                Err(Fail::Hard(e)) => return Err(e),
            }
            out.extend(found.iter().map(|i| crate::json::value(i.to_json())));
            return Ok(out);
        }
        match self.kind {
            Kind::Array => {
                if let Value::Array(items) = array {
                    out.extend(items.iter().cloned());
                }
            }
            Kind::Json { text } => {
                if !array.is_null() {
                    let bytes = crate::json::bytes(array)?;
                    let mut each = |b: &[u8]| -> Result<()> {
                        let mut at = Some(b);
                        for k in &self.keys {
                            at = match at {
                                Some(b) => crate::json::ops::field(b, k)?,
                                None => None,
                            };
                        }
                        let as_value = |b: &[u8]| -> Result<Value> {
                            Ok(if text || self.text {
                                crate::json::ops::as_text(b)?.map_or(Value::Null, Value::Text)
                            } else {
                                crate::json::value(b.to_vec())
                            })
                        };
                        match at {
                            None => out.push(Value::Null),
                            // An element's field holding an array: its
                            // elements (MongoDB's `items.tags`), so a
                            // containment by the field finds the row.
                            Some(b) if !self.keys.is_empty() => match crate::json::form::root(b)? {
                                crate::json::form::Node::Array(a) => {
                                    for i in 0..a.len() {
                                        out.push(as_value(a.bytes(i)?)?);
                                    }
                                }
                                _ => out.push(as_value(b)?),
                            },
                            Some(b) => out.push(as_value(b)?),
                        }
                        Ok(())
                    };
                    match crate::json::form::root(bytes)? {
                        crate::json::form::Node::Array(a) => {
                            for i in 0..a.len() {
                                each(a.bytes(i)?)?;
                            }
                        }
                        _ => each(bytes)?,
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The multi-valued part `x` is, if it is one: its [`Each`] and array.
pub fn of(x: &Expr) -> Option<(&Each, &Expr)> {
    let Expr::Call(f, args) = x else { return None };
    let each = f.as_each()?;
    Some((each, &args[0]))
}

/// Whether index expression `e` (as parsed) is a multi-valued part.
pub fn is_multi(e: &ast::Expr) -> bool {
    split(e).is_some()
}

/// Whether index expression text `sql` is a multi-valued part.
pub fn is_multi_sql(sql: &str) -> bool {
    (sql.contains("elements") || sql.contains("unnest") || sql.contains("path_query"))
        && crate::parser::parse_expr(sql).is_ok_and(|e| is_multi(&e))
}

/// The parts of a multi-valued part's expression: the function, its
/// array, the keys after it, whether the last is `->>` (text), and a lax
/// path's text (`json_path_query(x, '…')`).
type Split<'e> = (&'e str, &'e ast::Expr, Vec<String>, bool, Option<String>);

/// `f(array) -> 'k1' … ->> 'kn'` or `json_path_query(x, 'path')`.
fn split(e: &ast::Expr) -> Option<Split<'_>> {
    match e {
        ast::Expr::Function { name, args, .. }
            if args.len() == 2
                && matches!(name.as_str(), "json_path_query" | "jsonb_path_query") =>
        {
            let ast::Expr::Literal(Value::Text(p)) = &args[1] else {
                return None;
            };
            Some((name.as_str(), &args[0], Vec::new(), false, Some(p.clone())))
        }
        ast::Expr::Function { name, args, .. } if args.len() == 1 => {
            let name = name.as_str();
            matches!(
                name,
                "json_array_elements"
                    | "jsonb_array_elements"
                    | "json_array_elements_text"
                    | "jsonb_array_elements_text"
                    | "unnest"
            )
            .then(|| (name, &args[0], Vec::new(), false, None))
        }
        ast::Expr::Operator { op, left, right } if op == "->" || op == "->>" => {
            let ast::Expr::Literal(Value::Text(k)) = &**right else {
                return None;
            };
            let (f, a, mut keys, text, lax) = split(left)?;
            if text || lax.is_some() {
                return None; // a key of a text, or after a path
            }
            keys.push(k.clone());
            Some((f, a, keys, op == "->>", None))
        }
        _ => None,
    }
}

/// Binds multi-valued part `e` over `table`: the call of its [`Each`] on
/// its array, and the values' type.  `None`: `e` is no such part.
pub fn bind(
    binder: &mut crate::binder::Binder,
    table: &crate::catalog::Table,
    e: &ast::Expr,
) -> Result<Option<(Expr, DataType)>> {
    let Some((f, a, keys, text, lax)) = split(e) else {
        return Ok(None);
    };
    let (array, ty) = binder.expr(a, &crate::dml::table_scope(table))?;
    if let Some(text) = lax {
        if ty.is_some_and(|t| t != crate::json::data_type()) {
            return Err(Error::type_mismatch(format!("{f}() takes json")));
        }
        let path = crate::json::path::parse(&text)?;
        let lax_keys = lax_keys(&path).ok_or_else(|| {
            Error::feature(format!(
                "an index part {f}(x, path) takes a lax path of keys ending in [*] ('$.a.b[*]'), not '{text}'"
            ))
        })?;
        let json = crate::json::data_type();
        let each = Each {
            name: format!("{f}(…, '{path}')"),
            kind: Kind::Json { text: false },
            keys: Vec::new(),
            text: false,
            ty: json,
            lax: Some((lax_keys, Box::new(path))),
        };
        return Ok(Some((
            Expr::Call(FnRef::new(Arc::new(each)), vec![array]),
            json,
        )));
    }
    let json = crate::json::data_type();
    let kind = match (f, ty) {
        ("unnest", Some(DataType::Array(_))) => Kind::Array,
        ("unnest", _) => {
            return Err(Error::type_mismatch("unnest() in an index takes an array"));
        }
        (_, Some(t)) if t != json => {
            return Err(Error::type_mismatch(format!("{f}() takes json, not {}", t)));
        }
        (f, _) => Kind::Json {
            text: f.ends_with("_text"),
        },
    };
    let elem = match (kind, ty) {
        (Kind::Array, Some(DataType::Array(e))) => *e,
        (Kind::Json { text: true }, _) => DataType::Text,
        _ if text => DataType::Text,
        _ => json,
    };
    if !keys.is_empty() && kind != (Kind::Json { text: false }) {
        return Err(Error::type_mismatch(
            "only a json element has fields to index",
        ));
    }
    let mut name = format!("{f}(…)");
    for (i, k) in keys.iter().enumerate() {
        let op = if text && i + 1 == keys.len() {
            "->>"
        } else {
            "->"
        };
        name.push_str(&format!(" {op} '{}'", k.replace('\'', "''")));
    }
    let each = Each {
        name,
        kind,
        keys,
        text,
        ty: elem,
        lax: None,
    };
    Ok(Some((
        Expr::Call(FnRef::new(Arc::new(each)), vec![array]),
        elem,
    )))
}

/// A lax path `$.k…[*]`'s keys (`None`: another path).
fn lax_keys(path: &crate::json::path::Path) -> Option<Vec<String>> {
    use crate::json::path::ast::{Expr as P, Step};
    let P::Chain(head, steps) = &path.expr else {
        return None;
    };
    let (Some(Step::AnyArray), keys) = (steps.last(), &steps[..steps.len().checked_sub(1)?]) else {
        return None;
    };
    if path.strict || !matches!(**head, P::Root) {
        return None;
    }
    keys.iter()
        .map(|s| match s {
            Step::Key(k) => Some(k.clone()),
            _ => None,
        })
        .collect()
}

/// The bound on multi-valued part `part` condition `pred` implies, if
/// any: every row `pred` holds for has an entry within it (the condition
/// stays the scan's filter).  An equality: `a @> c` — `a` the part's
/// array or a json value it is a path below (`doc @> '{"tags": ["x"]}'`),
/// `c` a constant: `c` down that path, its first element if an array,
/// down the part's keys, a scalar; `c = ANY(a)` of an SQL array.  An
/// equality or a range: `x @? 'strict $.k…[*].f… ? (@.g… op c)'`, the
/// path to `[*]` the part's array below `x`, the keys after it (in the
/// filter or before it) the part's — strict only: lax unwraps arrays on
/// the way to `[*]`, which the part's array does not.
pub fn probe(pred: &Expr, part: &Expr) -> Option<(crate::ast::BinaryOp, Expr)> {
    use crate::ast::BinaryOp;
    if let Expr::Call(f, args) = pred
        && f.name() == "@?(json,jsonpath)"
        && let [x, Expr::Literal(p)] = &args[..]
    {
        let (each, array) = of(part)?;
        let path = crate::json::path::compiled(p).ok()?;
        if let Some((keys, _)) = &each.lax {
            let mut want = below(array, x)?;
            want.extend(keys.iter().cloned());
            return lax_bound(&want, &path);
        }
        return path_bound(each, &below(array, x)?, &path);
    }
    if let Some((keys, _)) = &of(part)?.0.lax {
        return chain_equal(pred, of(part)?.1, keys).map(|e| (BinaryOp::Eq, e));
    }
    contained(pred, part).map(|e| (BinaryOp::Eq, e))
}

/// `strict $.k…[*].f… ? (@.g… op c)` (or `c op @…`, a conjunct of the
/// filter) as a bound on `each`'s values, the array at keys `prefix`.
fn path_bound(
    each: &Each,
    prefix: &[String],
    path: &crate::json::path::Path,
) -> Option<(crate::ast::BinaryOp, Expr)> {
    use crate::ast::BinaryOp;
    use crate::json::path::ast::{Cmp, Expr as P, Step};
    if !path.strict || !matches!(each.kind, Kind::Json { text: false }) {
        return None;
    }
    let P::Chain(head, steps) = &path.expr else {
        return None;
    };
    if !matches!(**head, P::Root) || steps.len() < prefix.len() + 2 {
        return None;
    }
    for (s, k) in steps.iter().zip(prefix) {
        let Step::Key(s) = s else { return None };
        if s != k {
            return None;
        }
    }
    let rest = &steps[prefix.len()..];
    let (Step::AnyArray, Some((Step::Filter(cond), between))) = (&rest[0], rest[1..].split_last())
    else {
        return None;
    };
    let mut keys = Vec::new();
    for s in between {
        let Step::Key(k) = s else { return None };
        keys.push(k.clone());
    }
    // `@` or `@.g…`: its keys.
    let current = |e: &P| -> Option<Vec<String>> {
        match e {
            P::Current => Some(Vec::new()),
            P::Chain(h, steps) if matches!(**h, P::Current) => steps
                .iter()
                .map(|s| match s {
                    Step::Key(k) => Some(k.clone()),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    };
    let constant = |e: &P| -> Option<Value> {
        match e {
            P::Num(_, b) | P::Str(_, b) => Some(crate::json::value(b.to_vec())),
            P::Bool(_) | P::Null => None,
            _ => None,
        }
    };
    let mut conds = vec![&**cond];
    while let Some(c) = conds.pop() {
        match c {
            P::And(a, b) => {
                conds.push(a);
                conds.push(b);
            }
            P::Cmp(op, l, r) => {
                let (at, v, flip) = match (current(l), constant(r)) {
                    (Some(at), Some(v)) => (at, v, false),
                    _ => match (constant(l), current(r)) {
                        (Some(v), Some(at)) => (at, v, true),
                        _ => continue,
                    },
                };
                let mut all = keys.clone();
                all.extend(at);
                if all != each.keys {
                    continue;
                }
                let op = match (op, flip) {
                    (Cmp::Eq, _) => BinaryOp::Eq,
                    (Cmp::Lt, false) | (Cmp::Gt, true) => BinaryOp::Lt,
                    (Cmp::Le, false) | (Cmp::Ge, true) => BinaryOp::LtEq,
                    (Cmp::Gt, false) | (Cmp::Lt, true) => BinaryOp::Gt,
                    (Cmp::Ge, false) | (Cmp::Le, true) => BinaryOp::GtEq,
                    (Cmp::Ne, _) => continue,
                };
                // A text index (`->>`) orders numbers as text: equality
                // alone, by the constant's text.
                if each.text {
                    if op != BinaryOp::Eq {
                        continue;
                    }
                    let b = crate::json::bytes(&v).ok()?;
                    let t = crate::json::ops::as_text(b).ok()??;
                    return Some((op, Expr::Literal(Value::Text(t))));
                }
                return Some((op, Expr::Literal(v)));
            }
            _ => {}
        }
    }
    None
}

/// A lax path's bound on a lax part whose values are at keys `want`:
/// `$.k… ? (C)`, a conjunct of `C` `@.g… op c` (or `c op @.g…`) with the
/// keys before the filter and `g…` `want`.  An equality with a scalar,
/// or a range with a number (strings compare by code point in a path,
/// which need not be the index's order: no ranges on them); an equality
/// before a range.
fn lax_bound(
    want: &[String],
    path: &crate::json::path::Path,
) -> Option<(crate::ast::BinaryOp, Expr)> {
    use crate::ast::BinaryOp;
    use crate::json::path::ast::{Cmp, Expr as P, Step};
    if path.strict {
        return None;
    }
    let P::Chain(head, steps) = &path.expr else {
        return None;
    };
    let (Some(Step::Filter(cond)), before) = (steps.last(), &steps[..steps.len() - 1]) else {
        return None;
    };
    if !matches!(**head, P::Root) {
        return None;
    }
    let mut prefix = Vec::new();
    for s in before {
        let Step::Key(k) = s else { return None };
        prefix.push(k.clone());
    }
    let current = |e: &P| -> Option<Vec<String>> {
        let mut keys = prefix.clone();
        match e {
            P::Current => {}
            P::Chain(h, steps) if matches!(**h, P::Current) => {
                for s in steps {
                    let Step::Key(k) = s else { return None };
                    keys.push(k.clone());
                }
            }
            _ => return None,
        }
        Some(keys)
    };
    // The constant's json value, and whether it is a number.
    let constant = |e: &P| -> Option<(Value, bool)> {
        let mut b = Vec::new();
        match e {
            P::Num(_, j) => return Some((crate::json::value(j.to_vec()), true)),
            P::Str(_, j) => return Some((crate::json::value(j.to_vec()), false)),
            P::Bool(v) => crate::json::form::put_bool(&mut b, *v),
            P::Null => crate::json::form::put_null(&mut b),
            _ => return None,
        }
        Some((crate::json::value(b), false))
    };
    let mut range = None;
    let mut conds = vec![&**cond];
    while let Some(c) = conds.pop() {
        match c {
            P::And(a, b) => {
                conds.push(a);
                conds.push(b);
            }
            P::Cmp(op, l, r) => {
                let (at, (v, number), flip) = match (current(l), constant(r)) {
                    (Some(at), Some(v)) => (at, v, false),
                    _ => match (constant(l), current(r)) {
                        (Some(v), Some(at)) => (at, v, true),
                        _ => continue,
                    },
                };
                if at != want {
                    continue;
                }
                let op = match (op, flip) {
                    (Cmp::Eq, _) => return Some((BinaryOp::Eq, Expr::Literal(v))),
                    (Cmp::Lt, false) | (Cmp::Gt, true) => BinaryOp::Lt,
                    (Cmp::Le, false) | (Cmp::Ge, true) => BinaryOp::LtEq,
                    (Cmp::Gt, false) | (Cmp::Lt, true) => BinaryOp::Gt,
                    (Cmp::Ge, false) | (Cmp::Le, true) => BinaryOp::GtEq,
                    (Cmp::Ne, _) => continue,
                };
                if number && range.is_none() {
                    range = Some((op, Expr::Literal(v)));
                }
            }
            _ => {}
        }
    }
    range
}

/// `x -> 'k1' … -> 'kn' = c` (or `#>`, bound as the same walk), `c` a
/// json constant but an array, on a lax part over `x` at keys `keys`:
/// the value there equal to `c` is one of the part's values (an array
/// would be its elements, so not served).
fn chain_equal(pred: &Expr, array: &Expr, keys: &[String]) -> Option<Expr> {
    use crate::json::form::{Node, root};
    let Expr::Binary(crate::ast::BinaryOp::Eq, a, b) = pred else {
        return None;
    };
    let json = crate::json::data_type();
    for (chain, c) in [(a, b), (b, a)] {
        let Expr::Literal(v @ Value::Ext(t, bytes)) = &**c else {
            continue;
        };
        if DataType::Ext(*t) != json || matches!(root(bytes).ok()?, Node::Array(_)) {
            continue;
        }
        if matches!(&**chain, Expr::Call(..)) && below(chain, array).as_deref() == Some(keys) {
            return Some(Expr::Literal(v.clone()));
        }
    }
    None
}

/// The value `a @> c` or `c = ANY(a)` looks up in `part` ([`probe`]).
fn contained(pred: &Expr, part: &Expr) -> Option<Expr> {
    use crate::json::form::{Node, root};
    let (each, array) = of(part)?;
    match pred {
        Expr::Call(f, args) if f.name() == "@>(json,json)" && args.len() == 2 => {
            let (Kind::Json { .. }, Expr::Literal(c)) = (each.kind, &args[1]) else {
                return None;
            };
            let path = below(array, &args[0])?;
            let mut at: &[u8] = crate::json::bytes(c).ok()?;
            for k in &path {
                let Ok(Node::Object(_)) = root(at) else {
                    return None;
                };
                at = crate::json::ops::field(at, k).ok()??;
            }
            if let Ok(Node::Array(a)) = root(at) {
                if a.is_empty() {
                    return None;
                }
                at = a.bytes(0).ok()?;
            }
            for k in &each.keys {
                let Ok(Node::Object(_)) = root(at) else {
                    return None;
                };
                at = crate::json::ops::field(at, k).ok()??;
            }
            // The last field's array is its elements (as written).
            if !each.keys.is_empty()
                && let Ok(Node::Array(a)) = root(at)
            {
                if a.is_empty() {
                    return None;
                }
                at = a.bytes(0).ok()?;
            }
            if matches!(root(at).ok()?, Node::Array(_) | Node::Object(_)) {
                return None;
            }
            let text = each.text || each.kind == (Kind::Json { text: true });
            Some(Expr::Literal(if text {
                Value::Text(crate::json::ops::as_text(at).ok()??)
            } else {
                crate::json::value(at.to_vec())
            }))
        }
        // `array @> c` / `c <@ array`, `c` a constant array: an element of
        // `c` (the rows holding all of them hold it; with none, every row
        // holds them — no bound).
        Expr::Call(f, args)
            if each.kind == Kind::Array
                && args.len() == 2
                && matches!(f.name(), "arraycontains" | "arraycontained") =>
        {
            let (x, c) = if f.name() == "arraycontains" {
                (&args[0], &args[1])
            } else {
                (&args[1], &args[0])
            };
            if *x != *array {
                return None;
            }
            let element = |v: &Value| !matches!(v, Value::Null | Value::Array(_));
            match c {
                Expr::Literal(Value::Array(items)) => items
                    .iter()
                    .find(|v| element(v))
                    .cloned()
                    .map(Expr::Literal),
                Expr::Array(items) => items.iter().find_map(|e| match e {
                    Expr::Literal(v) if element(v) => Some(Expr::Literal(v.clone())),
                    _ => None,
                }),
                _ => None,
            }
        }
        Expr::AnyAll {
            left,
            op: crate::ast::BinaryOp::Eq,
            right,
            all: false,
        } if each.kind == Kind::Array && **right == *array => {
            let free = left.columns().is_empty() && !left.has_subquery();
            free.then(|| (**left).clone())
        }
        _ => None,
    }
}

/// The keys `array` is below `x` by (`x -> 'k1' -> … -> 'kn'`, as bound:
/// steps or one path call), if it is.
fn below(array: &Expr, x: &Expr) -> Option<Vec<String>> {
    if array == x {
        return Some(Vec::new());
    }
    let Expr::Call(f, args) = array else {
        return None;
    };
    let text = |e: &Expr| match e {
        Expr::Literal(Value::Text(k)) => Some(k.clone()),
        _ => None,
    };
    match f.name() {
        "->(json,text)" if args.len() == 2 => {
            let mut keys = below(&args[0], x)?;
            keys.push(text(&args[1])?);
            Some(keys)
        }
        "->(json,path)" => {
            let mut keys = below(&args[0], x)?;
            for k in &args[1..] {
                keys.push(text(k)?);
            }
            Some(keys)
        }
        _ => None,
    }
}
