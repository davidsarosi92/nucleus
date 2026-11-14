//! Evaluating a path over a json value: PostgreSQL's executor
//! (`jsonpath_exec.c`), step for step — lax mode's unwrapping and
//! wrapping and its silence about structural errors, strict mode's
//! errors, three-valued predicates, `exists`' early stop.
//!
//! Items are slices of the document (or of a variable, or of a value the
//! path made) read in place, or datetimes.  An error is soft — one the
//! `silent` argument and a predicate swallow, PostgreSQL's `RETURN_ERROR`
//! — or hard (a missing variable, a cast that needs a zone).

use std::cmp::Ordering;
use std::rc::Rc;

use super::ast::*;
use super::datetime::{self, Dt, Target};
use crate::decimal::Decimal;
use crate::error::Error;
use crate::json::{Node, form, key, ops};

/// Where an item's bytes are.
#[derive(Clone, Debug)]
enum Base<'a> {
    Doc(&'a [u8]),
    Own(Rc<[u8]>),
}

/// A json item: a range of a base, and where its outermost value starts
/// (for `.keyvalue()`'s ids).
#[derive(Clone, Debug)]
pub struct Bytes<'a> {
    base: Base<'a>,
    at: usize,
    end: usize,
    origin: usize,
    id_base: i64,
}

impl<'a> Bytes<'a> {
    fn doc(b: &'a [u8]) -> Self {
        Bytes {
            base: Base::Doc(b),
            at: 0,
            end: b.len(),
            origin: 0,
            id_base: 0,
        }
    }

    fn own(b: Vec<u8>) -> Self {
        let end = b.len();
        Bytes {
            base: Base::Own(b.into()),
            at: 0,
            end,
            origin: 0,
            id_base: 0,
        }
    }

    fn whole(&self) -> &[u8] {
        match &self.base {
            Base::Doc(b) => b,
            Base::Own(b) => b,
        }
    }

    pub fn get(&self) -> &[u8] {
        &self.whole()[self.at..self.end]
    }

    /// A part of this item (`part` a slice of [`Self::get`]).
    fn sub(&self, part: &[u8]) -> Self {
        let at = part.as_ptr() as usize - self.whole().as_ptr() as usize;
        Bytes {
            base: self.base.clone(),
            at,
            end: at + part.len(),
            origin: self.origin,
            id_base: self.id_base,
        }
    }
}

/// An item.
#[derive(Clone, Debug)]
pub enum Item<'a> {
    J(Bytes<'a>),
    D(Dt),
}

impl Item<'_> {
    /// Its json form (a datetime as its ISO 8601 string).
    pub fn to_json(&self) -> Vec<u8> {
        match self {
            Item::J(b) => b.get().to_vec(),
            Item::D(d) => ops::string(&d.text()),
        }
    }
}

fn json(b: Vec<u8>) -> Item<'static> {
    Item::J(Bytes::own(b))
}

fn num(d: Decimal) -> Item<'static> {
    json(
        ops::from_value(&crate::types::Value::Numeric(d)).unwrap_or_else(|_| vec![form::NULL << 4]),
    )
}

fn string(s: &str) -> Item<'static> {
    json(ops::string(s))
}

fn boolean(b: bool) -> Item<'static> {
    let mut v = Vec::with_capacity(1);
    form::put_bool(&mut v, b);
    json(v)
}

fn null() -> Item<'static> {
    let mut v = Vec::with_capacity(1);
    form::put_null(&mut v);
    json(v)
}

/// A failure: [`Fail::Soft`] is what the silent mode and predicates
/// swallow.
#[derive(Debug)]
pub enum Fail {
    Soft(Error),
    Hard(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Hard(e)
    }
}

type R<T> = std::result::Result<T, Fail>;

fn soft<T>(e: Error) -> R<T> {
    Err(Fail::Soft(e))
}

/// Whether a step found something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum St {
    Found,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum B3 {
    True,
    False,
    Unknown,
}

fn b3(b: bool) -> B3 {
    if b { B3::True } else { B3::False }
}

type Out<'f, 'a> = Option<&'f mut Vec<Item<'a>>>;

