//! JSON Schema (PLAN.md §M14.15 J5): `json_matches_schema(doc, schema)`,
//! whether a document fits a schema — so a `CHECK` refuses a model's
//! output that does not — and `json_schema_errors(doc, schema)`, where
//! and why it does not, each place a path (`$.user.age`), to hand back to
//! the model.
//!
//! The vocabulary is JSON Schema 2020-12's validation and applicators —
//! what the model providers' structured outputs take (`type`,
//! `properties`, `required`, `additionalProperties`, `enum`, `const`,
//! `items`, `anyOf`, `$defs` / `$ref`, bounds, `pattern`) and the rest of
//! it: `prefixItems`, `contains`, `uniqueItems`, `patternProperties`,
//! `propertyNames`, `dependentRequired`, `dependentSchemas`, `allOf`,
//! `oneOf`, `not`, `if` / `then` / `else`, `multipleOf` (exact), the
//! counts.  `$ref` is a pointer into the schema itself (`#`,
//! `#/$defs/name`, recursion included).  Annotations are ignored
//! (`title`, `description`, `default`, `examples`, `format`, …), as are
//! `x-` extensions; OpenAPI's `nullable` (Gemini's schemas) admits
//! `null`.  Any other keyword is refused (`0A000`) rather than ignored —
//! a misspelt `requried` would otherwise pass every document —, among
//! them `unevaluatedProperties`, `unevaluatedItems` and `$dynamicRef`.
//! `pattern` is the core's regular expressions (Rust's `regex`, close to
//! ECMA-262's without look-around or back-references).  Equality
//! (`enum`, `const`, `uniqueItems`) is of canonical bytes: equal values,
//! equal bytes (`1.0` is `1`).
//!
//! A schema is compiled once and kept, per thread, by its bytes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use super::form::{self, Node};
use crate::error::{Error, Result};
use crate::types::Value;

// Kinds of value, as bits.
const NULL: u8 = 1;
const BOOLEAN: u8 = 2;
const INTEGER: u8 = 4;
/// A number that is not an integer.
const FRACTION: u8 = 8;
const NUMBER: u8 = INTEGER | FRACTION;
const STRING: u8 = 16;
const ARRAY: u8 = 32;
const OBJECT: u8 = 64;

/// How deep a check may go through `$ref`s and subschemas.
const MAX_DEPTH: usize = 256;

/// A compiled schema: its subschemas, the root first.
pub struct Schema {
    nodes: Vec<Sub>,
}

enum Sub {
    /// `true` (anything) or `false` (nothing).
    Bool(bool),
    /// The keywords that check something, in the order they are checked
    /// (and their problems told): only those the schema has, those of a
    /// kind of value together — a document's value meets few of them.
    Rules(Vec<Kw>),
}

enum Kw {
    Type(u8),
    Enum(Vec<Vec<u8>>),
    Const(Vec<u8>),
    Ref(usize),
    Number(Box<NumberRules>),
    Str(Box<StrRules>),
    Array(Box<ArrayRules>),
    Object(Box<ObjectRules>),
    AllOf(Vec<usize>),
    AnyOf(Vec<usize>),
    OneOf(Vec<usize>),
    Not(usize),
    If(usize, Option<usize>, Option<usize>),
}

struct NumberRules {
    minimum: Option<Bound>,
    maximum: Option<Bound>,
    exclusive_minimum: Option<Bound>,
    exclusive_maximum: Option<Bound>,
    multiple_of: Option<(String, Vec<u8>)>,
}

struct StrRules {
    min_length: Option<u64>,
    max_length: Option<u64>,
    pattern: Option<(String, regex::bytes::Regex)>,
}

struct ArrayRules {
    min_items: Option<u64>,
    max_items: Option<u64>,
    unique_items: bool,
    prefix_items: Vec<usize>,
    items: Option<usize>,
    contains: Option<usize>,
    min_contains: Option<u64>,
    max_contains: Option<u64>,
}

struct ObjectRules {
    min_properties: Option<u64>,
    max_properties: Option<u64>,
    required: Vec<String>,
    /// `properties` in the order of an object's keys (`form::key_order`),
    /// each with whether it is required: a document's keys are walked
    /// along them, not looked up.
    properties: Vec<(String, usize, bool)>,
    /// How many of `properties` are required.
    required_known: usize,
    /// The required names that are not in `properties`.
    required_other: Vec<String>,
    pattern_properties: Vec<(String, regex::bytes::Regex, usize)>,
    additional_properties: Option<usize>,
    property_names: Option<usize>,
    dependent_required: Vec<(String, Vec<String>)>,
    dependent_schemas: Vec<(String, usize)>,
}

/// A bound: the number's text, its key encoding (compared exactly) and,
/// for an integer, the integer (compared with an integer's directly).
struct Bound {
    text: String,
    key: Vec<u8>,
    int: Option<i64>,
}

impl Bound {
    /// How `n` (whose bytes are `b`; its key, once made, in `key`)
    /// compares with the bound.
    fn order(&self, n: &Node, b: &[u8], key: &mut Option<Vec<u8>>) -> Result<std::cmp::Ordering> {
        if let (Node::Int(x), Some(y)) = (n, self.int) {
            return Ok(x.cmp(&y));
        }
        if key.is_none() {
            *key = Some(super::key::of(b)?);
        }
        Ok(key.as_deref().unwrap_or_default().cmp(&self.key[..]))
    }
}

