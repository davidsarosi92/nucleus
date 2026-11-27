//! Filter documents (PLAN.md §M16 Q1, D22): a MongoDB-style filter —
//! `{"age": {"$gt": 30}, "tags": "vip"}` — compiled to a lax SQL/JSON
//! path predicate, `$ ? (@.age > 30 && @.tags == "vip")`, which `@?`
//! applies to a document.  Not a second language inside: the path is an
//! ordinary `jsonpath` value, evaluated, printed and planned as any.
//!
//! The filter's matching rules are MongoDB's, and they are lax mode's: a
//! field that is an array matches when it or one of its elements does
//! (accessors and comparisons unwrap arrays); values of different types
//! never compare (`"40"` is not `> 30`: the comparison is unknown, so
//! false); a missing field compares false.  Where the rules go further,
//! the predicate says so explicitly:
//!
//! - `{"f": null}` (and `$eq`, `$in`, `$gte`, `$lte` with `null`) also
//!   matches a document without the field — at any step of the path;
//! - `$ne`, `$nin`, `$not` and `$nor` match where what they negate is
//!   false *or* unknown (`(p) is unknown || !(p)`), so a field of another
//!   type, or none, passes;
//! - equality with an object or array is exact (keys, order of elements,
//!   types), matching the field or one of its elements;
//! - `$size`, `$elemMatch` and `$type: "array"` hold of an array only;
//!   `$type` of another type, of the field or one of its elements;
//! - conditions on one field that must hold of the same value (`$size`,
//!   exact equality, `null`) are evaluated per value of the path's parent
//!   (`exists(@.a ? (…))`), as MongoDB walks a path through arrays.
//!
//! JSON has one number type: `$type` `"int"`, `"long"`, `"double"`,
//! `"decimal"` (and their codes) all mean a number.  Keys are not
//! ordered in nucleus's json (§M15 item 3), so equality with an object
//! ignores its keys' order, and an object holding both operators and
//! fields is an error rather than read by its first key.  Not compiled
//! (`0A000`): `$expr`, `$where`, `$text`, `$jsonSchema`, `$mod`,
//! geospatial and bitwise operators, positions in paths (`a.0`), ordering
//! comparisons with objects or arrays.

use super::ast::{Cmp, Expr, FLAG_I, FLAG_M, FLAG_S, Method, Path, Step};
use super::parse::{num, regex};
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::json::form::{self, Node};

/// The lax path a filter document (json, binary form) compiles to.
pub fn compile(filter: &[u8]) -> Result<Path> {
    let Node::Object(o) = form::root(filter)? else {
        return Err(Error::invalid_parameter("a filter must be a JSON object"));
    };
    let expr = match document(&Expr::Current, o)? {
        Some(p) => Expr::Chain(Box::new(Expr::Root), vec![Step::Filter(Box::new(p))]),
        None => Expr::Root,
    };
    Ok(Path {
        strict: false,
        expr,
    })
}

// ----------------------------------------------------------- the tree

fn chain(base: &Expr, steps: impl IntoIterator<Item = Step>) -> Expr {
    match base {
        Expr::Chain(h, s) => {
            let mut s = s.clone();
            s.extend(steps);
            Expr::Chain(h.clone(), s)
        }
        _ => Expr::Chain(Box::new(base.clone()), steps.into_iter().collect()),
    }
}

fn keys(base: &Expr, path: &[String]) -> Expr {
    if path.is_empty() {
        return base.clone();
    }
    chain(base, path.iter().map(|k| Step::Key(k.clone())))
}

fn method(e: &Expr, m: Method) -> Expr {
    chain(e, [Step::Method(m)])
}

fn filtered(e: &Expr, p: Expr) -> Expr {
    chain(e, [Step::Filter(Box::new(p))])
}

fn cmp(op: Cmp, l: Expr, r: Expr) -> Expr {
    Expr::Cmp(op, Box::new(l), Box::new(r))
}