/// Runs `path` over `doc`; the items go to `out` (when given — without
/// it, a lax path stops at the first).
pub fn run<'a>(
    path: &'a Path,
    doc: &'a [u8],
    vars: Option<&'a [u8]>,
    use_tz: bool,
    out: Out<'_, 'a>,
) -> R<St> {
    let vars = match vars {
        Some(v) => match form::root(v)? {
            Node::Object(_) => Some(v),
            _ => {
                return Err(Fail::Hard(
                    Error::invalid_parameter("\"vars\" argument is not an object").with_hint(
                        "Jsonpath parameters should be encoded as key-value pairs of \"vars\" object.",
                    ),
                ));
            }
        },
        None => None,
    };
    let mut cx = Cx {
        strict: path.strict,
        root: doc,
        vars,
        current: Item::J(Bytes::doc(doc)),
        last: -1,
        ignore: !path.strict,
        use_tz,
    };
    if path.strict && out.is_none() {
        // Strict: every item, to see that none fails.
        let mut all = Vec::new();
        cx.expr(&path.expr, Some(&mut all))?;
        return Ok(if all.is_empty() {
            St::NotFound
        } else {
            St::Found
        });
    }
    cx.expr(&path.expr, out)
}

struct Cx<'a> {
    strict: bool,
    root: &'a [u8],
    vars: Option<&'a [u8]>,
    /// `@`
    current: Item<'a>,
    /// The innermost subscripted array's size, for `last`.
    last: i64,
    /// Whether structural errors are ignored (lax mode, under `.**`).
    ignore: bool,
    use_tz: bool,
}

/// What kind of json value an item is.
fn node<'b>(it: &'b Item<'_>) -> R<Option<Node<'b>>> {
    match it {
        Item::J(b) => Ok(Some(form::root(b.get())?)),
        Item::D(_) => Ok(None),
    }
}

fn is_array(it: &Item<'_>) -> R<bool> {
    Ok(matches!(node(it)?, Some(Node::Array(_))))
}

