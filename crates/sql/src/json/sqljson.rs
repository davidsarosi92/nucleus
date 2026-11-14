//! SQL/JSON's query functions, predicate and constructors (PLAN.md
//! §M14.15 J3b): `JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY`, `IS JSON`,
//! `JSON_OBJECT`, `JSON_ARRAY`, `JSON_OBJECTAGG`, `JSON_ARRAYAGG`, `JSON`.
//!
//! The grammar's keywords are the parser's (`parser.rs`, `sql_json`): it
//! desugars each form into a call of a function here, its options as
//! constant arguments.  A `RETURNING` type comes as a typed `NULL` — the
//! witness — whose type is the call's; when the call is bound,
//! [`ScalarFunction::rewrite`] reads the type (and its modifier) off the
//! bound witness and gives the call specialized to it, which converts the
//! result itself, so that a conversion's error is the `ON ERROR` clause's
//! to handle, as SQL/JSON says.
//!
//! What the functions do is PostgreSQL 18's (`slt/67_json_sql.slt`, checked
//! against it), but for `json` being one type: `json`'s values are
//! canonical, so a constructor's result has its keys sorted and one of
//! equal keys (the last), as `jsonb`'s do.

use std::ops::RangeInclusive;
use std::sync::Arc;

use super::Node;
use super::form;
use super::path::eval::{self, Fail, St};
use crate::error::{Error, Result};
use crate::expr::Expr;
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, FnRef, Registry, Returns, ScalarFunction,
    Signature, TableFunction, TableSignature, Volatility,
};
use crate::types::{DataType, Row, Value};

// The ON EMPTY / ON ERROR behaviors, as the parser writes them.
// 0 is NULL, the default.
const ERROR: i64 = 1;
const DEFAULT: i64 = 2;
const EMPTY_ARRAY: i64 = 3;
const EMPTY_OBJECT: i64 = 4;
const TRUE: i64 = 5;
const FALSE: i64 = 6;