fn and(a: Expr, b: Expr) -> Expr {
    Expr::And(Box::new(a), Box::new(b))
}

fn or(a: Expr, b: Expr) -> Expr {
    Expr::Or(Box::new(a), Box::new(b))
}

fn exists(e: Expr) -> Expr {
    Expr::Exists(Box::new(e))
}

/// A predicate never true.
fn never() -> Expr {
    cmp(Cmp::Eq, Expr::Bool(true), Expr::Bool(false))
}

/// True where `p` is false or unknown: MongoDB's negations pass a field
/// of another type, or none.
fn negate(p: Expr) -> Expr {
    or(Expr::IsUnknown(Box::new(p.clone())), Expr::Not(Box::new(p)))
}

fn all(ps: Vec<Expr>, empty: Expr) -> Expr {
    ps.into_iter().reduce(and).unwrap_or(empty)
}

fn any(ps: Vec<Expr>) -> Expr {
    ps.into_iter().reduce(or).unwrap_or_else(never)
}

fn string(s: &str) -> Expr {
    Expr::Str(
        s.to_string(),
        crate::json::ops::string(s).into_boxed_slice(),
    )
}

fn type_is(e: &Expr, t: &str) -> Expr {
    cmp(Cmp::Eq, method(e, Method::Type), string(t))
}

/// A scalar's literal; `None` for an object or array.
fn literal(n: &Node<'_>, bytes: &[u8]) -> Result<Option<Expr>> {
    Ok(Some(match n {
        Node::Null => Expr::Null,
        Node::Bool(b) => Expr::Bool(*b),
        Node::Int(i) => num(Decimal::from_i64(*i)),
        Node::Dec(_) => num(Decimal::parse(&crate::json::to_text(bytes)?)?),
        Node::Str(s) => string(form::text_of(s)?),
        Node::Array(_) | Node::Object(_) => return Ok(None),
    }))
}

/// The JSON type `.type()` names for one of MongoDB's (`None`: no JSON
/// value has it).
fn json_type(t: &Node<'_>) -> Result<Option<&'static str>> {
    let name = match t {
        Node::Int(i) => match i {
            1 | 16 | 18 | 19 => "number",
            2 => "string",
            3 => "object",
            4 => "array",
            8 => "boolean",
            10 => "null",
            _ => return Ok(None),
        },
        Node::Str(s) => match form::text_of(s)? {
            "double" | "int" | "long" | "decimal" | "number" => "number",
            "string" => "string",
            "object" => "object",
            "array" => "array",
            "bool" => "boolean",
            "null" => "null",
            _ => return Ok(None),
        },
        _ => {
            return Err(Error::invalid_parameter(
                "$type takes a type's name, its number, or an array of them",
            ));
        }
    };
    Ok(Some(name))
}

// ----------------------------------------------------------- the filter

fn unsupported(what: &str) -> Error {
    Error::feature(format!("{what} is not supported in a filter"))
}

fn unknown(op: &str) -> Error {
    Error::invalid_parameter(format!("unknown operator in a filter: {op}"))
}

/// A field's path: its keys.  A key after the first may begin with `$`
/// (`at.$date`: Extended JSON's wrapper, which the MongoDB module ranges
/// on) — an operator is never inside a dotted path, so it is a name
/// there; MongoDB refuses it.
fn field_path(field: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = field.split('.').map(str::to_string).collect();
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            return Err(Error::invalid_parameter(format!(
                "a filter's field \"{field}\" has an empty part"
            )));
        }
        if i == 0 && p.starts_with('$') {
            return Err(unknown(p));
        }
        if p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(unsupported(&format!("a position in a path (\"{field}\")")));
        }
    }
    Ok(parts)
}