#[derive(Default)]
struct Rules {
    types: Option<u8>,
    enumerated: Option<Vec<Vec<u8>>>,
    constant: Option<Vec<u8>>,
    multiple_of: Option<(String, Vec<u8>)>,
    minimum: Option<Bound>,
    maximum: Option<Bound>,
    exclusive_minimum: Option<Bound>,
    exclusive_maximum: Option<Bound>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    pattern: Option<(String, regex::bytes::Regex)>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    unique_items: bool,
    prefix_items: Vec<usize>,
    items: Option<usize>,
    contains: Option<usize>,
    min_contains: Option<u64>,
    max_contains: Option<u64>,
    min_properties: Option<u64>,
    max_properties: Option<u64>,
    required: Vec<String>,
    properties: Vec<(String, usize)>,
    pattern_properties: Vec<(String, regex::bytes::Regex, usize)>,
    additional_properties: Option<usize>,
    property_names: Option<usize>,
    dependent_required: Vec<(String, Vec<String>)>,
    dependent_schemas: Vec<(String, usize)>,
    all_of: Vec<usize>,
    any_of: Vec<usize>,
    one_of: Vec<usize>,
    not: Option<usize>,
    condition: Option<(usize, Option<usize>, Option<usize>)>,
    reference: Option<usize>,
}

fn invalid(what: impl std::fmt::Display) -> Error {
    Error::invalid_parameter(format!("invalid JSON Schema: {what}"))
}

/// A JSON Pointer's reference token, escaped (RFC 6901).
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The value at `pointer` (`#…`) in `root`.
fn resolve<'a>(root: &'a [u8], reference: &str) -> Result<&'a [u8]> {
    let Some(pointer) = reference.strip_prefix('#') else {
        return Err(Error::feature(format!(
            "a JSON Schema $ref to another document ({reference}): only \"#…\" pointers into the schema"
        )));
    };
    let mut at = root;
    if pointer.is_empty() {
        return Ok(at);
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(Error::feature(format!(
            "a JSON Schema $ref to an anchor ({reference}): only JSON Pointers"
        )));
    };
    for token in rest.split('/') {
        let token = percent_decode(token).replace("~1", "/").replace("~0", "~");
        at = match form::root(at)? {
            Node::Object(o) => o
                .get_bytes(&token)?
                .ok_or_else(|| invalid(format!("$ref {reference} points nowhere")))?,
            Node::Array(a) => {
                let i: usize = token
                    .parse()
                    .map_err(|_| invalid(format!("$ref {reference} points nowhere")))?;
                if i >= a.len() {
                    return Err(invalid(format!("$ref {reference} points nowhere")));
                }
                a.bytes(i)?
            }
            _ => return Err(invalid(format!("$ref {reference} points nowhere"))),
        };
    }
    Ok(at)
}

/// `%XX` escapes decoded (a URI fragment's).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(x) = u8::from_str_radix(hex, 16)
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Compiler<'a> {
    root: &'a [u8],
    nodes: Vec<Sub>,
    /// Subschemas compiled, by their pointer: `$ref`s to one share it.
    by_pointer: HashMap<String, usize>,
}

fn regex_of(p: &str) -> Result<regex::bytes::Regex> {
    regex::bytes::RegexBuilder::new(p)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| invalid(format!("the pattern {p:?}: {e}")))
}

impl<'a> Compiler<'a> {
    /// The subschema at `pointer` (its bytes `b`), compiled once.
    fn at(&mut self, b: &'a [u8], pointer: String) -> Result<usize> {
        if let Some(&i) = self.by_pointer.get(&pointer) {
            return Ok(i);
        }
        let i = self.nodes.len();
        self.nodes.push(Sub::Bool(true));
        self.by_pointer.insert(pointer.clone(), i);
        let sub = self.compile(b, &pointer)?;
        self.nodes[i] = sub;
        Ok(i)
    }

    fn child(&mut self, b: &'a [u8], pointer: &str, key: &str) -> Result<usize> {
        self.at(b, format!("{pointer}/{}", escape(key)))
    }

    fn list(&mut self, b: &'a [u8], pointer: &str, key: &str) -> Result<Vec<usize>> {
        let Node::Array(a) = form::root(b)? else {
            return Err(invalid(format!("{key} must be an array of schemas")));
        };
        if a.is_empty() && key != "prefixItems" {
            return Err(invalid(format!("{key} must not be empty")));
        }
        (0..a.len())
            .map(|i| self.at(a.bytes(i)?, format!("{pointer}/{key}/{i}")))
            .collect()
    }