/// A number item's value: `None` when it is not a number.
fn numeric(it: &Item<'_>) -> R<Option<Decimal>> {
    match node(it)? {
        Some(n @ (Node::Int(_) | Node::Dec(_))) => match crate::json::to_numeric(&n) {
            Some(Ok(d)) => Ok(Some(d)),
            Some(Err(e)) => soft(e),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// A number's text, for messages.
fn text_of(it: &Item<'_>) -> String {
    match it {
        Item::J(b) => crate::json::to_text(b.get()).unwrap_or_default(),
        Item::D(d) => d.text(),
    }
}

impl<'a> Cx<'a> {
    fn lax(&self) -> bool {
        !self.strict
    }

    fn expr(&mut self, e: &'a Expr, out: Out<'_, 'a>) -> R<St> {
        match e {
            Expr::Chain(h, steps) => self.head(h, steps, out),
            e => self.head(e, &[], out),
        }
    }

    /// The rest of a chain over `v`.
    fn next(&mut self, steps: &'a [Step], v: Item<'a>, out: Out<'_, 'a>) -> R<St> {
        match steps.split_first() {
            None => {
                if let Some(o) = out {
                    o.push(v);
                }
                Ok(St::Found)
            }
            Some((s, rest)) => {
                let unwrap = self.lax();
                self.step(s, rest, v, out, unwrap)
            }
        }
    }

    fn head(&mut self, h: &'a Expr, steps: &'a [Step], out: Out<'_, 'a>) -> R<St> {
        match h {
            Expr::Root => self.next(steps, Item::J(Bytes::doc(self.root)), out),
            Expr::Current => {
                let c = self.current.clone();
                self.next(steps, c, out)
            }
            Expr::Last => {
                if self.last < 0 {
                    return Err(Fail::Hard(Error::internal(
                        "evaluating jsonpath LAST outside of array subscript",
                    )));
                }
                self.next(steps, num(Decimal::from_i64(self.last - 1)), out)
            }
            Expr::Var(name) => {
                let v = self.var(name)?;
                self.next(steps, v, out)
            }
            Expr::Null => self.next(steps, null(), out),
            Expr::Bool(b) => self.next(steps, boolean(*b), out),
            Expr::Num(_, b) | Expr::Str(_, b) => self.next(steps, Item::J(Bytes::doc(b)), out),
            Expr::Chain(..) => Err(Fail::Hard(Error::internal("a chain inside a chain"))),
            Expr::Neg(minus, a) => self.unary(*minus, a, steps, out),
            Expr::Arith(op, l, r) => self.arith(*op, l, r, steps, out),
            p => {
                let v = match self.boolean(p)? {
                    B3::True => boolean(true),
                    B3::False => boolean(false),
                    B3::Unknown => null(),
                };
                self.next(steps, v, out)
            }
        }
    }

    fn var(&self, name: &str) -> R<Item<'a>> {
        if let Some(vars) = self.vars
            && let Some(v) = ops::field(vars, name)?
        {
            let mut b = Bytes::doc(vars).sub(v);
            b.origin = b.at;
            b.id_base = 10_000_000_000;
            return Ok(Item::J(b));
        }
        Err(Fail::Hard(Error::undefined_object(format!(
            "could not find jsonpath variable \"{name}\""
        ))))
    }

    /// An expression's items, arrays unwrapped (in lax mode).
    fn unwrapped(&mut self, e: &'a Expr) -> R<Vec<Item<'a>>> {
        let mut seq = Vec::new();
        self.expr(e, Some(&mut seq))?;
        if self.strict {
            return Ok(seq);
        }
        let mut out = Vec::with_capacity(seq.len());
        for it in seq {
            match (&it, node(&it)?) {
                (Item::J(b), Some(Node::Array(a))) => {
                    for i in 0..a.len() {
                        out.push(Item::J(b.sub(a.bytes(i)?)));
                    }
                }
                _ => out.push(it),
            }
        }
        Ok(out)
    }

    /// `s` applied to each element of the array `jb` (lax mode's
    /// unwrapping), the elements not unwrapped again.
    fn unwrap_array(
        &mut self,
        s: &'a Step,
        rest: &'a [Step],
        jb: &Item<'a>,
        mut out: Out<'_, 'a>,
    ) -> R<St> {
        let (Item::J(b), Some(Node::Array(a))) = (jb, node(jb)?) else {
            return Ok(St::NotFound);
        };
        let mut res = St::NotFound;
        for i in 0..a.len() {
            let v = Item::J(b.sub(a.bytes(i)?));
            res = self.step(s, rest, v, out.as_deref_mut(), false)?;
            if res == St::Found && out.is_none() {
                break;
            }
        }
        Ok(res)
    }

    fn structural(&self, e: Error) -> R<St> {
        if self.ignore {
            Ok(St::NotFound)
        } else {
            soft(e)
        }
    }

    fn step(
        &mut self,
        s: &'a Step,
        rest: &'a [Step],
        jb: Item<'a>,
        mut out: Out<'_, 'a>,
        unwrap: bool,
    ) -> R<St> {
        match s {
            Step::Key(k) => match (&jb, node(&jb)?) {
                (Item::J(b), Some(Node::Object(o))) => match o.get_bytes(k)? {
                    Some(v) => {
                        let v = Item::J(b.sub(v));
                        self.next(rest, v, out)
                    }
                    None => self.structural(Error::json_member_not_found(format!(
                        "JSON object does not contain key \"{k}\""
                    ))),
                },
                (_, Some(Node::Array(_))) if unwrap => self.unwrap_array(s, rest, &jb, out),
                _ => self.structural(Error::json_member_not_found(
                    "jsonpath member accessor can only be applied to an object",
                )),
            },
            Step::AnyKey => match (&jb, node(&jb)?) {
                (Item::J(b), Some(Node::Object(o))) => {
                    let mut res = St::NotFound;
                    for i in 0..o.len() {
                        let v = Item::J(b.sub(o.value_bytes(i)?));
                        res = self.next(rest, v, out.as_deref_mut())?;
                        if res == St::Found && out.is_none() {
                            break;
                        }
                    }
                    Ok(res)
                }
                (_, Some(Node::Array(_))) if unwrap => self.unwrap_array(s, rest, &jb, out),
                _ => self.structural(Error::json_object_not_found(
                    "jsonpath wildcard member accessor can only be applied to an object",
                )),
            },
            Step::AnyArray => match (&jb, node(&jb)?) {
                (Item::J(b), Some(Node::Array(a))) => {
                    let mut res = St::NotFound;
                    for i in 0..a.len() {
                        let v = Item::J(b.sub(a.bytes(i)?));
                        res = self.next(rest, v, out.as_deref_mut())?;
                        if res == St::Found && out.is_none() {
                            break;
                        }
                    }
                    Ok(res)
                }
                _ if self.lax() => self.next(rest, jb, out),
                _ => self.structural(Error::json_array_not_found(
                    "jsonpath wildcard array accessor can only be applied to an array",
                )),
            },
            Step::Index(items) => self.index(items, rest, jb, out),
            Step::Any(first, last) => {
                let mut res = St::NotFound;
                if *first == 0 {
                    let saved = std::mem::replace(&mut self.ignore, true);
                    let r = self.next(rest, jb.clone(), out.as_deref_mut());
                    self.ignore = saved;
                    res = r?;
                    if res == St::Found && out.is_none() {
                        return Ok(res);
                    }
                }
                if matches!(node(&jb)?, Some(Node::Array(_) | Node::Object(_))) {
                    res = self.any(rest, &jb, out, 1, *first, *last)?;
                }
                Ok(res)
            }
            Step::Filter(p) => {
                if unwrap && is_array(&jb)? {
                    return self.unwrap_array(s, rest, &jb, out);
                }
                let saved = std::mem::replace(&mut self.current, jb.clone());
                let r = self.boolean(p);
                self.current = saved;
                if r? == B3::True {
                    self.next(rest, jb, out)
                } else {
                    Ok(St::NotFound)
                }
            }
            Step::Method(m) => self.method(s, m, rest, jb, out, unwrap),
        }
    }

    fn index(
        &mut self,
        items: &'a [(Expr, Option<Expr>)],
        rest: &'a [Step],
        jb: Item<'a>,
        out: Out<'_, 'a>,
    ) -> R<St> {
        let (size, arr) = match (&jb, node(&jb)?) {
            (Item::J(b), Some(Node::Array(a))) => (a.len() as i64, Some((b.clone(), a))),
            _ if self.lax() => (1, None),
            _ => {
                return self.structural(Error::json_array_not_found(
                    "jsonpath array accessor can only be applied to an array",
                ));
            }
        };
        let saved = std::mem::replace(&mut self.last, size);
        let r = self.subscripts(items, rest, &jb, size, arr, out);
        self.last = saved;
        r
    }

    fn subscripts(
        &mut self,
        items: &'a [(Expr, Option<Expr>)],
        rest: &'a [Step],
        jb: &Item<'a>,
        size: i64,
        arr: Option<(Bytes<'a>, crate::json::form::Array<'_>)>,
        mut out: Out<'_, 'a>,
    ) -> R<St> {
        let mut res = St::NotFound;
        for (from, to) in items {
            let f = self.subscript(from)?;
            let t = match to {
                Some(to) => self.subscript(to)?,
                None => f,
            };
            if !self.ignore && (f < 0 || f > t || t >= size) {
                return soft(Error::json_subscript(
                    "jsonpath array subscript is out of bounds",
                ));
            }
            let (f, t) = (f.max(0), t.min(size - 1));
            res = St::NotFound;
            let mut i = f;
            while i <= t {
                let v = match &arr {
                    Some((b, a)) => Item::J(b.sub(a.bytes(i as usize)?)),
                    None => jb.clone(),
                };
                if rest.is_empty() && out.is_none() {
                    return Ok(St::Found);
                }
                res = self.next(rest, v, out.as_deref_mut())?;
                if res == St::Found && out.is_none() {
                    break;
                }
                i += 1;
            }
            if res == St::Found && out.is_none() {
                break;
            }
        }
        Ok(res)
    }

    /// A subscript's value: one number, truncated to an integer.
    fn subscript(&mut self, e: &'a Expr) -> R<i64> {
        let mut found = Vec::new();
        self.expr(e, Some(&mut found))?;
        let d = match found.as_slice() {
            [one] => numeric(one)?,
            _ => None,
        };
        let Some(d) = d else {
            return soft(Error::json_subscript(
                "jsonpath array subscript is not a single numeric value",
            ));
        };
        d.trunc(0)
            .ok()
            .and_then(|d| d.to_i64().ok())
            .filter(|i| i32::try_from(*i).is_ok())
            .map_or_else(
                || {
                    soft(Error::json_subscript(
                        "jsonpath array subscript is out of integer range",
                    ))
                },
                Ok,
            )
    }

    /// `.**`'s levels below `jb`.
    fn any(
        &mut self,
        rest: &'a [Step],
        jb: &Item<'a>,
        mut out: Out<'_, 'a>,
        level: u32,
        first: u32,
        last: u32,
    ) -> R<St> {
        let mut res = St::NotFound;
        if level > last {
            return Ok(res);
        }
        let Item::J(b) = jb else {
            return Ok(res);
        };
        let children: Vec<&[u8]> = match form::root(b.get())? {
            Node::Array(a) => (0..a.len()).map(|i| a.bytes(i)).collect::<Result<_, _>>()?,
            Node::Object(o) => (0..o.len())
                .map(|i| o.value_bytes(i))
                .collect::<Result<_, _>>()?,
            _ => Vec::new(),
        };
        for c in children {
            let v = Item::J(b.sub(c));
            let container = matches!(form::root(c)?, Node::Array(_) | Node::Object(_));
            if level >= first || (first == LAST && last == LAST && !container) {
                if rest.is_empty() {
                    match out.as_deref_mut() {
                        Some(o) => o.push(v.clone()),
                        None => return Ok(St::Found),
                    }
                } else {
                    let saved = std::mem::replace(&mut self.ignore, true);
                    let r = self.next(rest, v.clone(), out.as_deref_mut());
                    self.ignore = saved;
                    res = r?;
                    if res == St::Found && out.is_none() {
                        break;
                    }
                }
            }
            if level < last && container {
                res = self.any(rest, &v, out.as_deref_mut(), level + 1, first, last)?;
                if res == St::Found && out.is_none() {
                    break;
                }
            }
        }
        Ok(res)
    }

    fn method(
        &mut self,
        s: &'a Step,
        m: &'a Method,
        rest: &'a [Step],
        jb: Item<'a>,
        out: Out<'_, 'a>,
        unwrap: bool,
    ) -> R<St> {
        let name = m.name();
        match m {
            Method::Type => {
                let t = match &jb {
                    Item::D(d) => d.type_name(),
                    Item::J(b) => crate::json::kind(&form::root(b.get())?),
                };
                return self.next(rest, string(t), out);
            }
            Method::Size => {
                let size = match node(&jb)? {
                    Some(Node::Array(a)) => a.len() as i64,
                    _ if self.lax() => 1,
                    _ => {
                        return self.structural(Error::json_array_not_found(format!(
                            "jsonpath item method .{name}() can only be applied to an array"
                        )));
                    }
                };
                return self.next(rest, num(Decimal::from_i64(size)), out);
            }
            _ => {}
        }
        if unwrap && is_array(&jb)? {
            return self.unwrap_array(s, rest, &jb, out);
        }
        let non_numeric = |what: &str| {
            soft(Error::json_non_numeric(format!(
                "jsonpath item method .{name}() can only be applied to {what}"
            )))
        };
        let invalid = |arg: &str, ty: &str| {
            soft(Error::json_non_numeric(format!(
                "argument \"{arg}\" of jsonpath item method .{name}() is invalid for type {ty}"
            )))
        };
        let nan = || {
            soft(Error::json_non_numeric(format!(
                "NaN or Infinity is not allowed for jsonpath item method .{name}()"
            )))
        };
        let as_str = str_of(&jb)?;
        let v: Item<'a> = match m {
            Method::Abs | Method::Floor | Method::Ceiling => {
                let Some(d) = numeric(&jb)? else {
                    return non_numeric("a numeric value");
                };
                let d = match m {
                    Method::Abs => d.abs(),
                    Method::Floor => d.floor().map_err(Fail::Soft)?,
                    _ => d.ceil().map_err(Fail::Soft)?,
                };
                if rest.is_empty() && out.is_none() {
                    return Ok(St::Found);
                }
                num(d)
            }
            Method::Double => {
                if let Some(n) = node(&jb)?.filter(|n| matches!(n, Node::Int(_) | Node::Dec(_))) {
                    match crate::json::to_f64(&n) {
                        Some(f) if f.is_finite() => jb,
                        _ => return invalid(&text_of(&jb), "double precision"),
                    }
                } else if let Some(s) = as_str {
                    let f: f64 = match s.trim().parse() {
                        Ok(f) => f,
                        Err(_) => return invalid(s, "double precision"),
                    };
                    if !f.is_finite() {
                        return nan();
                    }
                    num(Decimal::from_f64(f).map_err(Fail::Soft)?)
                } else {
                    return non_numeric("a string or numeric value");
                }
            }
            Method::KeyValue => {
                let (Item::J(b), Some(Node::Object(o))) = (&jb, node(&jb)?) else {
                    return soft(Error::json_object_not_found(format!(
                        "jsonpath item method .{name}() can only be applied to an object"
                    )));
                };
                let id = b.id_base + (b.at - b.origin) as i64;
                let mut res = St::NotFound;
                let mut out = out;
                for i in 0..o.len() {
                    let pair = ops::object_of(vec![
                        ("id", ops::from_value(&crate::types::Value::Int(id))?),
                        ("key", ops::string(o.key(i)?)),
                        ("value", o.value_bytes(i)?.to_vec()),
                    ])?;
                    res = self.next(rest, json(pair), out.as_deref_mut())?;
                    if res == St::Found && out.is_none() {
                        break;
                    }
                }
                return Ok(res);
            }
            Method::BigInt | Method::Integer => {
                let (ty, lo, hi) = if matches!(m, Method::BigInt) {
                    ("bigint", i64::MIN, i64::MAX)
                } else {
                    ("integer", i64::from(i32::MIN), i64::from(i32::MAX))
                };
                let v = if let Some(d) = numeric(&jb).ok().flatten() {
                    d.round(0).ok().and_then(|d| d.to_i64().ok())
                } else if node(&jb)?.is_some_and(|n| matches!(n, Node::Int(_) | Node::Dec(_))) {
                    None
                } else if let Some(s) = as_str {
                    parse_int(s)
                } else {
                    return non_numeric("a string or numeric value");
                };
                match v.filter(|v| (lo..=hi).contains(v)) {
                    Some(v) => num(Decimal::from_i64(v)),
                    None => {
                        return invalid(
                            &as_str.map_or_else(|| text_of(&jb), |s| s.to_string()),
                            ty,
                        );
                    }
                }
            }
            Method::Boolean => match node(&jb)? {
                Some(Node::Bool(_)) => jb,
                Some(Node::Int(i)) if i32::try_from(i).is_ok() => boolean(i != 0),
                Some(Node::Int(_) | Node::Dec(_)) => return invalid(&text_of(&jb), "boolean"),
                Some(Node::Str(_)) => {
                    let s = as_str.unwrap_or_default();
                    match parse_bool(s) {
                        Some(b) => boolean(b),
                        None => return invalid(s, "boolean"),
                    }
                }
                _ => return non_numeric("a boolean, string, or numeric value"),
            },
            Method::Number | Method::Decimal(..) => {
                let d = match node(&jb)? {
                    Some(Node::Int(_) | Node::Dec(_)) => {
                        if matches!(m, Method::Number | Method::Decimal(None, _)) {
                            if rest.is_empty() && out.is_none() {
                                return Ok(St::Found);
                            }
                            return self.next(rest, jb, out);
                        }
                        numeric(&jb)?.expect("a number")
                    }
                    Some(Node::Str(_)) => {
                        let s = as_str.unwrap_or_default();
                        let t = s.trim().to_ascii_lowercase();
                        if matches!(t.trim_start_matches(['+', '-']), "nan" | "inf" | "infinity") {
                            return nan();
                        }
                        match Decimal::parse(s) {
                            Ok(d) => d,
                            Err(_) => return invalid(s, "numeric"),
                        }
                    }
                    _ => return non_numeric("a string or numeric value"),
                };
                let d = match m {
                    Method::Decimal(Some(p), s) => {
                        let s = s.unwrap_or(0);
                        if !(1..=1000).contains(p) {
                            return Err(Fail::Hard(Error::invalid_parameter(format!(
                                "NUMERIC precision {p} must be between 1 and 1000"
                            ))));
                        }
                        if !(-1000..=1000).contains(&s) {
                            return Err(Fail::Hard(Error::invalid_parameter(format!(
                                "NUMERIC scale {s} must be between -1000 and 1000"
                            ))));
                        }
                        match fit(d, *p, s) {
                            Some(d) => d,
                            None => {
                                return invalid(
                                    &as_str.map_or_else(|| text_of(&jb), |s| s.to_string()),
                                    "numeric",
                                );
                            }
                        }
                    }
                    _ => d,
                };
                num(d)
            }
            Method::String => match (&jb, node(&jb)?) {
                (Item::D(d), _) => string(&d.text()),
                (_, Some(Node::Str(_))) => jb,
                (_, Some(Node::Int(_) | Node::Dec(_) | Node::Bool(_))) => string(&text_of(&jb)),
                _ => return non_numeric("a boolean, string, numeric, or datetime value"),
            },
            Method::Datetime(_)
            | Method::Date
            | Method::Time(_)
            | Method::TimeTz(_)
            | Method::Timestamp(_)
            | Method::TimestampTz(_) => {
                let Some(text) = as_str.filter(|_| matches!(jb, Item::J(_))) else {
                    return soft(Error::json_datetime(format!(
                        "jsonpath item method .{name}() can only be applied to a string"
                    )));
                };
                let (template, target, precision) = match m {
                    Method::Datetime(t) => (t.as_deref(), Target::Any, None),
                    Method::Date => (None, Target::Date, None),
                    Method::Time(p) => (None, Target::Time, *p),
                    Method::TimeTz(p) => (None, Target::TimeTz, *p),
                    Method::Timestamp(p) => (None, Target::Timestamp, *p),
                    _ => (None, Target::TimestampTz, precision_of(m)),
                };
                match datetime::method(text, template, target, precision, self.use_tz) {
                    Ok(d) => Item::D(d),
                    Err(Ok(e)) => return soft(e),
                    Err(Err(e)) => return Err(Fail::Hard(e)),
                }
            }
            Method::Type | Method::Size => unreachable!("handled above"),
        };
        self.next(rest, v, out)
    }

    /// Unary `-` or `+` over each item.
    fn unary(
        &mut self,
        minus: bool,
        a: &'a Expr,
        steps: &'a [Step],
        mut out: Out<'_, 'a>,
    ) -> R<St> {
        let seq = self.unwrapped(a)?;
        let mut res = St::NotFound;
        for v in seq {
            let d = numeric(&v);
            let is_num = matches!(d, Ok(Some(_)) | Err(_));
            if !is_num {
                if steps.is_empty() && out.is_none() {
                    continue;
                }
                return soft(Error::json_number_not_found(format!(
                    "operand of unary jsonpath operator {} is not a numeric value",
                    if minus { "-" } else { "+" }
                )));
            }
            if steps.is_empty() && out.is_none() {
                return Ok(St::Found);
            }
            let v = if minus {
                num(d?.expect("a number").negate())
            } else {
                v
            };
            let r = self.next(steps, v, out.as_deref_mut())?;
            if r == St::Found {
                if out.is_none() {
                    return Ok(r);
                }
                res = r;
            }
        }
        Ok(res)
    }

    fn arith(
        &mut self,
        op: Arith,
        l: &'a Expr,
        r: &'a Expr,
        steps: &'a [Step],
        out: Out<'_, 'a>,
    ) -> R<St> {
        let sym = match op {
            Arith::Add => "+",
            Arith::Sub => "-",
            Arith::Mul => "*",
            Arith::Div => "/",
            Arith::Mod => "%",
        };
        let ls = self.unwrapped(l)?;
        let rs = self.unwrapped(r)?;
        let single = |s: &[Item<'_>], side: &str| -> R<Decimal> {
            if let [one] = s
                && let Some(d) = numeric(one)?
            {
                return Ok(d);
            }
            soft(Error::json_singleton_required(format!(
                "{side} operand of jsonpath operator {sym} is not a single numeric value"
            )))
        };
        let a = single(&ls, "left")?;
        let b = single(&rs, "right")?;
        let v = match op {
            Arith::Add => a.checked_add(b),
            Arith::Sub => a.checked_sub(b),
            Arith::Mul => a.checked_mul(b),
            Arith::Div => a.checked_div(b),
            Arith::Mod => a.checked_rem(b),
        }
        .map_err(Fail::Soft)?;
        if steps.is_empty() && out.is_none() {
            return Ok(St::Found);
        }
        self.next(steps, num(v), out)
    }

    /// A predicate's value; only hard errors escape.
    fn boolean(&mut self, e: &'a Expr) -> R<B3> {
        Ok(match e {
            Expr::And(l, r) => {
                let a = self.boolean(l)?;
                if a == B3::False {
                    return Ok(a);
                }
                let b = self.boolean(r)?;
                if b == B3::True { a } else { b }
            }
            Expr::Or(l, r) => {
                let a = self.boolean(l)?;
                if a == B3::True {
                    return Ok(a);
                }
                let b = self.boolean(r)?;
                if b == B3::False { a } else { b }
            }
            Expr::Not(a) => match self.boolean(a)? {
                B3::True => B3::False,
                B3::False => B3::True,
                B3::Unknown => B3::Unknown,
            },
            Expr::IsUnknown(a) => b3(self.boolean(a)? == B3::Unknown),
            Expr::Cmp(op, l, r) => self.predicate(l, Some(r), true, |cx, a, b| {
                cx.compare(*op, a, b.expect("a right operand"))
            })?,
            Expr::StartsWith(l, r) => self.predicate(l, Some(r), false, |_, a, b| {
                let b = b.expect("a right operand");
                Ok(match (str_of(a)?, str_of(b)?) {
                    (Some(a), Some(b)) => b3(a.starts_with(b)),
                    _ => B3::Unknown,
                })
            })?,
            Expr::LikeRegex(l, re) => self.predicate(l, None, false, |_, a, _| {
                Ok(match str_of(a)? {
                    Some(s) => b3(re.re.is_match(s)),
                    None => B3::Unknown,
                })
            })?,
            Expr::Exists(a) => {
                if self.strict {
                    let mut all = Vec::new();
                    match self.expr(a, Some(&mut all)) {
                        Ok(_) => b3(!all.is_empty()),
                        Err(Fail::Soft(_)) => B3::Unknown,
                        Err(e) => return Err(e),
                    }
                } else {
                    match self.expr(a, None) {
                        Ok(st) => b3(st == St::Found),
                        Err(Fail::Soft(_)) => B3::Unknown,
                        Err(e) => return Err(e),
                    }
                }
            }
            _ => return Err(Fail::Hard(Error::internal("not a jsonpath predicate"))),
        })
    }

    /// A predicate over each pair of items: lax mode answers at the first
    /// true, strict mode at the first unknown; an error is unknown.
    fn predicate(
        &mut self,
        l: &'a Expr,
        r: Option<&'a Expr>,
        unwrap_right: bool,
        f: impl Fn(&Self, &Item<'a>, Option<&Item<'a>>) -> R<B3>,
    ) -> R<B3> {
        let ls = match self.unwrapped(l) {
            Ok(v) => v,
            Err(Fail::Soft(_)) => return Ok(B3::Unknown),
            Err(e) => return Err(e),
        };
        let rs = match r {
            Some(r) => {
                let res = if unwrap_right {
                    self.unwrapped(r)
                } else {
                    let mut v = Vec::new();
                    self.expr(r, Some(&mut v)).map(|_| v)
                };
                match res {
                    Ok(v) => Some(v),
                    Err(Fail::Soft(_)) => return Ok(B3::Unknown),
                    Err(e) => return Err(e),
                }
            }
            None => None,
        };
        let (mut error, mut found) = (false, false);
        for a in &ls {
            let rights: Vec<Option<&Item<'a>>> = match &rs {
                Some(rs) => rs.iter().map(Some).collect(),
                None => vec![None],
            };
            for b in rights {
                match f(self, a, b)? {
                    B3::Unknown => {
                        if self.strict {
                            return Ok(B3::Unknown);
                        }
                        error = true;
                    }
                    B3::True => {
                        if !self.strict {
                            return Ok(B3::True);
                        }
                        found = true;
                    }
                    B3::False => {}
                }
            }
        }
        Ok(if found {
            B3::True
        } else if error {
            B3::Unknown
        } else {
            B3::False
        })
    }

    /// `a op b`: items of different kinds are unknown (but null is not
    /// equal to anything else), containers too.
    fn compare(&self, op: Cmp, a: &Item<'a>, b: &Item<'a>) -> R<B3> {
        let apply = |o: Ordering| {
            b3(match op {
                Cmp::Eq => o == Ordering::Equal,
                Cmp::Ne => o != Ordering::Equal,
                Cmp::Lt => o == Ordering::Less,
                Cmp::Le => o != Ordering::Greater,
                Cmp::Gt => o == Ordering::Greater,
                Cmp::Ge => o != Ordering::Less,
            })
        };
        let null_vs_other = || b3(op == Cmp::Ne);
        let o = match (a, b) {
            (Item::D(x), Item::D(y)) => match datetime::compare(*x, *y, self.use_tz)? {
                Some(o) => o,
                None => return Ok(B3::Unknown),
            },
            (Item::D(_), j) | (j, Item::D(_)) => {
                return Ok(if matches!(node(j)?, Some(Node::Null)) {
                    null_vs_other()
                } else {
                    B3::Unknown
                });
            }
            (Item::J(x), Item::J(y)) => {
                let (nx, ny) = (form::root(x.get())?, form::root(y.get())?);
                match (&nx, &ny) {
                    (Node::Null, Node::Null) => Ordering::Equal,
                    (Node::Null, _) | (_, Node::Null) => return Ok(null_vs_other()),
                    (Node::Bool(p), Node::Bool(q)) => p.cmp(q),
                    (Node::Int(_) | Node::Dec(_), Node::Int(_) | Node::Dec(_)) => {
                        key::compare(x.get(), y.get())?
                    }
                    (Node::Str(p), Node::Str(q)) => p.cmp(q),
                    _ => return Ok(B3::Unknown),
                }
            }
        };
        Ok(apply(o))
    }
}

fn str_of<'b>(it: &'b Item<'_>) -> R<Option<&'b str>> {
    Ok(match node(it)? {
        Some(Node::Str(s)) => Some(std::str::from_utf8(s).map_err(|_| form::malformed())?),
        _ => None,
    })
}

fn precision_of(m: &Method) -> Option<i32> {
    match m {
        Method::Time(p) | Method::TimeTz(p) | Method::Timestamp(p) | Method::TimestampTz(p) => *p,
        _ => None,
    }
}

/// `int8in`: spaces around, a sign, digits (`_` between two).
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|c| c.is_ascii_digit() || c == b'_')
    {
        return None;
    }
    let mut v: i64 = 0;
    for c in digits.bytes().filter(u8::is_ascii_digit) {
        v = v.checked_mul(10)?.checked_sub(i64::from(c - b'0'))?;
    }
    if neg { Some(v) } else { v.checked_neg() }
}

