//! `json`'s operators, functions, table functions and aggregates (PLAN.md
//! §M14.15 J2): PostgreSQL's, under `json_*` names with the `jsonb_*`
//! ones accepted beside them.  What they do is in [`super::ops`].

use std::ops::RangeInclusive;
use std::sync::Arc;

use super::ops;
use crate::error::{Error, Result};
use crate::ext::{
    Accumulator, Aggregate, Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction,
    Signature, TableFunction, TableSignature, Volatility,
};
use crate::types::{DataType, Row, Value};

fn json() -> DataType {
    super::data_type()
}

fn text_array() -> DataType {
    DataType::Text.array_of().expect("text[]")
}

/// A `json` argument's bytes.
fn jb(v: &Value) -> Result<&[u8]> {
    super::bytes(v)
}

fn text(v: &Value) -> Result<&str> {
    match v {
        Value::Text(s) => Ok(s),
        _ => Err(Error::internal("not a text value")),
    }
}

fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        _ => Err(Error::internal("not an integer value")),
    }
}

fn boolean(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(Error::internal("not a boolean value")),
    }
}

/// A `text[]` argument: its elements, `NULL` as `None`.
fn texts(v: &Value) -> Result<Vec<Option<String>>> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|x| match x {
                Value::Null => Ok(None),
                Value::Text(s) => Ok(Some(s.clone())),
                _ => Err(Error::internal("not a text[] value")),
            })
            .collect(),
        _ => Err(Error::internal("not a text[] value")),
    }
}

fn json_value(b: Vec<u8>) -> Value {
    super::value(b)
}

fn maybe_json(b: Option<&[u8]>) -> Value {
    b.map_or(Value::Null, |b| json_value(b.to_vec()))
}

fn maybe_text(b: Option<&[u8]>) -> Result<Value> {
    Ok(match b {
        Some(b) => ops::as_text(b)?.map_or(Value::Null, Value::Text),
        None => Value::Null,
    })
}

pub(super) type Body = fn(&[Value]) -> Result<Value>;

/// A function: its arguments' types (the last repeated for a variadic
/// one), its result's, and what it computes.
pub(super) struct Func {
    name: &'static str,
    arity: RangeInclusive<usize>,
    args: Vec<Arg>,
    returns: DataType,
    strict: bool,
    volatility: Volatility,
    body: Body,
}

impl Func {
    pub(super) fn new(name: &'static str, args: Vec<Arg>, returns: DataType, body: Body) -> Self {
        let n = args.len();
        Self {
            name,
            arity: n..=n,
            args,
            returns,
            strict: true,
            volatility: Volatility::Immutable,
            body,
        }
    }

    pub(super) fn arity(mut self, arity: RangeInclusive<usize>) -> Self {
        self.arity = arity;
        self
    }

    fn lax(mut self) -> Self {
        self.strict = false;
        self
    }

    pub(super) fn stable(mut self) -> Self {
        self.volatility = Volatility::Stable;
        self
    }
}

impl ScalarFunction for Func {
    fn name(&self) -> &str {
        self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        self.arity.clone()
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let last = self.args.last().copied().unwrap_or(Arg::Keep);
        let mut a: Vec<Arg> = self.args.iter().copied().take(args.len()).collect();
        a.resize(args.len(), last);
        Ok(Signature::new(a, Returns::Type(Some(self.returns))))
    }
    fn strict(&self) -> bool {
        self.strict
    }
    fn volatility(&self) -> Volatility {
        self.volatility
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        (self.body)(args)
    }
}

pub(super) fn u(t: DataType) -> Arg {
    Arg::Untyped(t)
}

// ------------------------------------------------------------- operators