    fn compile(&mut self, b: &'a [u8], pointer: &str) -> Result<Sub> {
        let o = match form::root(b)? {
            Node::Bool(x) => return Ok(Sub::Bool(x)),
            Node::Object(o) => o,
            _ => {
                return Err(invalid(format!(
                    "{pointer:?} is not a schema (an object or a boolean)"
                )));
            }
        };
        let mut r = Rules::default();
        let mut nullable = false;
        for k in 0..o.len() {
            let key = o.key(k)?;
            let v = o.value_bytes(k)?;
            let n = form::root(v)?;
            match key {
                "$schema" | "$id" | "$comment" | "$anchor" | "$vocabulary" | "$defs"
                | "definitions" | "title" | "description" | "default" | "examples"
                | "deprecated" | "readOnly" | "writeOnly" | "format" | "contentEncoding"
                | "contentMediaType" | "contentSchema" | "propertyOrdering" => {}
                k if k.starts_with("x-") => {}
                "nullable" => nullable = matches!(n, Node::Bool(true)),
                "type" => {
                    let one = |n: Node| -> Result<u8> {
                        let Node::Str(s) = n else {
                            return Err(invalid("type must be a name or names"));
                        };
                        Ok(match form::text_of(s)? {
                            "null" => NULL,
                            "boolean" => BOOLEAN,
                            "integer" => INTEGER,
                            "number" => NUMBER,
                            "string" => STRING,
                            "array" => ARRAY,
                            "object" => OBJECT,
                            other => return Err(invalid(format!("unknown type {other:?}"))),
                        })
                    };
                    r.types = Some(match n {
                        Node::Array(a) => {
                            let mut m = 0;
                            for i in 0..a.len() {
                                m |= one(a.get(i)?)?;
                            }
                            m
                        }
                        n => one(n)?,
                    });
                }
                "enum" => {
                    let Node::Array(a) = n else {
                        return Err(invalid("enum must be an array"));
                    };
                    r.enumerated = Some(
                        (0..a.len())
                            .map(|i| a.bytes(i).map(<[u8]>::to_vec))
                            .collect::<Result<_>>()?,
                    );
                }
                "const" => r.constant = Some(v.to_vec()),
                "multipleOf" => {
                    let x = number(n, key)?;
                    if super::to_f64(&n).is_none_or(|f| f <= 0.0) {
                        return Err(invalid("multipleOf must be greater than 0"));
                    }
                    r.multiple_of = Some((x, v.to_vec()));
                }
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                    let bound = Bound {
                        text: number(n, key)?,
                        key: super::key::of(v)?,
                        int: match n {
                            Node::Int(i) => Some(i),
                            _ => None,
                        },
                    };
                    match key {
                        "minimum" => r.minimum = Some(bound),
                        "maximum" => r.maximum = Some(bound),
                        "exclusiveMinimum" => r.exclusive_minimum = Some(bound),
                        _ => r.exclusive_maximum = Some(bound),
                    }
                }
                "minLength" => r.min_length = Some(count(n, key)?),
                "maxLength" => r.max_length = Some(count(n, key)?),
                "minItems" => r.min_items = Some(count(n, key)?),
                "maxItems" => r.max_items = Some(count(n, key)?),
                "minProperties" => r.min_properties = Some(count(n, key)?),
                "maxProperties" => r.max_properties = Some(count(n, key)?),
                "minContains" => r.min_contains = Some(count(n, key)?),
                "maxContains" => r.max_contains = Some(count(n, key)?),
                "pattern" => {
                    let Node::Str(s) = n else {
                        return Err(invalid("pattern must be a string"));
                    };
                    let p = form::text_of(s)?;
                    r.pattern = Some((p.to_string(), regex_of(p)?));
                }
                "uniqueItems" => {
                    let Node::Bool(x) = n else {
                        return Err(invalid("uniqueItems must be a boolean"));
                    };
                    r.unique_items = x;
                }
                "items" => {
                    if matches!(n, Node::Array(_)) {
                        return Err(Error::feature(
                            "items as an array (before JSON Schema 2020-12): use prefixItems",
                        ));
                    }
                    r.items = Some(self.child(v, pointer, key)?);
                }
                "prefixItems" => r.prefix_items = self.list(v, pointer, key)?,
                "contains" => r.contains = Some(self.child(v, pointer, key)?),
                "required" => {
                    let Node::Array(a) = n else {
                        return Err(invalid("required must be an array of names"));
                    };
                    for i in 0..a.len() {
                        let Node::Str(s) = a.get(i)? else {
                            return Err(invalid("required must be an array of names"));
                        };
                        r.required.push(form::text_of(s)?.to_string());
                    }
                }
                "properties" | "patternProperties" | "dependentSchemas" | "dependentRequired" => {
                    let Node::Object(p) = n else {
                        return Err(invalid(format!("{key} must be an object")));
                    };
                    for i in 0..p.len() {
                        let name = p.key(i)?.to_string();
                        let sub = p.value_bytes(i)?;
                        let at = format!("{pointer}/{key}/{}", escape(&name));
                        match key {
                            "properties" => r.properties.push((name, self.at(sub, at)?)),
                            "patternProperties" => {
                                let re = regex_of(&name)?;
                                let s = self.at(sub, at)?;
                                r.pattern_properties.push((name, re, s));
                            }
                            "dependentSchemas" => {
                                r.dependent_schemas.push((name, self.at(sub, at)?));
                            }
                            _ => {
                                let Node::Array(a) = form::root(sub)? else {
                                    return Err(invalid("dependentRequired takes arrays of names"));
                                };
                                let mut names = Vec::new();
                                for j in 0..a.len() {
                                    let Node::Str(s) = a.get(j)? else {
                                        return Err(invalid(
                                            "dependentRequired takes arrays of names",
                                        ));
                                    };
                                    names.push(form::text_of(s)?.to_string());
                                }
                                r.dependent_required.push((name, names));
                            }
                        }
                    }
                }
                "additionalProperties" => {
                    r.additional_properties = Some(self.child(v, pointer, key)?);
                }
                "propertyNames" => r.property_names = Some(self.child(v, pointer, key)?),
                "allOf" => r.all_of = self.list(v, pointer, key)?,
                "anyOf" => r.any_of = self.list(v, pointer, key)?,
                "oneOf" => r.one_of = self.list(v, pointer, key)?,
                "not" => r.not = Some(self.child(v, pointer, key)?),
                "if" => {
                    let c = self.child(v, pointer, key)?;
                    let then = match o.get_bytes("then")? {
                        Some(t) => Some(self.child(t, pointer, "then")?),
                        None => None,
                    };
                    let otherwise = match o.get_bytes("else")? {
                        Some(e) => Some(self.child(e, pointer, "else")?),
                        None => None,
                    };
                    r.condition = Some((c, then, otherwise));
                }
                // Without `if`, they do nothing (as the standard says).
                "then" | "else" => {}
                "$ref" => {
                    let Node::Str(s) = n else {
                        return Err(invalid("$ref must be a string"));
                    };
                    let reference = form::text_of(s)?;
                    let target = resolve(self.root, reference)?;
                    let pointer = reference.trim_start_matches('#').to_string();
                    r.reference = Some(self.at(target, percent_decode(&pointer))?);
                }
                other => {
                    return Err(Error::feature(format!(
                        "the JSON Schema keyword {other:?} is not supported"
                    )));
                }
            }
        }
        if nullable && let Some(t) = &mut r.types {
            *t |= NULL;
        }
        Ok(Sub::Rules(lower(r)))
    }
}