fn json() -> DataType {
    super::data_type()
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn flag(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

// ------------------------------------------------------- the conversion

/// A modifier of the `RETURNING` type, checked as the type's input
/// checks it (an error, where a cast would cut).
#[derive(Clone)]
enum Fit {
    None,
    Varchar(usize),
    Char(usize, FnRef),
    Call(FnRef, Vec<Value>),
}

/// The `RETURNING` type.
#[derive(Clone)]
struct Target {
    ty: DataType,
    fit: Fit,
}

impl Target {
    /// Read off the bound witness: `CAST(NULL AS t)`, maybe inside the
    /// call that fits it to a modifier.
    fn of(w: &Expr) -> Option<Target> {
        match w {
            Expr::Cast(_, ty) => Some(Target {
                ty: *ty,
                fit: Fit::None,
            }),
            Expr::Call(f, a) => {
                let Some(Expr::Cast(_, ty)) = a.first() else {
                    return None;
                };
                let lits = a[1..]
                    .iter()
                    .map(|e| match e {
                        Expr::Literal(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect::<Option<Vec<Value>>>()?;
                let n = |v: Option<&Value>| match v {
                    Some(Value::Int(i)) => usize::try_from(*i).ok(),
                    _ => None,
                };
                let fit = match f.name() {
                    "substr" => Fit::Varchar(n(lits.get(1))?),
                    "bpchar" => Fit::Char(n(lits.first())?, f.clone()),
                    _ => Fit::Call(f.clone(), lits),
                };
                Some(Target { ty: *ty, fit })
            }
            _ => None,
        }
    }

    /// A result — a JSON value, or a scalar's text — as this type.
    fn coerce(&self, cx: &CallContext<'_>, out: Out<'_>) -> Result<Value> {
        if self.ty == json() {
            return match out {
                Out::Json(b) => Ok(super::value(b.to_vec())),
                Out::Text(s) => super::from_text(&s),
            };
        }
        let s = match out {
            Out::Json(b) => super::to_text(b)?,
            Out::Text(s) => s,
        };
        let v = match self.ty {
            DataType::Ext(_) => crate::builtin::type_of(self.ty).input(&s)?,
            ty => crate::enums::cast(cx.txn(), Value::Text(s), ty)?,
        };
        match &self.fit {
            Fit::None => Ok(v),
            Fit::Varchar(n) => match v {
                Value::Text(s) if s.chars().count() > *n => {
                    if s.chars().skip(*n).all(|c| c == ' ') {
                        Ok(Value::Text(s.chars().take(*n).collect()))
                    } else {
                        Err(Error::string_too_long(format!(
                            "value too long for type character varying({n})"
                        )))
                    }
                }
                v => Ok(v),
            },
            Fit::Char(n, f) => {
                if let Value::Text(s) | Value::Char(s) = &v
                    && s.trim_end_matches(' ').chars().count() > *n
                {
                    return Err(Error::string_too_long(format!(
                        "value too long for type character({n})"
                    )));
                }
                f.call(cx, &[v, Value::Int(*n as i64)])
            }
            Fit::Call(f, lits) => {
                let mut a = Vec::with_capacity(lits.len() + 1);
                a.push(v);
                a.extend(lits.iter().cloned());
                f.call(cx, &a)
            }
        }
    }
}

/// What a query function converts: a JSON value, or text (a scalar's,
/// a string's without its quotes).
enum Out<'a> {
    Json(&'a [u8]),
    Text(String),
}

/// A JSON scalar's text, as `JSON_VALUE` gives it (a boolean `t`/`f`,
/// as PostgreSQL's).
fn scalar_text(n: &Node<'_>) -> Result<String> {
    Ok(match n {
        Node::Str(s) => form::text_of(s)?.to_string(),
        Node::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Node::Int(i) => i.to_string(),
        n => match super::to_numeric(n) {
            Some(d) => Value::Numeric(d?).render(),
            None => return Err(Error::internal("not a JSON scalar")),
        },
    })
}

// ------------------------------------------------- the query functions

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exists,
    Value,
    Query,
}

/// `JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY`: unspecialized as
/// registered, specialized to the `RETURNING` type when bound.
struct Query {
    kind: Kind,
    target: Option<Target>,
}

impl Query {
    /// The items the path finds, or its failure.
    fn items(a: &[Value]) -> std::result::Result<Vec<Vec<u8>>, Fail> {
        Self::with_items(a, |found| found.iter().map(eval::Item::to_json).collect())
    }

    /// `f` of the items the path finds (borrowed from the document where
    /// they are its parts), or the path's failure.
    fn with_items<R>(
        a: &[Value],
        f: impl FnOnce(&[eval::Item<'_>]) -> R,
    ) -> std::result::Result<R, Fail> {
        let path = super::path::compiled(&a[1])?;
        let doc = super::bytes(&a[0])?;
        let vars = match &a[2] {
            Value::Null => None,
            v => Some(super::bytes(v)?),
        };
        let mut found = Vec::new();
        eval::run(&path, doc, vars, false, Some(&mut found))?;
        Ok(f(&found))
    }

    /// What an `ON EMPTY` / `ON ERROR` clause gives for `e`.
    fn behave(
        t: &Target,
        cx: &CallContext<'_>,
        kind: &Value,
        default: &Value,
        e: Error,
    ) -> Result<Value> {
        match int(kind) {
            ERROR => Err(e),
            DEFAULT => Ok(default.clone()),
            EMPTY_ARRAY => t.coerce(cx, Out::Text("[]".into())),
            EMPTY_OBJECT => t.coerce(cx, Out::Text("{}".into())),
            _ => Ok(Value::Null),
        }
    }

    fn exists(a: &[Value]) -> Result<Value> {
        let path = super::path::compiled(&a[1])?;
        let doc = super::bytes(&a[0])?;
        let vars = match &a[2] {
            Value::Null => None,
            v => Some(super::bytes(v)?),
        };
        match eval::run(&path, doc, vars, false, None) {
            Ok(st) => Ok(Value::Bool(st == St::Found)),
            Err(Fail::Hard(e)) => Err(e),
            Err(Fail::Soft(e)) => match int(&a[3]) {
                ERROR => Err(e),
                TRUE => Ok(Value::Bool(true)),
                FALSE => Ok(Value::Bool(false)),
                _ => Ok(Value::Null),
            },
        }
    }

    fn value(t: &Target, cx: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        let (on_empty, on_error) = ((&a[4], &a[5]), (&a[6], &a[7]));
        let fail = |e: Error| Self::behave(t, cx, on_error.0, on_error.1, e);
        // The one item — an integer read where it lies in the document
        // (an integer type's commonest case, no copy), else copied out.
        let fast = matches!(t.fit, Fit::None)
            && matches!(
                t.ty,
                DataType::BigInt | DataType::Integer | DataType::SmallInt
            );
        let one = Self::with_items(a, |found| match found {
            [] => Ok(None),
            [eval::Item::J(b)] => {
                if fast && let Ok(Node::Int(i)) = form::root(b.get()) {
                    return Ok(Some(Err(i)));
                }
                Ok(Some(Ok(b.get().to_vec())))
            }
            [d] => Ok(Some(Ok(d.to_json()))),
            _ => Err(()),
        });
        let one = match one {
            Ok(Ok(Some(Err(i)))) => {
                let fits = match t.ty {
                    DataType::Integer => i32::try_from(i).is_ok(),
                    DataType::SmallInt => i16::try_from(i).is_ok(),
                    _ => true,
                };
                if fits {
                    return Ok(Value::Int(i));
                }
                // Out of the type's range: its text's conversion and error.
                let mut b = Vec::new();
                form::put_int(&mut b, i);
                b
            }
            Ok(Ok(Some(Ok(one)))) => one,
            Ok(Ok(None)) => {
                let e = Error::json_no_item("no SQL/JSON item found for specified path");
                return Self::behave(t, cx, on_empty.0, on_empty.1, e);
            }
            Ok(Err(())) => {
                return fail(Error::json_more_than_one_item(
                    "JSON path expression in JSON_VALUE must return single item",
                ));
            }
            Err(Fail::Hard(e)) => return Err(e),
            Err(Fail::Soft(e)) => return fail(e),
        };
        let one: &[u8] = &one;
        let out = match form::root(one)? {
            Node::Array(_) | Node::Object(_) => {
                return fail(Error::json_scalar_required(
                    "JSON path expression in JSON_VALUE must return single scalar item",
                ));
            }
            Node::Null => return Ok(Value::Null),
            _ if t.ty == json() => Out::Json(one),
            n => Out::Text(scalar_text(&n)?),
        };
        t.coerce(cx, out).or_else(fail)
    }

    fn query(t: &Target, cx: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        let (wrapper, omit_quotes) = (int(&a[4]), flag(&a[5]));
        let (on_empty, on_error) = ((&a[6], &a[7]), (&a[8], &a[9]));
        let fail = |e: Error| Self::behave(t, cx, on_error.0, on_error.1, e);
        let found = match Self::items(a) {
            Ok(f) => f,
            Err(Fail::Hard(e)) => return Err(e),
            Err(Fail::Soft(e)) => return fail(e),
        };
        if found.is_empty() {
            let e = Error::json_no_item("no SQL/JSON item found for specified path");
            return Self::behave(t, cx, on_empty.0, on_empty.1, e);
        }
        let wrapped;
        let result: &[u8] = match (wrapper, found.as_slice()) {
            (1, _) | (2, [_, _, ..]) => {
                wrapped = super::ops::array_of(&found)?;
                &wrapped
            }
            (_, [one]) => one,
            _ => {
                return fail(
                    Error::json_more_than_one_item(
                        "JSON path expression in JSON_QUERY must return single item when no wrapper is requested",
                    )
                    .with_hint("Use the WITH WRAPPER clause to wrap SQL/JSON items into an array."),
                );
            }
        };
        let out = match form::root(result)? {
            Node::Str(s) if omit_quotes => Out::Text(form::text_of(s)?.to_string()),
            _ => Out::Json(result),
        };
        t.coerce(cx, out).or_else(fail)
    }
}

impl ScalarFunction for Query {
    fn name(&self) -> &str {
        match self.kind {
            Kind::Exists => "json_exists",
            Kind::Value => "json_value",
            Kind::Query => "json_query",
        }
    }
    fn arity(&self) -> RangeInclusive<usize> {
        match self.kind {
            Kind::Exists => 4..=4,
            Kind::Value => 8..=8,
            Kind::Query => 10..=10,
        }
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut a = vec![Arg::Convert(json()), Arg::Convert(super::path::data_type())];
        a.resize(args.len(), Arg::Keep);
        if self.kind == Kind::Exists {
            return Ok(Signature::new(a, Returns::Type(Some(DataType::Bool))));
        }
        let ty = args[3].ty.unwrap_or(if self.kind == Kind::Value {
            DataType::Text
        } else {
            json()
        });
        // The DEFAULT expressions, converted to the result's type.
        let n = args.len();
        a[n - 3] = Arg::Convert(ty);
        a[n - 1] = Arg::Convert(ty);
        Ok(Signature::new(a, Returns::Type(Some(ty))))
    }
    fn strict(&self) -> bool {
        false
    }
    fn volatility(&self) -> Volatility {
        match (&self.target, self.kind) {
            (_, Kind::Exists) => Volatility::Immutable,
            (Some(t), _) if t.ty != DataType::TimestampTz => Volatility::Immutable,
            _ => Volatility::Stable,
        }
    }
    fn rewrite(&self, args: &[Expr]) -> Option<Expr> {
        if self.kind == Kind::Exists || self.target.is_some() {
            return None;
        }
        let target = Target::of(args.get(3)?)?;
        Some(Expr::Call(
            FnRef::new(Arc::new(Query {
                kind: self.kind,
                target: Some(target),
            })),
            args.to_vec(),
        ))
    }
    fn call(&self, cx: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        if matches!(a[0], Value::Null) || matches!(a[1], Value::Null) {
            return Ok(Value::Null);
        }
        if self.kind == Kind::Exists {
            return Self::exists(a);
        }
        let Some(t) = &self.target else {
            return Err(Error::feature(format!(
                "{}() RETURNING this type",
                self.name().to_uppercase()
            )));
        };
        match self.kind {
            Kind::Value => Self::value(t, cx, a),
            _ => Self::query(t, cx, a),
        }
    }
}

// ------------------------------------------------------------ IS JSON

/// `x IS JSON [kind] [WITH UNIQUE KEYS]`: `__is_json(x, kind, unique)`,
/// kind 0 value, 1 array, 2 object, 3 scalar.
struct IsJson;

/// The text or `json` a predicate or `JSON()` reads; an error for any
/// other type.
fn json_input(ty: Option<DataType>, what: &str) -> Result<()> {
    match ty {
        None | Some(DataType::Text | DataType::Char | DataType::Bytea) => Ok(()),
        Some(t) if t == json() => Ok(()),
        Some(t) => Err(Error::type_mismatch(format!(
            "cannot use type {t} in {what}"
        ))),
    }
}

/// A text or `json` value's JSON form; `None` for text that is not JSON
/// (or has a key twice, when `unique`).
fn parsed(v: &Value, unique: bool) -> Result<Option<std::borrow::Cow<'_, [u8]>>> {
    let text = match v {
        Value::Ext(_, b) => return Ok(Some(std::borrow::Cow::Borrowed(b))),
        Value::Text(s) | Value::Char(s) => s.as_str(),
        Value::Bytea(b) => match std::str::from_utf8(b) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        },
        _ => return Err(Error::internal("not a JSON input")),
    };
    Ok(super::parse::parse_checked(text, unique)
        .ok()
        .map(std::borrow::Cow::Owned))
}

impl ScalarFunction for IsJson {
    fn name(&self) -> &str {
        "is_json"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        3..=3
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if !args[0].text_literal {
            json_input(args[0].ty, "IS JSON predicate")?;
        }
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Text), Arg::Keep, Arg::Keep],
            Returns::Type(Some(DataType::Bool)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        let Some(b) = parsed(&a[0], flag(&a[2]))? else {
            return Ok(Value::Bool(false));
        };
        let node = form::root(&b)?;
        Ok(Value::Bool(match int(&a[1]) {
            1 => matches!(node, Node::Array(_)),
            2 => matches!(node, Node::Object(_)),
            3 => !matches!(node, Node::Array(_) | Node::Object(_)),
            _ => true,
        }))
    }
}

/// `JSON(x [WITH UNIQUE KEYS])`: `__json_parse(x, unique)`.
struct Parse;

impl ScalarFunction for Parse {
    fn name(&self) -> &str {
        "json"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        if !args[0].text_literal {
            json_input(args[0].ty, "JSON()")?;
        }
        Ok(Signature::new(
            vec![Arg::Untyped(DataType::Text), Arg::Keep],
            Returns::Type(Some(json())),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        match &a[0] {
            Value::Ext(..) => Ok(a[0].clone()),
            Value::Text(s) | Value::Char(s) => {
                Ok(super::value(super::parse::parse_checked(s, flag(&a[1]))?))
            }
            Value::Bytea(b) => {
                let s = std::str::from_utf8(b).map_err(|_| {
                    Error::invalid_text("invalid input syntax for type json: not UTF-8")
                })?;
                Ok(super::value(super::parse::parse_checked(s, flag(&a[1]))?))
            }
            _ => Err(Error::internal("not a JSON input")),
        }
    }
}

// ------------------------------------------------------- constructors

/// An object of keys and values, `NULL` values left out when `absent`;
/// an error for a key twice when `unique`.
fn object(pairs: &[(&Value, &Value)], absent: bool, unique: bool) -> Result<Vec<u8>> {
    let mut items = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if matches!(k, Value::Null) {
            return Err(Error::null_value("null value not allowed for object key"));
        }
        items.push((super::ops::key_text(k)?, *v));
    }
    // A key twice is refused even when one's value is left out.
    if unique {
        super::parse::unique_keys(items.iter().map(|(k, _)| k.as_str()))?;
    }
    let items = items
        .into_iter()
        .filter(|(_, v)| !(absent && matches!(v, Value::Null)))
        .map(|(k, v)| Ok((k, super::ops::from_value(v)?)))
        .collect::<Result<Vec<_>>>()?;
    super::ops::object_of(items)
}

/// `JSON_OBJECT(k VALUE v, …)`: `__json_object(absent, unique, k, v, …)`.
struct Object;

impl ScalarFunction for Object {
    fn name(&self) -> &str {
        "json_object"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=1000
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::keep(args.len(), Some(json())))
    }
    fn strict(&self) -> bool {
        false
    }
    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }
    fn call(&self, _: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        let pairs: Vec<(&Value, &Value)> = a[2..].chunks(2).map(|p| (&p[0], &p[1])).collect();
        Ok(super::value(object(&pairs, flag(&a[0]), flag(&a[1]))?))
    }
}

/// An array of the values, `NULL`s left out when `absent`.
fn array<'v>(values: impl Iterator<Item = &'v Value>, absent: bool) -> Result<Vec<u8>> {
    let items = values
        .filter(|v| !(absent && matches!(v, Value::Null)))
        .map(super::ops::from_value)
        .collect::<Result<Vec<_>>>()?;
    super::ops::array_of(&items)
}

/// `JSON_ARRAY(v, …)`: `__json_array(absent, v, …)`.
struct Array;

impl ScalarFunction for Array {
    fn name(&self) -> &str {
        "json_array"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1000
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::keep(args.len(), Some(json())))
    }
    fn strict(&self) -> bool {
        false
    }
    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }
    fn call(&self, _: &CallContext<'_>, a: &[Value]) -> Result<Value> {
        Ok(super::value(array(a[1..].iter(), flag(&a[0]))?))
    }
}