fn operators(r: &mut Registry) -> Result<()> {
    let j = json();
    let ints = [DataType::SmallInt, DataType::Integer, DataType::BigInt];
    let op =
        |name, right, returns, body| Arc::new(Func::new(name, vec![u(j), u(right)], returns, body));
    r.add_operator(
        "->",
        j,
        DataType::Text,
        op("->(json,text)", DataType::Text, j, |a| {
            Ok(maybe_json(ops::field(jb(&a[0])?, text(&a[1])?)?))
        }),
    )?;
    r.add_operator(
        "->>",
        j,
        DataType::Text,
        op("->>(json,text)", DataType::Text, DataType::Text, |a| {
            maybe_text(ops::field(jb(&a[0])?, text(&a[1])?)?)
        }),
    )?;
    for t in ints {
        r.add_operator(
            "->",
            j,
            t,
            op("->(json,integer)", t, j, |a| {
                Ok(maybe_json(ops::element(jb(&a[0])?, int(&a[1])?)?))
            }),
        )?;
        r.add_operator(
            "->>",
            j,
            t,
            op("->>(json,integer)", t, DataType::Text, |a| {
                maybe_text(ops::element(jb(&a[0])?, int(&a[1])?)?)
            }),
        )?;
    }
    r.add_operator(
        "#>",
        j,
        text_array(),
        op("#>(json,text[])", text_array(), j, |a| {
            Ok(maybe_json(ops::at_path(jb(&a[0])?, &texts(&a[1])?)?))
        }),
    )?;
    r.add_operator(
        "#>>",
        j,
        text_array(),
        op("#>>(json,text[])", text_array(), DataType::Text, |a| {
            maybe_text(ops::at_path(jb(&a[0])?, &texts(&a[1])?)?)
        }),
    )?;
    r.add_operator(
        "@>",
        j,
        j,
        op("@>(json,json)", j, DataType::Bool, |a| {
            Ok(Value::Bool(ops::contains(jb(&a[0])?, jb(&a[1])?)?))
        }),
    )?;
    r.add_operator(
        "<@",
        j,
        j,
        op("<@(json,json)", j, DataType::Bool, |a| {
            Ok(Value::Bool(ops::contains(jb(&a[1])?, jb(&a[0])?)?))
        }),
    )?;
    r.add_operator(
        "?",
        j,
        DataType::Text,
        op("?(json,text)", DataType::Text, DataType::Bool, |a| {
            Ok(Value::Bool(ops::exists(jb(&a[0])?, text(&a[1])?)?))
        }),
    )?;
    r.add_operator(
        "?|",
        j,
        text_array(),
        op("?|(json,text[])", text_array(), DataType::Bool, |a| {
            let b = jb(&a[0])?;
            for k in texts(&a[1])?.into_iter().flatten() {
                if ops::exists(b, &k)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }),
    )?;
    r.add_operator(
        "?&",
        j,
        text_array(),
        op("?&(json,text[])", text_array(), DataType::Bool, |a| {
            let b = jb(&a[0])?;
            for k in texts(&a[1])?.into_iter().flatten() {
                if !ops::exists(b, &k)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }),
    )?;
    r.add_operator(
        "#-",
        j,
        text_array(),
        op("#-(json,text[])", text_array(), j, |a| {
            Ok(json_value(ops::delete_path(jb(&a[0])?, &texts(&a[1])?)?))
        }),
    )?;
    Ok(())
}

/// `||` and `-`, the grammar's own operators: the type's arithmetic.
pub(crate) struct Arith;

impl crate::ext::Arithmetic for Arith {
    fn operators(&self) -> &[&str] {
        &["||", "-"]
    }

    fn other_operand(&self, op: &str, b: Option<DataType>) -> Option<DataType> {
        if op != "-" {
            return None;
        }
        match b {
            None | Some(DataType::Text) => Some(DataType::Text),
            Some(DataType::SmallInt | DataType::Integer | DataType::BigInt) => b,
            Some(t) if t == text_array() => Some(t),
            _ => None,
        }
    }

    fn apply(&self, op: &str, a: &Value, b: &Value) -> Result<Value> {
        let x = jb(a)?;
        Ok(json_value(match (op, b) {
            ("||", b) => ops::concat(x, jb(b)?)?,
            ("-", Value::Text(k)) => ops::delete_keys(x, &[k.as_str()])?,
            ("-", Value::Int(i)) => ops::delete_index(x, *i)?,
            ("-", Value::Array(_)) => {
                let keys = texts(b)?;
                let keys: Vec<&str> = keys.iter().flatten().map(String::as_str).collect();
                ops::delete_keys(x, &keys)?
            }
            _ => {
                return Err(Error::undefined_function(format!(
                    "operator does not exist: json {op} json"
                )));
            }
        }))
    }
}

// ------------------------------------------------------------- functions

fn kind(a: &[Value]) -> Result<Value> {
    Ok(Value::Text(
        super::kind(&super::form::root(jb(&a[0])?)?).to_string(),
    ))
}

fn array_length(a: &[Value]) -> Result<Value> {
    match super::form::root(jb(&a[0])?)? {
        super::Node::Array(x) => Ok(Value::Int(x.len() as i64)),
        super::Node::Object(_) => Err(Error::invalid_parameter(
            "cannot get array length of a non-array",
        )),
        _ => Err(Error::invalid_parameter(
            "cannot get array length of a scalar",
        )),
    }
}

fn build_object(a: &[Value]) -> Result<Value> {
    if a.len() % 2 == 1 {
        return Err(
            Error::invalid_parameter("argument list must have even number of elements").with_hint(
                "The arguments of json_build_object() must consist of alternating keys and values.",
            ),
        );
    }
    let mut pairs = Vec::with_capacity(a.len() / 2);
    for (i, kv) in a.chunks(2).enumerate() {
        if matches!(kv[0], Value::Null) {
            return Err(Error::invalid_parameter(format!(
                "argument {}: key must not be null",
                2 * i + 1
            )));
        }
        pairs.push((ops::key_text(&kv[0])?, ops::from_value(&kv[1])?));
    }
    Ok(json_value(ops::object_of(pairs)?))
}

fn build_array(a: &[Value]) -> Result<Value> {
    let items: Vec<Vec<u8>> = a.iter().map(ops::from_value).collect::<Result<_>>()?;
    Ok(json_value(ops::array_of(&items)?))
}

fn object_from_texts(a: &[Value]) -> Result<Value> {
    let (keys, values) = if a.len() == 1 {
        let all = texts(&a[0])?;
        if all.len() % 2 == 1 {
            return Err(Error::array_subscript(
                "array must have even number of elements",
            ));
        }
        let (k, v): (Vec<_>, Vec<_>) = all.chunks(2).map(|p| (p[0].clone(), p[1].clone())).unzip();
        (k, v)
    } else {
        let (k, v) = (texts(&a[0])?, texts(&a[1])?);
        if k.len() != v.len() {
            return Err(Error::array_subscript("mismatched array dimensions"));
        }
        (k, v)
    };
    let mut pairs = Vec::with_capacity(keys.len());
    for (k, v) in keys.into_iter().zip(values) {
        let k = k.ok_or_else(|| Error::null_value("null value not allowed for object key"))?;
        let v = match v {
            Some(s) => ops::string(&s),
            None => vec![0],
        };
        pairs.push((k, v));
    }
    Ok(json_value(ops::object_of(pairs)?))
}

fn set(a: &[Value]) -> Result<Value> {
    let create = a.get(3).map_or(Ok(true), boolean)?;
    Ok(json_value(ops::set(
        jb(&a[0])?,
        &texts(&a[1])?,
        jb(&a[2])?,
        create,
    )?))
}

fn insert(a: &[Value]) -> Result<Value> {
    let after = a.get(3).map_or(Ok(false), boolean)?;
    Ok(json_value(ops::insert(
        jb(&a[0])?,
        &texts(&a[1])?,
        jb(&a[2])?,
        after,
    )?))
}

fn path_of(a: &[Value]) -> Result<Vec<Option<String>>> {
    a[1..]
        .iter()
        .map(|v| match v {
            Value::Null => Ok(None),
            v => text(v).map(|s| Some(s.to_string())),
        })
        .collect()
}

fn functions(r: &mut Registry) -> Result<()> {
    let j = json();
    let t = DataType::Text;
    let add = |r: &mut Registry, names: &[&str], f: Func| r.add_function(names, Arc::new(f));
    add(
        r,
        &["json_typeof", "jsonb_typeof"],
        Func::new("json_typeof", vec![u(j)], t, kind),
    )?;
    add(
        r,
        &["json_array_length", "jsonb_array_length"],
        Func::new(
            "json_array_length",
            vec![u(j)],
            DataType::Integer,
            array_length,
        ),
    )?;
    add(
        r,
        &["json_pretty", "jsonb_pretty"],
        Func::new("json_pretty", vec![u(j)], t, |a| {
            Ok(Value::Text(super::to_pretty(jb(&a[0])?)?))
        }),
    )?;
    add(
        r,
        &["json_strip_nulls", "jsonb_strip_nulls"],
        Func::new("json_strip_nulls", vec![u(j), u(DataType::Bool)], j, |a| {
            let arrays = a.get(1).map_or(Ok(false), boolean)?;
            Ok(json_value(ops::strip_nulls(jb(&a[0])?, arrays)?))
        })
        .arity(1..=2),
    )?;
    add(
        r,
        &["to_json", "to_jsonb"],
        Func::new("to_json", vec![Arg::Keep], j, |a| {
            Ok(json_value(ops::from_value(&a[0])?))
        })
        .stable(),
    )?;
    add(
        r,
        &["json_build_object", "jsonb_build_object"],
        Func::new("json_build_object", vec![Arg::Keep], j, build_object)
            .arity(0..=100)
            .lax()
            .stable(),
    )?;
    add(
        r,
        &["json_build_array", "jsonb_build_array"],
        Func::new("json_build_array", vec![Arg::Keep], j, build_array)
            .arity(0..=100)
            .lax()
            .stable(),
    )?;
    add(
        r,
        &["json_object", "jsonb_object"],
        Func::new(
            "json_object",
            vec![u(text_array()), u(text_array())],
            j,
            object_from_texts,
        )
        .arity(1..=2),
    )?;
    add(
        r,
        &["json_set", "jsonb_set"],
        Func::new(
            "json_set",
            vec![u(j), u(text_array()), u(j), u(DataType::Bool)],
            j,
            set,
        )
        .arity(3..=4),
    )?;
    add(
        r,
        &["json_insert", "jsonb_insert"],
        Func::new(
            "json_insert",
            vec![u(j), u(text_array()), u(j), u(DataType::Bool)],
            j,
            insert,
        )
        .arity(3..=4),
    )?;
    add(
        r,
        &["json_extract_path", "jsonb_extract_path"],
        Func::new("json_extract_path", vec![u(j), u(t)], j, |a| {
            Ok(maybe_json(ops::at_path(jb(&a[0])?, &path_of(a)?)?))
        })
        .arity(2..=100)
        .lax(),
    )?;
    add(
        r,
        &["json_extract_path_text", "jsonb_extract_path_text"],
        Func::new("json_extract_path_text", vec![u(j), u(t)], t, |a| {
            maybe_text(ops::at_path(jb(&a[0])?, &path_of(a)?)?)
        })
        .arity(2..=100)
        .lax(),
    )?;
    Ok(())
}

// ------------------------------------------------------------- tables

/// `json_each(j)` / `json_each_text(j)`: an object's keys and values.
struct Each {
    name: &'static str,
    text: bool,
}

impl TableFunction for Each {
    fn name(&self) -> &str {
        self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![u(json())],
            columns: vec![
                ("key".into(), Some(DataType::Text)),
                (
                    "value".into(),
                    Some(if self.text { DataType::Text } else { json() }),
                ),
            ],
        })
    }
    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        if matches!(args[0], Value::Null) {
            return Ok(Vec::new());
        }
        match super::form::root(jb(&args[0])?)? {
            super::Node::Object(o) => (0..o.len())
                .map(|i| {
                    let v = o.value_bytes(i)?;
                    Ok(vec![
                        Value::Text(o.key(i)?.to_string()),
                        if self.text {
                            maybe_text(Some(v))?
                        } else {
                            json_value(v.to_vec())
                        },
                    ])
                })
                .collect(),
            _ => Err(Error::invalid_parameter(format!(
                "cannot call {} on a non-object",
                self.name
            ))),
        }
    }
}