/// A filter document's conditions on the value `base` (the document, or
/// an element in `$elemMatch`), all of them; `None` when it has none.
fn document(base: &Expr, o: form::Object<'_>) -> Result<Option<Expr>> {
    let mut ps = Vec::new();
    for i in 0..o.len() {
        let key = o.key(i)?;
        let v = o.value(i)?;
        match key {
            "$and" | "$or" | "$nor" => {
                let Node::Array(a) = v else {
                    return Err(Error::invalid_parameter(format!(
                        "{key} takes a nonempty array of filters"
                    )));
                };
                if a.is_empty() {
                    return Err(Error::invalid_parameter(format!(
                        "{key} takes a nonempty array of filters"
                    )));
                }
                let mut each = Vec::new();
                for j in 0..a.len() {
                    let Node::Object(f) = a.get(j)? else {
                        return Err(Error::invalid_parameter(format!(
                            "{key} takes an array of filter objects"
                        )));
                    };
                    each.push(
                        document(base, f)?
                            .unwrap_or_else(|| cmp(Cmp::Eq, Expr::Bool(true), Expr::Bool(true))),
                    );
                }
                ps.push(match key {
                    "$and" => all(each, never()),
                    "$or" => any(each),
                    _ => negate(any(each)),
                });
            }
            "$comment" => {}
            "$expr" | "$where" | "$text" | "$jsonSchema" | "$sampleRate" => {
                return Err(unsupported(key));
            }
            k if k.starts_with('$') => return Err(unknown(k)),
            field => {
                let path = field_path(field)?;
                ps.push(condition(base, &path, &v, o.value_bytes(i)?)?);
            }
        }
    }
    Ok(ps.into_iter().reduce(and))
}

/// Whether an object is one of operators (all keys `$…`), of fields
/// (none), or mixed (an error).
fn operators(o: &form::Object<'_>) -> Result<bool> {
    let mut ops = 0;
    for i in 0..o.len() {
        if o.key(i)?.starts_with('$') {
            ops += 1;
        }
    }
    if ops != 0 && ops != o.len() {
        return Err(Error::invalid_parameter(
            "a filter's object mixes operators ($…) and fields",
        ));
    }
    Ok(ops != 0 && ops == o.len())
}

/// The condition `spec` on the field at `path` below `base`.
fn condition(base: &Expr, path: &[String], spec: &Node<'_>, bytes: &[u8]) -> Result<Expr> {
    match spec {
        Node::Object(o) if !o.is_empty() && operators(o)? => {
            let mut ps = Vec::new();
            let options = match o.get_bytes("$options")? {
                Some(b) => match form::root(b)? {
                    Node::Str(s) => Some(form::text_of(s)?),
                    _ => return Err(Error::invalid_parameter("$options takes a string")),
                },
                None => None,
            };
            for i in 0..o.len() {
                let op = o.key(i)?;
                if op == "$options" {
                    if o.find("$regex")?.is_none() {
                        return Err(Error::invalid_parameter("$options without $regex"));
                    }
                    continue;
                }
                ps.push(operator(
                    base,
                    path,
                    op,
                    &o.value(i)?,
                    o.value_bytes(i)?,
                    options,
                )?);
            }
            Ok(all(ps, never()))
        }
        _ => equal(base, path, spec, bytes),
    }
}