/// A schema object's rules as the keywords checked, in their order.
fn lower(r: Rules) -> Vec<Kw> {
    let mut out = Vec::new();
    if let Some(t) = r.types {
        out.push(Kw::Type(t));
    }
    if let Some(e) = r.enumerated {
        out.push(Kw::Enum(e));
    }
    if let Some(c) = r.constant {
        out.push(Kw::Const(c));
    }
    if let Some(t) = r.reference {
        out.push(Kw::Ref(t));
    }
    if r.minimum.is_some()
        || r.maximum.is_some()
        || r.exclusive_minimum.is_some()
        || r.exclusive_maximum.is_some()
        || r.multiple_of.is_some()
    {
        out.push(Kw::Number(Box::new(NumberRules {
            minimum: r.minimum,
            maximum: r.maximum,
            exclusive_minimum: r.exclusive_minimum,
            exclusive_maximum: r.exclusive_maximum,
            multiple_of: r.multiple_of,
        })));
    }
    if r.min_length.is_some() || r.max_length.is_some() || r.pattern.is_some() {
        out.push(Kw::Str(Box::new(StrRules {
            min_length: r.min_length,
            max_length: r.max_length,
            pattern: r.pattern,
        })));
    }
    if r.min_items.is_some()
        || r.max_items.is_some()
        || r.unique_items
        || !r.prefix_items.is_empty()
        || r.items.is_some()
        || r.contains.is_some()
    {
        out.push(Kw::Array(Box::new(ArrayRules {
            min_items: r.min_items,
            max_items: r.max_items,
            unique_items: r.unique_items,
            prefix_items: r.prefix_items,
            items: r.items,
            contains: r.contains,
            min_contains: r.min_contains,
            max_contains: r.max_contains,
        })));
    }
    if r.min_properties.is_some()
        || r.max_properties.is_some()
        || !r.required.is_empty()
        || !r.properties.is_empty()
        || !r.pattern_properties.is_empty()
        || r.additional_properties.is_some()
        || r.property_names.is_some()
        || !r.dependent_required.is_empty()
        || !r.dependent_schemas.is_empty()
    {
        let mut properties: Vec<(String, usize, bool)> = r
            .properties
            .into_iter()
            .map(|(name, s)| {
                let required = r.required.contains(&name);
                (name, s, required)
            })
            .collect();
        properties.sort_by(|a, b| form::key_order(a.0.as_bytes(), b.0.as_bytes()));
        let required_known = properties.iter().filter(|p| p.2).count();
        let mut required_other: Vec<String> = r
            .required
            .iter()
            .filter(|n| !properties.iter().any(|p| &p.0 == *n))
            .cloned()
            .collect();
        required_other.dedup();
        out.push(Kw::Object(Box::new(ObjectRules {
            min_properties: r.min_properties,
            max_properties: r.max_properties,
            required: r.required,
            properties,
            required_known,
            required_other,
            pattern_properties: r.pattern_properties,
            additional_properties: r.additional_properties,
            property_names: r.property_names,
            dependent_required: r.dependent_required,
            dependent_schemas: r.dependent_schemas,
        })));
    }
    if !r.all_of.is_empty() {
        out.push(Kw::AllOf(r.all_of));
    }
    if !r.any_of.is_empty() {
        out.push(Kw::AnyOf(r.any_of));
    }
    if !r.one_of.is_empty() {
        out.push(Kw::OneOf(r.one_of));
    }
    if let Some(s) = r.not {
        out.push(Kw::Not(s));
    }
    if let Some((c, t, e)) = r.condition {
        out.push(Kw::If(c, t, e));
    }
    out
}

/// A number keyword's value, as text.
fn number(n: Node, key: &str) -> Result<String> {
    match n {
        Node::Int(_) | Node::Dec(_) => {
            let mut out = Vec::new();
            super::text::write_bytes(n, &mut out)?;
            String::from_utf8(out).map_err(|_| form::malformed())
        }
        _ => Err(invalid(format!("{key} must be a number"))),
    }
}

/// A count keyword's value: a non-negative integer.
fn count(n: Node, key: &str) -> Result<u64> {
    match n {
        Node::Int(i) if i >= 0 => Ok(i as u64),
        _ => Err(invalid(format!("{key} must be a non-negative integer"))),
    }
}

impl Schema {
    /// `schema` (its binary form) compiled.
    pub fn compile(schema: &[u8]) -> Result<Self> {
        let mut c = Compiler {
            root: schema,
            nodes: Vec::new(),
            by_pointer: HashMap::new(),
        };
        c.at(schema, String::new())?;
        Ok(Self { nodes: c.nodes })
    }

    /// Whether `doc` (its binary form) fits.
    pub fn matches(&self, doc: &[u8]) -> Result<bool> {
        let mut path = Vec::new();
        self.check(0, doc, &mut path, &mut None, 0)
    }

    /// Where and why `doc` does not fit (none where it does).
    pub fn errors(&self, doc: &[u8]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.check(0, doc, &mut path, &mut Some(&mut out), 0)?;
        Ok(out)
    }

