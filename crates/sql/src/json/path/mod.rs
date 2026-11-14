//! The SQL/JSON path language (PLAN.md §M14.15 J3a): PostgreSQL's
//! `jsonpath` — its grammar, its normalized text, its lax and strict
//! modes, item methods, filters, `like_regex`, datetimes — and the
//! functions over it (`json_path_query` and kin, `jsonb_path_*` beside
//! them; `@?`, `@@`).
//!
//! A `jsonpath` value is its normalized text ([`print`]); a function
//! reads it back through a small per-thread cache of parsed paths, so a
//! query running one path over many rows parses it once.

pub(crate) mod ast;
pub(crate) mod datetime;
pub(crate) mod eval;
mod parse;
mod print;

use std::cell::RefCell;
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::sync::Arc;

pub(crate) use ast::Path;

use super::funcs::{Func, u};
use crate::error::{Error, Result};
use crate::ext::{
    ArgInfo, CallContext, CanonicalBytes, Equality, Hashing, Registry, TableFunction,
    TableSignature, Type, TypeRef,
};
use crate::types::{DataType, Row, Value};
use eval::{Fail, St};

/// Parses a path.
pub(crate) fn parse(text: &str) -> Result<Path> {
    parse::parse(text)
}

/// A path's normalized text (as `'…'::jsonpath` prints it).
pub fn normalize(text: &str) -> Result<String> {
    Ok(parse(text)?.to_string())
}

/// The items `path` finds in the json value `doc` (binary form), each in
/// binary form; `silent` hides the errors PostgreSQL's `silent` hides
/// (and returns what was found before one).
pub fn query(doc: &[u8], path: &str, silent: bool) -> Result<Vec<Vec<u8>>> {
    let path = Rc::new(parse(path)?);
    let c = Call {
        path,
        doc,
        vars: None,
        silent,
    };
    Ok(items(&c, false)?.0)
}

/// The `jsonpath` type.
pub struct JsonPath;

/// The interned `jsonpath` type.
pub fn path_type() -> TypeRef {
    TypeRef::named("jsonpath").expect("the built-in module registers jsonpath")
}

/// The `jsonpath` column type.
pub fn data_type() -> DataType {
    DataType::Ext(path_type())
}

fn path_bytes(v: &Value) -> Result<&[u8]> {
    match v {
        Value::Ext(t, b) if t.name() == "jsonpath" => Ok(b),
        _ => Err(Error::internal("not a jsonpath value")),
    }
}

impl Type for JsonPath {
    fn name(&self) -> &str {
        "jsonpath"
    }
    fn category(&self) -> &str {
        "jsonpath"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let p = parse(text)?;
        Ok(Value::ext(path_type(), p.to_string().into_bytes()))
    }
    fn output(&self, v: &Value) -> String {
        path_bytes(v)
            .ok()
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("?")
            .to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn wire_output(&self, v: &Value) -> Option<Vec<u8>> {
        Some(self.output(v).into_bytes())
    }
    fn wire_input(&self, b: &[u8]) -> Option<Result<Value>> {
        Some(match std::str::from_utf8(b) {
            Ok(s) => self.input(s),
            Err(_) => Err(Error::invalid_text(
                "invalid input syntax for type jsonpath: not UTF-8",
            )),
        })
    }
    fn examples(&self) -> Vec<Value> {
        [
            "$",
            "$.a",
            "strict $.a[*] ? (@ > 1)",
            "$.**{2 to last}.b",
            "$ ? (@.x like_regex \"^a\" flag \"i\")",
            "($.a + 1) * 2",
            "$.datetime(\"YYYY-MM-DD\").type()",
        ]
        .iter()
        .map(|s| self.input(s).expect("valid"))
        .collect()
    }
}

thread_local! {
    /// Paths parsed lately, by their text.
    static CACHE: RefCell<Vec<(Box<[u8]>, Rc<Path>)>> = const { RefCell::new(Vec::new()) };
}

/// A `jsonpath` value, parsed (from the cache when it was lately).
pub(crate) fn compiled(v: &Value) -> Result<Rc<Path>> {
    let b = path_bytes(v)?;
    if let Some(p) = CACHE.with(|c| {
        c.borrow()
            .iter()
            .find(|(k, _)| **k == *b)
            .map(|(_, p)| p.clone())
    }) {
        return Ok(p);
    }
    let text = std::str::from_utf8(b).map_err(|_| Error::internal("a jsonpath not UTF-8"))?;
    let p = Rc::new(parse(text)?);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= 16 {
            c.remove(0);
        }
        c.push((b.into(), p.clone()));
    });
    Ok(p)
}

/// What a call asks: the path, the document, the variables, silence.
struct Call<'v> {
    path: Rc<Path>,
    doc: &'v [u8],
    vars: Option<&'v [u8]>,
    silent: bool,
}

fn call(a: &[Value]) -> Result<Call<'_>> {
    Ok(Call {
        doc: super::bytes(&a[0])?,
        path: compiled(&a[1])?,
        vars: a.get(2).map(super::bytes).transpose()?,
        silent: matches!(a.get(3), Some(Value::Bool(true))),
    })
}

/// An operator's call: no variables, silent.
fn operator_call(a: &[Value]) -> Result<Call<'_>> {
    Ok(Call {
        doc: super::bytes(&a[0])?,
        path: compiled(&a[1])?,
        vars: None,
        silent: true,
    })
}