fn operator(
    base: &Expr,
    path: &[String],
    op: &str,
    v: &Node<'_>,
    bytes: &[u8],
    options: Option<&str>,
) -> Result<Expr> {
    let field = keys(base, path);
    let order = |c: Cmp| -> Result<Expr> {
        match literal(v, bytes)? {
            // MongoDB: `$gte` / `$lte` null as equality with null,
            // `$gt` / `$lt` null nothing.
            Some(Expr::Null) => Ok(match c {
                Cmp::Ge | Cmp::Le => null(base, path),
                _ => never(),
            }),
            Some(l) => Ok(cmp(c, field.clone(), l)),
            None => Err(unsupported(&format!("{op} with an object or array"))),
        }
    };
    let array = |what: &str| -> Result<form::Array<'_>> {
        match v {
            Node::Array(a) => Ok(*a),
            _ => Err(Error::invalid_parameter(format!("{what} takes an array"))),
        }
    };
    Ok(match op {
        "$eq" => equal(base, path, v, bytes)?,
        "$ne" => negate(equal(base, path, v, bytes)?),
        "$gt" => order(Cmp::Gt)?,
        "$gte" => order(Cmp::Ge)?,
        "$lt" => order(Cmp::Lt)?,
        "$lte" => order(Cmp::Le)?,
        "$in" | "$nin" => {
            let a = array(op)?;
            let mut each = Vec::new();
            for j in 0..a.len() {
                each.push(equal(base, path, &a.get(j)?, a.bytes(j)?)?);
            }
            if op == "$in" {
                any(each)
            } else {
                negate(any(each))
            }
        }
        "$all" => {
            let a = array(op)?;
            let mut each = Vec::new();
            for j in 0..a.len() {
                let e = a.get(j)?;
                if let Node::Object(o) = &e
                    && !o.is_empty()
                    && operators(o)?
                {
                    return Err(unsupported("$all with operators"));
                }
                each.push(equal(base, path, &e, a.bytes(j)?)?);
            }
            all(each, never())
        }
        "$exists" => {
            let yes = match v {
                Node::Bool(b) => *b,
                Node::Int(i) => *i != 0,
                Node::Null => false,
                Node::Dec(_) => !crate::json::to_text(bytes)?
                    .trim_matches(['0', '.', '-'])
                    .is_empty(),
                _ => true,
            };
            if yes {
                exists(field)
            } else {
                Expr::Not(Box::new(exists(field)))
            }
        }
        "$type" => {
            let mut names = Vec::new();
            match v {
                Node::Array(a) => {
                    for j in 0..a.len() {
                        names.push(json_type(&a.get(j)?)?);
                    }
                }
                t => names.push(json_type(t)?),
            }
            any(names
                .into_iter()
                .map(|t| match t {
                    None => never(),
                    Some("array") => type_is(&field, "array"),
                    Some(t) => exists(filtered(&field, type_is(&Expr::Current, t))),
                })
                .collect())
        }
        "$size" => {
            let n = match v {
                Node::Int(n) if *n >= 0 => *n,
                _ => {
                    return Err(Error::invalid_parameter(
                        "$size takes a nonnegative integer",
                    ));
                }
            };
            scoped(base, path, |f| {
                and(
                    type_is(f, "array"),
                    cmp(Cmp::Eq, method(f, Method::Size), num(Decimal::from_i64(n))),
                )
            })
        }
        "$regex" => {
            let Node::Str(s) = v else {
                return Err(Error::invalid_parameter("$regex takes a string"));
            };
            let mut flags = String::new();
            for c in options.unwrap_or("").chars() {
                match c {
                    'i' | 'm' | 's' => flags.push(c),
                    'x' => return Err(unsupported("$options \"x\"")),
                    _ => {
                        return Err(Error::invalid_parameter(format!(
                            "$options has an unknown flag \"{c}\""
                        )));
                    }
                }
            }
            let re = regex(form::text_of(s)?.to_string(), &flags)?;
            debug_assert!(re.flags & !(FLAG_I | FLAG_M | FLAG_S) == 0);
            Expr::LikeRegex(Box::new(field), Box::new(re))
        }
        "$elemMatch" => {
            let Node::Object(o) = v else {
                return Err(Error::invalid_parameter("$elemMatch takes an object"));
            };
            // Operators on the element itself, or a filter on its fields.
            let inner = if !o.is_empty() && operators(o)? {
                condition(&Expr::Current, &[], v, bytes)?
            } else {
                // Fields: of an element that is an object or an array
                // (MongoDB's; a scalar has no fields to test).
                and(
                    or(
                        type_is(&Expr::Current, "object"),
                        type_is(&Expr::Current, "array"),
                    ),
                    document(&Expr::Current, *o)?
                        .unwrap_or_else(|| cmp(Cmp::Eq, Expr::Bool(true), Expr::Bool(true))),
                )
            };
            scoped(base, path, |f| {
                and(
                    type_is(f, "array"),
                    exists(filtered(&chain(f, [Step::AnyArray]), inner.clone())),
                )
            })
        }
        "$not" => match v {
            Node::Object(o) if !o.is_empty() && operators(o)? => {
                negate(condition(base, path, v, bytes)?)
            }
            _ => {
                return Err(Error::invalid_parameter(
                    "$not takes an object of operators",
                ));
            }
        },
        "$mod" | "$bitsAllSet" | "$bitsAnySet" | "$bitsAllClear" | "$bitsAnyClear"
        | "$geoWithin" | "$geoIntersects" | "$near" | "$nearSphere" => {
            return Err(unsupported(op));
        }
        _ => return Err(unknown(op)),
    })
}