/// `json_array_elements(j)` / `json_array_elements_text(j)`.
struct Elements {
    name: &'static str,
    text: bool,
}

impl TableFunction for Elements {
    fn name(&self) -> &str {
        self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![u(json())],
            columns: vec![(
                "value".into(),
                Some(if self.text { DataType::Text } else { json() }),
            )],
        })
    }
    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        if matches!(args[0], Value::Null) {
            return Ok(Vec::new());
        }
        match super::form::root(jb(&args[0])?)? {
            super::Node::Array(a) => (0..a.len())
                .map(|i| {
                    let v = a.bytes(i)?;
                    Ok(vec![if self.text {
                        maybe_text(Some(v))?
                    } else {
                        json_value(v.to_vec())
                    }])
                })
                .collect(),
            super::Node::Object(_) => Err(Error::invalid_parameter(
                "cannot extract elements from an object",
            )),
            _ => Err(Error::invalid_parameter(
                "cannot extract elements from a scalar",
            )),
        }
    }
}

/// `json_object_keys(j)`.
struct Keys {
    name: &'static str,
}

impl TableFunction for Keys {
    fn name(&self) -> &str {
        self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<TableSignature> {
        Ok(TableSignature {
            args: vec![u(json())],
            columns: vec![(self.name.into(), Some(DataType::Text))],
        })
    }
    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        if matches!(args[0], Value::Null) {
            return Ok(Vec::new());
        }
        match super::form::root(jb(&args[0])?)? {
            super::Node::Object(o) => (0..o.len())
                .map(|i| Ok(vec![Value::Text(o.key(i)?.to_string())]))
                .collect(),
            super::Node::Array(_) => Err(Error::invalid_parameter(format!(
                "cannot call {} on an array",
                self.name
            ))),
            _ => Err(Error::invalid_parameter(format!(
                "cannot call {} on a scalar",
                self.name
            ))),
        }
    }
}