/// `JSON_ARRAYAGG(v)`: `__json_arrayagg(v, absent)`.
struct ArrayAgg;

impl Aggregate for ArrayAgg {
    fn name(&self) -> &str {
        "json_arrayagg"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn keeps_nulls(&self) -> bool {
        true
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::keep(2, Some(json())))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(ArrayAggState::default())
    }
}

#[derive(Default)]
struct ArrayAggState {
    rows: usize,
    data: Vec<u8>,
    ends: Vec<usize>,
}

impl Accumulator for ArrayAggState {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.rows += 1;
        if matches!(args[0], Value::Null) && flag(&args[1]) {
            return Ok(());
        }
        self.data
            .extend_from_slice(&super::ops::from_value(&args[0])?);
        self.ends.push(self.data.len());
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        if self.rows == 0 {
            return Ok(Value::Null);
        }
        let mut out = Vec::with_capacity(self.data.len() + 4 * self.ends.len() + 8);
        form::put_array(&mut out, &self.ends, &self.data)?;
        Ok(super::value(out))
    }
}

/// `JSON_OBJECTAGG(k VALUE v)`: `__json_objectagg(k, v, absent, unique)`.
struct ObjectAgg;

impl Aggregate for ObjectAgg {
    fn name(&self) -> &str {
        "json_objectagg"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        4..=4
    }
    fn keeps_nulls(&self) -> bool {
        true
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::keep(4, Some(json())))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(ObjectAggState::default())
    }
}