/// `f` of each value the path's last key holds, per value of its parent
/// (so that tests of one value are of the same one): `f(@.k)` when the
/// path is one key, else `exists(@.a.b ? (f(@.k)))`.
fn scoped(base: &Expr, path: &[String], f: impl Fn(&Expr) -> Expr) -> Expr {
    match path.split_last() {
        None => f(base),
        Some((last, [])) => f(&keys(base, std::slice::from_ref(last))),
        Some((last, parent)) => exists(filtered(
            &keys(base, parent),
            f(&keys(&Expr::Current, std::slice::from_ref(last))),
        )),
    }
}

/// `{f: null}`: the field is `null` (or holds one) or missing.  Missing
/// at a step: the step's value is missing or a scalar; through an array
/// only its objects are walked into (MongoDB's: a scalar element has no
/// fields to lack).
fn null(base: &Expr, path: &[String]) -> Expr {
    match path.split_first() {
        None => cmp(Cmp::Eq, base.clone(), Expr::Null),
        Some((k, [])) => {
            let f = keys(base, std::slice::from_ref(k));
            or(
                cmp(Cmp::Eq, f.clone(), Expr::Null),
                Expr::Not(Box::new(exists(f))),
            )
        }
        Some((k, rest)) => {
            let f = keys(base, std::slice::from_ref(k));
            let scalar = any(["string", "number", "boolean", "null"]
                .into_iter()
                .map(|t| type_is(&f, t))
                .collect());
            or(
                or(Expr::Not(Box::new(exists(f.clone()))), scalar),
                exists(filtered(
                    &f,
                    and(
                        type_is(&Expr::Current, "object"),
                        null(&Expr::Current, rest),
                    ),
                )),
            )
        }
    }
}

/// Equality with `v`: a scalar compares (arrays unwrapped); `null` as
/// [`null`]; an object or array exactly, of the field or an element.
fn equal(base: &Expr, path: &[String], v: &Node<'_>, bytes: &[u8]) -> Result<Expr> {
    match literal(v, bytes)? {
        Some(Expr::Null) => Ok(null(base, path)),
        Some(l) => Ok(cmp(Cmp::Eq, keys(base, path), l)),
        None => {
            let whole = exact(&Expr::Current, v, bytes)?;
            let field = keys(base, path);
            // The field itself (an array compared whole) or one of its
            // elements (the filter unwraps an array).
            let element = exists(filtered(&field, whole.clone()));
            Ok(match v {
                Node::Array(_) => or(
                    scoped(base, path, |f| {
                        exact(f, v, bytes).unwrap_or_else(|_| never())
                    }),
                    element,
                ),
                _ => element,
            })
        }
    }
}