// ------------------------------------------------------------- aggregates

/// `json_agg(x)`: the values as an array (`NULL`s as `null`); `NULL` over
/// no rows.
struct Agg;

impl Aggregate for Agg {
    fn name(&self) -> &str {
        "json_agg"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        1..=1
    }
    fn keeps_nulls(&self) -> bool {
        true
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(vec![Arg::Keep], Returns::Type(Some(json()))))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(AggState::default())
    }
}

#[derive(Default)]
struct AggState {
    data: Vec<u8>,
    ends: Vec<usize>,
}

impl Accumulator for AggState {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.data.extend_from_slice(&ops::from_value(&args[0])?);
        self.ends.push(self.data.len());
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        if self.ends.is_empty() {
            return Ok(Value::Null);
        }
        let mut out = Vec::with_capacity(self.data.len() + 4 * self.ends.len() + 8);
        super::form::put_array(&mut out, &self.ends, &self.data)?;
        Ok(json_value(out))
    }
}

/// `json_object_agg(k, v)`: an object of the keys and values (the last
/// of equal keys kept); `NULL` over no rows.
struct ObjectAgg;

impl Aggregate for ObjectAgg {
    fn name(&self) -> &str {
        "json_object_agg"
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn keeps_nulls(&self) -> bool {
        true
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Keep, Arg::Keep],
            Returns::Type(Some(json())),
        ))
    }
    fn start(&self) -> Box<dyn Accumulator> {
        Box::new(ObjectAggState::default())
    }
}