    /// Checks `b` against subschema `i`, at `path`; the problems into
    /// `out` where it is given (all of them), else stopping at the first.
    fn check<'d>(
        &self,
        i: usize,
        b: &'d [u8],
        path: &mut Vec<Seg<'d>>,
        out: &mut Option<&mut Vec<String>>,
        depth: usize,
    ) -> Result<bool> {
        if depth > MAX_DEPTH {
            return Err(Error::statement_too_complex(
                "a JSON Schema check went too deep (a $ref loop?)",
            ));
        }
        let rules = match &self.nodes[i] {
            Sub::Bool(true) => return Ok(true),
            Sub::Bool(false) => {
                report(out, path, "no value is allowed here".into());
                return Ok(false);
            }
            Sub::Rules(r) => r,
        };
        let n = form::root(b)?;
        let mut ok = true;
        // A failed rule: noted, and the end where nothing is collected.
        macro_rules! fail {
            ($($msg:tt)*) => {{
                ok = false;
                if out.is_none() {
                    return Ok(false);
                }
                report(out, path, format!($($msg)*));
            }};
        }
        // A part checked against subschema `s` (at `path`, extended by
        // `seg` where given) failed: noted, the end where nothing is
        // collected.
        macro_rules! sub {
            ($s:expr, $b:expr) => {{
                if !self.check($s, $b, path, out, depth + 1)? {
                    ok = false;
                    if out.is_none() {
                        return Ok(false);
                    }
                }
            }};
            ($s:expr, $b:expr, $seg:expr) => {{
                path.push($seg);
                let good = self.check($s, $b, path, out, depth + 1)?;
                path.pop();
                if !good {
                    ok = false;
                    if out.is_none() {
                        return Ok(false);
                    }
                }
            }};
        }
        for kw in rules {
            match kw {
                Kw::Type(t) => {
                    let kind = kind_of(&n);
                    if t & kind == 0 {
                        fail!("expected {}, got {}", type_names(*t), type_names(kind));
                    }
                }
                Kw::Enum(values) => {
                    if !values.iter().any(|v| v == b) {
                        let shown: Vec<String> = values.iter().map(|v| show(v)).collect();
                        fail!("{} is not one of {}", show(b), shown.join(", "));
                    }
                }
                Kw::Const(c) => {
                    if c != b {
                        fail!("{} is not {}", show(b), show(c));
                    }
                }
                Kw::Ref(t) => sub!(*t, b),
                Kw::Number(r) => {
                    if !matches!(n, Node::Int(_) | Node::Dec(_)) {
                        continue;
                    }
                    use std::cmp::Ordering::{Greater, Less};
                    let mut key = None;
                    if let Some(m) = &r.minimum
                        && m.order(&n, b, &mut key)? == Less
                    {
                        fail!("{} is less than the minimum {}", show(b), m.text);
                    }
                    if let Some(m) = &r.maximum
                        && m.order(&n, b, &mut key)? == Greater
                    {
                        fail!("{} is greater than the maximum {}", show(b), m.text);
                    }
                    if let Some(m) = &r.exclusive_minimum
                        && m.order(&n, b, &mut key)? != Greater
                    {
                        fail!("{} is not greater than {}", show(b), m.text);
                    }
                    if let Some(m) = &r.exclusive_maximum
                        && m.order(&n, b, &mut key)? != Less
                    {
                        fail!("{} is not less than {}", show(b), m.text);
                    }
                    if let Some((text, m)) = &r.multiple_of
                        && !multiple_of(&n, &form::root(m)?)
                    {
                        fail!("{} is not a multiple of {text}", show(b));
                    }
                }
                Kw::Str(r) => {
                    let Node::Str(s) = n else { continue };
                    if r.min_length.is_some() || r.max_length.is_some() {
                        // Characters: the bytes that do not continue one.
                        let len = s.iter().filter(|&&c| (c as i8) >= -0x40).count() as u64;
                        if let Some(m) = r.min_length
                            && len < m
                        {
                            fail!("{} is {len} characters long, fewer than {m}", show(b));
                        }
                        if let Some(m) = r.max_length
                            && len > m
                        {
                            fail!("{} is {len} characters long, more than {m}", show(b));
                        }
                    }
                    if let Some((p, re)) = &r.pattern
                        && !re.is_match(s)
                    {
                        fail!("{} does not match the pattern {p}", show(b));
                    }
                }
                Kw::Array(r) => {
                    let Node::Array(a) = &n else { continue };
                    let len = a.len() as u64;
                    if let Some(m) = r.min_items
                        && len < m
                    {
                        fail!("{len} items, fewer than {m}");
                    }
                    if let Some(m) = r.max_items
                        && len > m
                    {
                        fail!("{len} items, more than {m}");
                    }
                    if r.unique_items
                        && let Some((first, k)) = duplicate(a)?
                    {
                        fail!("items {first} and {k} are equal");
                    }
                    for (k, &s) in r.prefix_items.iter().enumerate().take(a.len()) {
                        sub!(s, a.bytes(k)?, Seg::Index(k));
                    }
                    if let Some(s) = r.items {
                        for k in r.prefix_items.len()..a.len() {
                            sub!(s, a.bytes(k)?, Seg::Index(k));
                        }
                    }
                    if let Some(s) = r.contains {
                        let mut found = 0u64;
                        for k in 0..a.len() {
                            if self.check(s, a.bytes(k)?, path, &mut None, depth + 1)? {
                                found += 1;
                            }
                        }
                        let least = r.min_contains.unwrap_or(1);
                        if found < least {
                            fail!("{found} items match contains, fewer than {least}");
                        }
                        if let Some(most) = r.max_contains
                            && found > most
                        {
                            fail!("{found} items match contains, more than {most}");
                        }
                    }
                }
                Kw::Object(r) => {
                    let Node::Object(o) = &n else { continue };
                    let len = o.len() as u64;
                    if let Some(m) = r.min_properties
                        && len < m
                    {
                        fail!("{len} properties, fewer than {m}");
                    }
                    if let Some(m) = r.max_properties
                        && len > m
                    {
                        fail!("{len} properties, more than {m}");
                    }
                    if out.is_some() {
                        // Each missing one named, in the schema's order.
                        for name in &r.required {
                            if o.find(name)?.is_none() {
                                fail!("the required property {} is missing", quoted(name));
                            }
                        }
                    } else {
                        // Those among `properties` are counted on the walk.
                        for name in &r.required_other {
                            if o.find(name)?.is_none() {
                                return Ok(false);
                            }
                        }
                    }
                    for (name, needs) in &r.dependent_required {
                        if o.find(name)?.is_some() {
                            for need in needs {
                                if o.find(need)?.is_none() {
                                    fail!(
                                        "the property {} requires {}, which is missing",
                                        quoted(name),
                                        quoted(need)
                                    );
                                }
                            }
                        }
                    }
                    for (name, s) in &r.dependent_schemas {
                        if o.find(name)?.is_some() {
                            sub!(*s, b);
                        }
                    }
                    let mut j = 0;
                    let mut required_seen = 0;
                    for k in 0..o.len() {
                        let key = o.key_bytes(k)?;
                        let value = o.value_bytes(k)?;
                        if let Some(s) = r.property_names {
                            let mut name = Vec::new();
                            form::put_str(&mut name, form::text_of(key)?);
                            if !self.check(s, &name, &mut Vec::new(), &mut None, depth + 1)? {
                                fail!("the property name {} is not allowed", quoted_bytes(key));
                            }
                        }
                        let mut matched = false;
                        // Both in the keys' order: a walk along.
                        while j < r.properties.len()
                            && form::key_order(r.properties[j].0.as_bytes(), key)
                                == std::cmp::Ordering::Less
                        {
                            j += 1;
                        }
                        if j < r.properties.len() && r.properties[j].0.as_bytes() == key {
                            matched = true;
                            let (_, s, required) = r.properties[j];
                            j += 1;
                            required_seen += usize::from(required);
                            sub!(s, value, Seg::Key(key));
                        }
                        for (_, re, s) in &r.pattern_properties {
                            if re.is_match(key) {
                                matched = true;
                                sub!(*s, value, Seg::Key(key));
                            }
                        }
                        if !matched && let Some(s) = r.additional_properties {
                            if matches!(self.nodes[s], Sub::Bool(false)) {
                                fail!("the property {} is not allowed", quoted_bytes(key));
                                continue;
                            }
                            sub!(s, value, Seg::Key(key));
                        }
                    }
                    if required_seen < r.required_known && out.is_none() {
                        return Ok(false);
                    }
                }
                Kw::AllOf(list) => {
                    for &s in list {
                        sub!(s, b);
                    }
                }
                Kw::AnyOf(list) => {
                    let mut any = false;
                    for &s in list {
                        if self.check(s, b, path, &mut None, depth + 1)? {
                            any = true;
                            break;
                        }
                    }
                    if !any {
                        fail!("{} matches none of anyOf's {} schemas", show(b), list.len());
                    }
                }
                Kw::OneOf(list) => {
                    let mut fits = 0;
                    for &s in list {
                        if self.check(s, b, path, &mut None, depth + 1)? {
                            fits += 1;
                        }
                    }
                    if fits != 1 {
                        fail!(
                            "{} matches {fits} of oneOf's schemas, not exactly one",
                            show(b)
                        );
                    }
                }
                Kw::Not(s) => {
                    if self.check(*s, b, path, &mut None, depth + 1)? {
                        fail!("{} matches the schema of not", show(b));
                    }
                }
                Kw::If(c, then, otherwise) => {
                    let branch = if self.check(*c, b, path, &mut None, depth + 1)? {
                        then
                    } else {
                        otherwise
                    };
                    if let Some(s) = branch {
                        sub!(*s, b);
                    }
                }
            }
        }
        Ok(ok)
    }
}