/// `parse_bool`: `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0` and
/// their unambiguous prefixes, in any case.
fn parse_bool(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    let prefix = |word: &str| !l.is_empty() && word.starts_with(l.as_str());
    match l.as_bytes().first()? {
        b't' if prefix("true") => Some(true),
        b'f' if prefix("false") => Some(false),
        b'y' if prefix("yes") => Some(true),
        b'n' if prefix("no") => Some(false),
        b'o' if l == "on" => Some(true),
        b'o' if l.len() >= 2 && prefix("off") => Some(false),
        b'1' if l.len() == 1 => Some(true),
        b'0' if l.len() == 1 => Some(false),
        _ => None,
    }
}

/// `numeric(precision, scale)`: rounded to `scale` digits (a negative
/// scale rounds left of the point); `None` if it does not fit.
fn fit(d: Decimal, precision: i32, scale: i32) -> Option<Decimal> {
    let r = d.round(scale).ok()?;
    let int_digits = if r.mantissa() == 0 {
        0
    } else {
        let mag = r.mantissa().unsigned_abs();
        let digits = mag.checked_ilog10().map_or(1, |l| l as i64 + 1);
        digits - i64::from(r.scale())
    };
    (int_digits <= i64::from(precision) - i64::from(scale)).then_some(r)
}