/// `e` is exactly `v`: its type, its keys, its elements in order.
fn exact(e: &Expr, v: &Node<'_>, bytes: &[u8]) -> Result<Expr> {
    Ok(match v {
        Node::Null => type_is(e, "null"),
        Node::Bool(_) => and(
            type_is(e, "boolean"),
            cmp(Cmp::Eq, e.clone(), literal(v, bytes)?.expect("scalar")),
        ),
        Node::Int(_) | Node::Dec(_) => and(
            type_is(e, "number"),
            cmp(Cmp::Eq, e.clone(), literal(v, bytes)?.expect("scalar")),
        ),
        Node::Str(_) => and(
            type_is(e, "string"),
            cmp(Cmp::Eq, e.clone(), literal(v, bytes)?.expect("scalar")),
        ),
        Node::Array(a) => {
            let mut ps = vec![
                type_is(e, "array"),
                cmp(
                    Cmp::Eq,
                    method(e, Method::Size),
                    num(Decimal::from_i64(a.len() as i64)),
                ),
            ];
            for j in 0..a.len() {
                let at = chain(
                    e,
                    [Step::Index(vec![(num(Decimal::from_i64(j as i64)), None)])],
                );
                ps.push(exact(&at, &a.get(j)?, a.bytes(j)?)?);
            }
            all(ps, never())
        }
        Node::Object(o) => {
            // Its own keys and no others; each value exact.  Tested
            // through a filter on the object itself (no array there to
            // unwrap: its type is checked first).
            let mut ps = Vec::new();
            let mut others = Vec::new();
            for i in 0..o.len() {
                let k = o.key(i)?;
                others.push(cmp(
                    Cmp::Ne,
                    chain(&Expr::Current, [Step::Key("key".into())]),
                    string(k),
                ));
                ps.push(exact(
                    &keys(&Expr::Current, &[k.to_string()]),
                    &o.value(i)?,
                    o.value_bytes(i)?,
                )?);
            }
            let kv = method(&Expr::Current, Method::KeyValue);
            let no_other = Expr::Not(Box::new(exists(if others.is_empty() {
                kv
            } else {
                filtered(&kv, all(others, never()))
            })));
            ps.push(no_other);
            and(type_is(e, "object"), exists(filtered(e, all(ps, never()))))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(f: &str) -> String {
        compile(&crate::json::parse::parse(f).unwrap())
            .map(|p| p.to_string())
            .unwrap_or_else(|e| format!("ERROR {}: {}", e.code, e.message))
    }

    #[test]
    fn filters_compile_to_lax_paths() {
        assert_eq!(c("{}"), "$");
        assert_eq!(c(r#"{"name": "ann"}"#), r#"$?(@."name" == "ann")"#);
        assert_eq!(
            c(r#"{"age": {"$gt": 30, "$lte": 40}}"#),
            r#"$?(@."age" > 30 && @."age" <= 40)"#
        );
        assert_eq!(
            c(r#"{"a.b": {"$ne": 1}}"#),
            r#"$?((@."a"."b" == 1) is unknown || !(@."a"."b" == 1))"#
        );
        assert_eq!(
            c(r#"{"x": null}"#),
            r#"$?(@."x" == null || !(exists (@."x")))"#
        );
        assert_eq!(
            c(r#"{"t": {"$size": 2}}"#),
            r#"$?(@."t".type() == "array" && @."t".size() == 2)"#
        );
    }

    #[test]
    fn what_a_filter_cannot_say_is_an_error() {
        assert!(c(r#"{"a": {"$foo": 1}}"#).starts_with("ERROR 22023"));
        assert!(c(r#"{"$where": "1"}"#).starts_with("ERROR 0A000"));
        assert!(c(r#"{"a.0": 1}"#).starts_with("ERROR 0A000"));
        assert!(c(r#"{"a": {"$gt": [1]}}"#).starts_with("ERROR 0A000"));
        assert!(c(r#"{"a": {"$gt": 1, "b": 2}}"#).starts_with("ERROR 22023"));
        assert!(c(r#"{"$or": []}"#).starts_with("ERROR 22023"));
        assert!(c("[1]").starts_with("ERROR 22023"));
    }
}