/// A step of a path into a document: a key (its bytes), an index.
enum Seg<'d> {
    Key(&'d [u8]),
    Index(usize),
}

/// The first two equal items of `a` (their positions), if any.
fn duplicate(a: &form::Array) -> Result<Option<(usize, usize)>> {
    let n = a.len();
    if n <= 32 {
        for j in 1..n {
            let x = a.bytes(j)?;
            for i in 0..j {
                if a.bytes(i)? == x {
                    return Ok(Some((i, j)));
                }
            }
        }
        return Ok(None);
    }
    let mut seen: HashMap<&[u8], usize> = HashMap::with_capacity(n);
    for k in 0..n {
        if let Some(first) = seen.insert(a.bytes(k)?, k) {
            return Ok(Some((first, k)));
        }
    }
    Ok(None)
}

fn quoted_bytes(k: &[u8]) -> String {
    quoted(&String::from_utf8_lossy(k))
}

fn report(out: &mut Option<&mut Vec<String>>, path: &[Seg], msg: String) {
    if let Some(out) = out {
        let mut p = String::from("$");
        for s in path {
            match s {
                Seg::Index(i) => {
                    let _ = write!(p, "[{i}]");
                }
                Seg::Key(k) => {
                    let k = String::from_utf8_lossy(k);
                    let plain = !k.is_empty()
                        && !k.starts_with(|c: char| c.is_ascii_digit())
                        && k.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if plain {
                        let _ = write!(p, ".{k}");
                    } else {
                        let _ = write!(p, ".{}", quoted(&k));
                    }
                }
            }
        }
        out.push(format!("{p}: {msg}"));
    }
}