#[derive(Default)]
struct ObjectAggState {
    pairs: Vec<(Value, Value)>,
    absent: bool,
    unique: bool,
}

impl Accumulator for ObjectAggState {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.absent = flag(&args[2]);
        self.unique = flag(&args[3]);
        if matches!(args[0], Value::Null) {
            return Err(Error::null_value("null value not allowed for object key"));
        }
        self.pairs.push((args[0].clone(), args[1].clone()));
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        if self.pairs.is_empty() {
            return Ok(Value::Null);
        }
        let pairs: Vec<(&Value, &Value)> = self.pairs.iter().map(|(k, v)| (k, v)).collect();
        Ok(super::value(object(&pairs, self.absent, self.unique)?))
    }
}

// ---------------------------------------------------------- JSON_TABLE

/// `JSON_TABLE`'s rows (J3c): `__json_table_rows(ctx, path, vars, error)`,
/// the items the path finds and their positions from 1 (`item`, `ord`).
/// A failure of the path gives no rows, or the error when `error` (`ERROR
/// ON ERROR`); a missing variable is an error always.
struct TableRows;

impl TableFunction for TableRows {
    fn name(&self) -> &str {
        "__json_table_rows"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        4..=4
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![
                Arg::Convert(json()),
                Arg::Convert(super::path::data_type()),
                Arg::Keep,
                Arg::Keep,
            ],
            columns: vec![
                ("item".into(), Some(json())),
                ("ord".into(), Some(DataType::BigInt)),
            ],
        })
    }
    fn rows(&self, _: &CallContext<'_>, a: &[Value]) -> Result<Vec<Row>> {
        if matches!(a[0], Value::Null) || matches!(a[1], Value::Null) {
            return Ok(Vec::new());
        }
        let ty = super::json_type();
        let rows = Query::with_items(a, |found| {
            found
                .iter()
                .zip(1..)
                .map(|(item, i)| {
                    // Straight into the value's shared bytes: one copy.
                    let b: std::sync::Arc<[u8]> = match item {
                        eval::Item::J(b) => b.get().into(),
                        d => d.to_json().into(),
                    };
                    vec![Value::Ext(ty, b), Value::Int(i)]
                })
                .collect()
        });
        match rows {
            Ok(rows) => Ok(rows),
            Err(Fail::Hard(e)) => Err(e),
            Err(Fail::Soft(e)) if flag(&a[3]) => Err(e),
            Err(Fail::Soft(_)) => Ok(Vec::new()),
        }
    }
}

/// Registers the functions and aggregates the parser's SQL/JSON forms
/// call.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    for (name, kind) in [
        ("__json_exists", Kind::Exists),
        ("__json_value", Kind::Value),
        ("__json_query", Kind::Query),
    ] {
        r.add_function(&[name], Arc::new(Query { kind, target: None }))?;
    }
    r.add_function(&["__is_json"], Arc::new(IsJson))?;
    r.add_function(&["__json_parse"], Arc::new(Parse))?;
    r.add_function(&["__json_object"], Arc::new(Object))?;
    r.add_function(&["__json_array"], Arc::new(Array))?;
    r.add_aggregate(&["__json_arrayagg"], Arc::new(ArrayAgg))?;
    r.add_aggregate(&["__json_objectagg"], Arc::new(ObjectAgg))?;
    r.add_table_function(Arc::new(TableRows))?;
    Ok(())
}