/// The items a path finds — as far as it got, when it failed silently
/// (as PostgreSQL's functions return them), and whether it succeeded.
fn items(c: &Call<'_>, use_tz: bool) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut found = Vec::new();
    let r = eval::run(&c.path, c.doc, c.vars, use_tz, Some(&mut found));
    let items = found.iter().map(eval::Item::to_json).collect();
    match r {
        Ok(_) => Ok((items, true)),
        Err(Fail::Soft(_)) if c.silent => Ok((items, false)),
        Err(Fail::Soft(e) | Fail::Hard(e)) => Err(e),
    }
}

/// `json_path_exists`: whether the path finds anything (`NULL` on a
/// silent failure).
pub(crate) fn exists(a: &[Value], use_tz: bool) -> Result<Value> {
    exists_of(&call(a)?, use_tz)
}

fn exists_of(c: &Call<'_>, use_tz: bool) -> Result<Value> {
    match eval::run(&c.path, c.doc, c.vars, use_tz, None) {
        Ok(st) => Ok(Value::Bool(st == St::Found)),
        Err(Fail::Soft(_)) if c.silent => Ok(Value::Null),
        Err(Fail::Soft(e) | Fail::Hard(e)) => Err(e),
    }
}

/// `json_path_match`: the predicate's value (`NULL` when unknown).
pub(crate) fn matches(a: &[Value], use_tz: bool) -> Result<Value> {
    matches_of(&call(a)?, use_tz)
}

fn matches_of(c: &Call<'_>, use_tz: bool) -> Result<Value> {
    let (items, _) = items(c, use_tz)?;
    if let [one] = items.as_slice() {
        match super::form::root(one)? {
            super::Node::Bool(b) => return Ok(Value::Bool(b)),
            super::Node::Null => return Ok(Value::Null),
            _ => {}
        }
    }
    if c.silent {
        return Ok(Value::Null);
    }
    Err(Error::json_singleton_required(
        "single boolean result is expected",
    ))
}

fn query_array(a: &[Value], use_tz: bool) -> Result<Value> {
    let (items, _) = items(&call(a)?, use_tz)?;
    Ok(super::value(super::ops::array_of(&items)?))
}

fn query_first(a: &[Value], use_tz: bool) -> Result<Value> {
    let (items, _) = items(&call(a)?, use_tz)?;
    Ok(items.into_iter().next().map_or(Value::Null, super::value))
}

/// `json_path_query`: the items, a row each.
struct Query {
    name: &'static str,
    tz: bool,
}

impl TableFunction for Query {
    fn name(&self) -> &str {
        self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=4
    }
    fn resolve(&self, args: &[ArgInfo]) -> Result<TableSignature> {
        let mut a = vec![
            u(super::data_type()),
            u(data_type()),
            u(super::data_type()),
            u(DataType::Bool),
        ];
        a.truncate(args.len());
        Ok(TableSignature {
            args: a,
            columns: vec![(self.name.into(), Some(super::data_type()))],
        })
    }
    fn rows(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>> {
        if args.iter().any(|v| matches!(v, Value::Null)) {
            return Ok(Vec::new());
        }
        let (items, _) = items(&call(args)?, self.tz)?;
        Ok(items.into_iter().map(|b| vec![super::value(b)]).collect())
    }
}

/// Registers the type, the functions and the operators.
pub(crate) fn register(r: &mut Registry) -> Result<()> {
    r.add_type(Arc::new(JsonPath))?;
    let (j, p, b) = (super::data_type(), data_type(), DataType::Bool);
    let args = || vec![u(j), u(p), u(j), u(b)];
    type F = fn(&[Value]) -> Result<Value>;
    // The names: json_path_x, its jsonb_ alias, and the _tz pair.
    let fns: [([&str; 4], DataType, F, F); 4] = [
        (
            [
                "json_path_exists",
                "jsonb_path_exists",
                "json_path_exists_tz",
                "jsonb_path_exists_tz",
            ],
            b,
            |a| exists(a, false),
            |a| exists(a, true),
        ),
        (
            [
                "json_path_match",
                "jsonb_path_match",
                "json_path_match_tz",
                "jsonb_path_match_tz",
            ],
            b,
            |a| matches(a, false),
            |a| matches(a, true),
        ),
        (
            [
                "json_path_query_array",
                "jsonb_path_query_array",
                "json_path_query_array_tz",
                "jsonb_path_query_array_tz",
            ],
            j,
            |a| query_array(a, false),
            |a| query_array(a, true),
        ),
        (
            [
                "json_path_query_first",
                "jsonb_path_query_first",
                "json_path_query_first_tz",
                "jsonb_path_query_first_tz",
            ],
            j,
            |a| query_first(a, false),
            |a| query_first(a, true),
        ),
    ];
    for (names, returns, plain, tz) in fns {
        r.add_function(
            &names[..2],
            Arc::new(Func::new(names[0], args(), returns, plain).arity(2..=4)),
        )?;
        r.add_function(
            &names[2..],
            Arc::new(
                Func::new(names[2], args(), returns, tz)
                    .arity(2..=4)
                    .stable(),
            ),
        )?;
    }
    for (name, tz) in [
        ("json_path_query", false),
        ("jsonb_path_query", false),
        ("json_path_query_tz", true),
        ("jsonb_path_query_tz", true),
    ] {
        r.add_table_function(Arc::new(Query { name, tz }))?;
    }
    r.add_operator(
        "@?",
        j,
        p,
        Arc::new(Func::new("@?(json,jsonpath)", vec![u(j), u(p)], b, |a| {
            exists_of(&operator_call(a)?, false)
        })),
    )?;
    r.add_operator(
        "@@",
        j,
        p,
        Arc::new(Func::new("@@(json,jsonpath)", vec![u(j), u(p)], b, |a| {
            matches_of(&operator_call(a)?, false)
        })),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