fn quoted(s: &str) -> String {
    show(&super::ops::string(s))
}

/// A value's text, cut short past 60 characters.
fn show(b: &[u8]) -> String {
    let s = super::to_text(b).unwrap_or_else(|_| "?".into());
    if s.chars().count() > 60 {
        let cut: String = s.chars().take(57).collect();
        format!("{cut}...")
    } else {
        s
    }
}

fn kind_of(n: &Node) -> u8 {
    match n {
        Node::Null => NULL,
        Node::Bool(_) => BOOLEAN,
        Node::Int(_) => INTEGER,
        Node::Dec(d) if d.exp >= 0 => INTEGER,
        Node::Dec(_) => FRACTION,
        Node::Str(_) => STRING,
        Node::Array(_) => ARRAY,
        Node::Object(_) => OBJECT,
    }
}

fn type_names(t: u8) -> String {
    let mut names = Vec::new();
    for (bit, name) in [
        (NULL, "null"),
        (BOOLEAN, "boolean"),
        (STRING, "string"),
        (ARRAY, "array"),
        (OBJECT, "object"),
    ] {
        if t & bit != 0 {
            names.push(name);
        }
    }
    match t & NUMBER {
        NUMBER => names.push("number"),
        INTEGER => names.push("integer"),
        FRACTION => names.push("number"),
        _ => {}
    }
    names.join(" or ")
}

/// A number as a mantissa and a power of ten, where the mantissa fits.
fn decimal(n: &Node) -> Option<(i128, i64)> {
    match n {
        Node::Int(i) => Some((i128::from(*i), 0)),
        Node::Dec(d) if d.len <= 36 => {
            let mut m: i128 = 0;
            for k in 0..d.len {
                m = m * 10 + i128::from(d.digit(k));
            }
            Some((if d.negative { -m } else { m }, d.exp))
        }
        _ => None,
    }
}

/// Whether `x` is an integer multiple of `m` (> 0): exactly, in
/// decimals, where both fit; else in `f64`.
fn multiple_of(x: &Node, m: &Node) -> bool {
    if let (Some((a, ea)), Some((b, eb))) = (decimal(x), decimal(m)) {
        let e = ea.min(eb);
        let scale = |v: i128, from: i64| -> Option<i128> {
            let p = u32::try_from(from - e).ok()?;
            v.checked_mul(10i128.checked_pow(p)?)
        };
        if let (Some(a), Some(b)) = (scale(a, ea), scale(b, eb))
            && b != 0
        {
            return a % b == 0;
        }
    }
    match (super::to_f64(x), super::to_f64(m)) {
        (Some(a), Some(b)) => {
            let q = a / b;
            q.is_finite() && q == q.trunc()
        }
        _ => false,
    }
}

thread_local! {
    static COMPILED: RefCell<HashMap<Vec<u8>, Arc<Schema>>> = RefCell::new(HashMap::new());
}

/// Schemas a thread keeps compiled at most.
const KEPT: usize = 64;

/// `schema` (its binary form) compiled, from the thread's cache.
pub fn compiled(schema: &[u8]) -> Result<Arc<Schema>> {
    if let Some(s) = COMPILED.with(|c| c.borrow().get(schema).cloned()) {
        return Ok(s);
    }
    let s = Arc::new(Schema::compile(schema)?);
    COMPILED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= KEPT {
            c.clear();
        }
        c.insert(schema.to_vec(), s.clone());
    });
    Ok(s)
}

fn matches_fn(a: &[Value]) -> Result<Value> {
    let s = compiled(super::bytes(&a[1])?)?;
    Ok(Value::Bool(s.matches(super::bytes(&a[0])?)?))
}

fn errors_fn(a: &[Value]) -> Result<Value> {
    let s = compiled(super::bytes(&a[1])?)?;
    Ok(Value::Array(
        s.errors(super::bytes(&a[0])?)?
            .into_iter()
            .map(Value::Text)
            .collect(),
    ))
}