#[derive(Default)]
struct ObjectAggState {
    data: Vec<u8>,
    pairs: Vec<(String, std::ops::Range<usize>)>,
}

impl Accumulator for ObjectAggState {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if matches!(args[0], Value::Null) {
            return Err(Error::invalid_parameter("field name must not be null"));
        }
        let k = ops::key_text(&args[0])?;
        let start = self.data.len();
        self.data.extend_from_slice(&ops::from_value(&args[1])?);
        self.pairs.push((k, start..self.data.len()));
        Ok(())
    }
    fn finish(&mut self) -> Result<Value> {
        if self.pairs.is_empty() {
            return Ok(Value::Null);
        }
        let mut out = Vec::with_capacity(self.data.len() + 8 * self.pairs.len() + 8);
        super::form::put_object(&mut out, &mut self.pairs, &self.data)?;
        Ok(json_value(out))
    }
}

/// Registers the operators and functions (the type is registered).
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    operators(r)?;
    functions(r)?;
    for (name, text) in [
        ("json_each", false),
        ("jsonb_each", false),
        ("json_each_text", true),
        ("jsonb_each_text", true),
    ] {
        r.add_table_function(Arc::new(Each { name, text }))?;
    }
    for (name, text) in [
        ("json_array_elements", false),
        ("jsonb_array_elements", false),
        ("json_array_elements_text", true),
        ("jsonb_array_elements_text", true),
    ] {
        r.add_table_function(Arc::new(Elements { name, text }))?;
    }
    for name in ["json_object_keys", "jsonb_object_keys"] {
        r.add_table_function(Arc::new(Keys { name }))?;
    }
    r.add_aggregate(&["json_agg", "jsonb_agg"], Arc::new(Agg))?;
    r.add_aggregate(
        &["json_object_agg", "jsonb_object_agg"],
        Arc::new(ObjectAgg),
    )?;
    Ok(())
}