/// Registers `json_matches_schema` and `json_schema_errors`.
pub(crate) fn register(r: &mut crate::ext::Registry) -> Result<()> {
    use super::funcs::{Func, u};
    let j = super::data_type();
    r.add_function(
        &["json_matches_schema", "jsonb_matches_schema"],
        Arc::new(Func::new(
            "json_matches_schema",
            vec![u(j), u(j)],
            crate::types::DataType::Bool,
            matches_fn,
        )),
    )?;
    r.add_function(
        &["json_schema_errors"],
        Arc::new(Func::new(
            "json_schema_errors",
            vec![u(j), u(j)],
            crate::types::DataType::Text.array_of().expect("text[]"),
            errors_fn,
        )),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(text: &str) -> Vec<u8> {
        super::super::parse::parse(text).expect("json")
    }

    fn errors(doc: &str, schema: &str) -> Vec<String> {
        let s = Schema::compile(&j(schema)).expect("schema");
        let e = s.errors(&j(doc)).expect("check");
        assert_eq!(
            s.matches(&j(doc)).unwrap(),
            e.is_empty(),
            "{doc} against {schema}"
        );
        e
    }

    const PERSON: &str = r#"{
        "type": "object",
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "age": {"type": "integer", "minimum": 0, "maximum": 150},
            "email": {"type": "string", "pattern": "^[^@]+@[^@]+$"},
            "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true},
            "kind": {"enum": ["a", "b"]}
        },
        "required": ["name", "age"],
        "additionalProperties": false
    }"#;

    #[test]
    fn a_fitting_document_has_no_errors() {
        assert!(
            errors(
                r#"{"name": "Ann", "age": 31, "tags": ["x"], "kind": "a"}"#,
                PERSON
            )
            .is_empty()
        );
        assert!(errors(r#"{"name": "Ann", "age": 31.0}"#, PERSON).is_empty());
    }

    #[test]
    fn each_problem_is_told_where() {
        assert_eq!(
            errors(
                r#"{"name": "", "age": -1, "tags": ["x", 2, "x"], "kind": "c", "zip": 1}"#,
                PERSON
            ),
            [
                "$.age: -1 is less than the minimum 0",
                "$: the property \"zip\" is not allowed",
                "$.kind: \"c\" is not one of \"a\", \"b\"",
                "$.name: \"\" is 0 characters long, fewer than 1",
                "$.tags: items 0 and 2 are equal",
                "$.tags[1]: expected string, got integer",
            ]
        );
        assert_eq!(
            errors(r#"{"age": 2.5, "email": "nope"}"#, PERSON),
            [
                "$: the required property \"name\" is missing",
                "$.age: expected integer, got number",
                "$.email: \"nope\" does not match the pattern ^[^@]+@[^@]+$",
            ]
        );
    }

    #[test]
    fn references_and_recursion() {
        let tree = r##"{
            "$defs": {"node": {"type": "object", "properties": {
                "v": {"type": "integer"},
                "kids": {"type": "array", "items": {"$ref": "#/$defs/node"}}}}},
            "$ref": "#/$defs/node"
        }"##;
        assert!(errors(r#"{"v": 1, "kids": [{"v": 2, "kids": [{"v": 3}]}]}"#, tree).is_empty());
        assert_eq!(
            errors(
                r#"{"v": 1, "kids": [{"v": 2, "kids": [{"v": "x"}]}]}"#,
                tree
            ),
            ["$.kids[0].kids[0].v: expected integer, got string"]
        );
        let looped = r##"{"$ref": "#"}"##;
        assert!(
            Schema::compile(&j(looped))
                .unwrap()
                .matches(&j("1"))
                .is_err()
        );
    }

    #[test]
    fn applicators() {
        let s = r#"{"anyOf": [{"type": "string"}, {"type": "integer"}]}"#;
        assert!(errors("1", s).is_empty());
        assert_eq!(
            errors("1.5", s),
            ["$: 1.5 matches none of anyOf's 2 schemas"]
        );
        let s = r#"{"oneOf": [{"multipleOf": 3}, {"multipleOf": 5}]}"#;
        assert!(errors("9", s).is_empty());
        assert_eq!(
            errors("15", s),
            ["$: 15 matches 2 of oneOf's schemas, not exactly one"]
        );
        let s = r#"{"if": {"properties": {"k": {"const": "a"}}}, "then": {"required": ["x"]}, "else": {"required": ["y"]}}"#;
        assert!(errors(r#"{"k": "a", "x": 1}"#, s).is_empty());
        assert_eq!(
            errors(r#"{"k": "b", "x": 1}"#, s),
            ["$: the required property \"y\" is missing"]
        );
        assert_eq!(
            errors("5", r#"{"not": {"type": "integer"}}"#),
            ["$: 5 matches the schema of not"]
        );
    }

    #[test]
    fn numbers_exactly() {
        assert!(errors("0.3", r#"{"multipleOf": 0.1}"#).is_empty());
        assert_eq!(
            errors("0.35", r#"{"multipleOf": 0.1}"#),
            ["$: 0.35 is not a multiple of 0.1"]
        );
        assert!(errors("1e30", r#"{"type": "integer"}"#).is_empty());
        assert_eq!(
            errors("10.000000000000000001", r#"{"exclusiveMaximum": 10}"#),
            ["$: 10.000000000000000001 is not less than 10"]
        );
    }

    #[test]
    fn arrays() {
        let s = r#"{"prefixItems": [{"type": "string"}], "items": {"type": "integer"},
                    "contains": {"const": 7}, "maxContains": 1, "minItems": 2}"#;
        assert!(errors(r#"["a", 7, 8]"#, s).is_empty());
        assert_eq!(
            errors(r#"["a", 7, 7, "b"]"#, s),
            [
                "$[3]: expected integer, got string",
                "$: 2 items match contains, more than 1"
            ]
        );
    }

    #[test]
    fn providers_and_openapi() {
        // Gemini's OpenAPI subset.
        let s = r#"{"type": "string", "nullable": true, "propertyOrdering": [], "x-note": 1}"#;
        assert!(errors("null", s).is_empty());
        assert!(errors(r#""a""#, s).is_empty());
    }

    #[test]
    fn what_is_refused() {
        for (schema, code) in [
            (r#"{"requried": ["a"]}"#, "0A000"),
            (r#"{"unevaluatedProperties": false}"#, "0A000"),
            (r##"{"$ref": "other.json#/a"}"##, "0A000"),
            (r#"{"items": [{"type": "string"}]}"#, "0A000"),
            (r#"{"type": "text"}"#, "22023"),
            (r#"{"pattern": "("}"#, "22023"),
            (r##"{"$ref": "#/$defs/none"}"##, "22023"),
            (r#"{"minLength": -1}"#, "22023"),
            ("1", "22023"),
        ] {
            let e = Schema::compile(&j(schema)).err().expect(schema);
            assert_eq!(e.code, code, "{schema}: {e}");
        }
    }
}
