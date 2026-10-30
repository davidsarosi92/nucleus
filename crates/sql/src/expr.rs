//! Bound expressions — names resolved to positions — and their evaluation.

use std::cell::RefCell;
use std::collections::HashMap;

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, UnaryOp};
use crate::datetime as dt;
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::ext::{CallContext, FnRef, Registry};
use crate::plan::Plan;
use crate::types::{DataType, Row, Value, type_name};

/// A bound expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A constant.
    Literal(Value),
    /// A column of the input row.
    Column(usize),
    /// A column of an enclosing query's row: `depth` 1 is the immediate
    /// enclosing query.
    Outer {
        /// How many queries up.
        depth: usize,
        /// The column there.
        index: usize,
    },
    /// Parameter `$n` (0-based here).
    Param(usize),
    /// `-x`, `+x`, `NOT x`.
    Unary(UnaryOp, Box<Expr>),
    /// `x op y`.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `x IS [NOT] NULL`.
    IsNull(Box<Expr>, bool),
    /// `x [NOT] IN (list)`.
    InList(Box<Expr>, Vec<Expr>, bool),
    /// `x [NOT] [I]LIKE pattern`.
    Like {
        /// The operand.
        expr: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// `NOT`.
        negated: bool,
        /// `ILIKE`.
        case_insensitive: bool,
    },
    /// `CASE`.
    Case {
        /// The operand of a simple `CASE`.
        operand: Option<Box<Expr>>,
        /// `WHEN` / `THEN`.
        whens: Vec<(Expr, Expr)>,
        /// `ELSE`.
        otherwise: Option<Box<Expr>>,
    },
    /// `CAST`.
    Cast(Box<Expr>, DataType),
    /// A call of a registered scalar function.
    Call(FnRef, Vec<Expr>),
    /// A subquery.
    Subquery(Box<SubPlan>),
    /// `ARRAY[…]`.
    Array(Vec<Expr>),
    /// `array[index]`.
    Subscript(Box<Expr>, Box<Expr>),
    /// `left op ANY|ALL (array)`.
    AnyAll {
        /// The left operand.
        left: Box<Expr>,
        /// The comparison.
        op: BinaryOp,
        /// The array.
        right: Box<Expr>,
        /// `ALL`.
        all: bool,
    },
}

/// A subquery inside an expression.
#[derive(Clone, Debug, PartialEq)]
pub struct SubPlan {
    /// What the subquery computes.
    pub kind: SubKind,
    /// The query.
    pub plan: Plan,
    /// Whether it refers to enclosing rows (else it runs once per statement).
    pub correlated: bool,
}

/// The kinds of subquery.
#[derive(Clone, Debug, PartialEq)]
pub enum SubKind {
    /// `(SELECT x …)`: one value (NULL if no row; error if several).
    Scalar,
    /// `EXISTS (…)`.
    Exists,
    /// `x [NOT] IN (SELECT …)`.
    In(Box<Expr>, bool),
    /// `ARRAY(SELECT …)`: the single column as an array.
    Array,
}

/// The built-in scalar functions ([`crate::builtin`] registers them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Func {
    /// `lower(text)`.
    Lower,
    /// `upper(text)`.
    Upper,
    /// `length(text)` (characters).
    Length,
    /// `abs(x)`.
    Abs,
    /// `coalesce(a, b, …)`.
    Coalesce,
    /// `nullif(a, b)`.
    Nullif,
    /// `substr(text, from [, count])` (1-based).
    Substr,
    /// `round(x [, digits])`.
    Round,
    /// `floor(x)`.
    Floor,
    /// `ceil(x)`.
    Ceil,
    /// `trim(text)`.
    Trim,
    /// `concat(…)` (NULLs as empty).
    Concat,
    /// `greatest(…)`.
    Greatest,
    /// `least(…)`.
    Least,
    /// `a IS DISTINCT FROM b`.
    IsDistinctFrom,
    /// `replace(text, from, to)`.
    Replace,
    /// `repeat(text, n)`.
    Repeat,
    /// `position(sub in text)` / `strpos(text, sub)`.
    Strpos,
    /// `sqrt(x)`.
    Sqrt,
    /// `power(x, y)`.
    Power,
    /// `version()`.
    Version,
    /// `trunc(x [, digits])`.
    Trunc,
    /// `sign(x)`.
    Sign,
    /// `mod(a, b)`.
    Mod,
    /// `now()`, `current_timestamp`: the transaction's start.
    Now,
    /// `clock_timestamp()`: the clock.
    ClockTimestamp,
    /// `current_date`.
    CurrentDate,
    /// `localtimestamp`.
    LocalTimestamp,
    /// `localtime`: the transaction's start, its time of day.
    LocalTime,
    /// `make_time(hour, min, sec)`.
    MakeTime,
    /// `extract(field FROM x)` (numeric).
    Extract,
    /// `date_part('field', x)` (double precision).
    DatePart,
    /// `date_trunc('field', x)`.
    DateTrunc,
    /// `make_date(y, m, d)`.
    MakeDate,
    /// `age(a [, b])`: years, months and days between.
    Age,
    /// `justify_days(interval)`.
    JustifyDays,
    /// `justify_hours(interval)`.
    JustifyHours,
    /// `justify_interval(interval)`.
    JustifyInterval,
    /// `to_timestamp(epoch seconds)`.
    ToTimestamp,
    /// A cast to `numeric(p, s)`: rounds, checks the precision.
    NumericFit,
    /// A cast to `char(n)`: truncates or pads to `n`.
    CharFit,
    /// `current_user`: the role a statement runs as.
    CurrentUser,
    /// `session_user`: the role the session logged in as.
    SessionUser,
    /// `current_setting(name [, missing_ok])`: a session setting.
    CurrentSetting,
    /// `current_database()`, `current_catalog`.
    CurrentDatabase,
    /// `current_schema()`.
    CurrentSchema,
    /// `array_to_string(array, separator [, null text])`.
    ArrayToString,
    /// `array_upper(array, dimension)`.
    ArrayUpper,
    /// `array_lower(array, dimension)`.
    ArrayLower,
    /// `array_length(array, dimension)`.
    ArrayLength,
    /// `cardinality(array)`.
    Cardinality,
    /// `array_append(array, element)`.
    ArrayAppend,
    /// `regexp_replace(text, pattern, replacement [, flags])`.
    RegexpReplace,
}

impl Func {
    /// The SQL name, for `EXPLAIN`.
    pub fn name(self) -> &'static str {
        match self {
            Func::Lower => "lower",
            Func::Upper => "upper",
            Func::Length => "length",
            Func::Abs => "abs",
            Func::Coalesce => "coalesce",
            Func::Nullif => "nullif",
            Func::Substr => "substr",
            Func::Round => "round",
            Func::Floor => "floor",
            Func::Ceil => "ceil",
            Func::Trim => "trim",
            Func::Concat => "concat",
            Func::Greatest => "greatest",
            Func::Least => "least",
            Func::IsDistinctFrom => "is_distinct_from",
            Func::Replace => "replace",
            Func::Repeat => "repeat",
            Func::Strpos => "strpos",
            Func::Sqrt => "sqrt",
            Func::Power => "power",
            Func::Version => "version",
            Func::Trunc => "trunc",
            Func::Sign => "sign",
            Func::Mod => "mod",
            Func::Now => "now",
            Func::ClockTimestamp => "clock_timestamp",
            Func::CurrentDate => "current_date",
            Func::LocalTimestamp => "localtimestamp",
            Func::LocalTime => "localtime",
            Func::MakeTime => "make_time",
            Func::Extract => "extract",
            Func::DatePart => "date_part",
            Func::DateTrunc => "date_trunc",
            Func::MakeDate => "make_date",
            Func::Age => "age",
            Func::JustifyDays => "justify_days",
            Func::JustifyHours => "justify_hours",
            Func::JustifyInterval => "justify_interval",
            Func::ToTimestamp => "to_timestamp",
            Func::NumericFit => "numeric",
            Func::CharFit => "bpchar",
            Func::CurrentUser => "current_user",
            Func::SessionUser => "session_user",
            Func::CurrentSetting => "current_setting",
            Func::CurrentDatabase => "current_database",
            Func::CurrentSchema => "current_schema",
            Func::ArrayToString => "array_to_string",
            Func::ArrayUpper => "array_upper",
            Func::ArrayLower => "array_lower",
            Func::ArrayLength => "array_length",
            Func::Cardinality => "cardinality",
            Func::ArrayAppend => "array_append",
            Func::RegexpReplace => "regexp_replace",
        }
    }
}

/// What an execution shares: the transaction, the registry, the
/// parameters, and cached results of uncorrelated subqueries.
pub struct Ctx<'a> {
    /// The transaction.
    pub txn: &'a dyn KvTxn,
    /// The session's functions and views.
    pub registry: &'a Registry,
    /// Parameter values.
    pub params: &'a [Value],
    /// Uncorrelated subquery results, by plan address.
    pub cache: RefCell<HashMap<usize, std::rc::Rc<Vec<Row>>>>,
    /// CTE results, by CTE id.
    pub ctes: RefCell<HashMap<usize, std::rc::Rc<crate::spill::Stored>>>,
    /// The current round's rows of each recursive CTE being computed.
    pub work: RefCell<HashMap<usize, std::rc::Rc<crate::spill::Stored>>>,
    /// The columns each scan of the statement's plan needs, by the scan's
    /// address ([`crate::prune`]); a scan not in it decodes every column.
    pub needed: HashMap<usize, Vec<bool>>,
    /// Table scans are validated reads (foreign key checks).
    pub validated: bool,
    /// Every node measured (`EXPLAIN ANALYZE`).
    pub profile: Option<&'a crate::profile::Profile>,
    /// Correlated subqueries' results are kept by the outer values they
    /// read (a query's: nothing it runs writes).
    pub memoize: bool,
    /// Those results, by plan address and outer values.
    pub memo: RefCell<HashMap<(usize, Vec<Value>), std::rc::Rc<Vec<Row>>>>,
    /// The outer values each correlated subquery reads (`None`: it is not
    /// kept — a volatile function, a table function), by plan address.
    pub memo_reads: RefCell<HashMap<usize, Option<Vec<(usize, usize)>>>>,
}

impl<'a> Ctx<'a> {
    /// A context.
    pub fn new(txn: &'a dyn KvTxn, registry: &'a Registry, params: &'a [Value]) -> Self {
        Self {
            txn,
            registry,
            params,
            cache: RefCell::new(HashMap::new()),
            ctes: RefCell::new(HashMap::new()),
            work: RefCell::new(HashMap::new()),
            needed: HashMap::new(),
            validated: false,
            profile: None,
            memoize: false,
            memo: RefCell::new(HashMap::new()),
            memo_reads: RefCell::new(HashMap::new()),
        }
    }
}

/// The rows an expression is evaluated against: its own, and those of the
/// enclosing queries.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    /// The current row.
    pub row: &'a [Value],
    /// The enclosing query's environment.
    pub outer: Option<&'a Env<'a>>,
    /// The execution.
    pub ctx: &'a Ctx<'a>,
}

impl Expr {
    /// Evaluates the expression.
    pub fn eval(&self, env: &Env) -> Result<Value> {
        Ok(match self {
            Expr::Literal(v) => v.clone(),
            Expr::Column(i) => env
                .row
                .get(*i)
                .cloned()
                .ok_or_else(|| Error::internal("column out of range"))?,
            Expr::Outer { depth, index } => {
                let mut e = env;
                for _ in 0..*depth {
                    e = e
                        .outer
                        .ok_or_else(|| Error::internal("outer reference without an outer row"))?;
                }
                e.row
                    .get(*index)
                    .cloned()
                    .ok_or_else(|| Error::internal("outer column out of range"))?
            }
            Expr::Param(i) => env.ctx.params.get(*i).cloned().ok_or_else(|| {
                Error::invalid_parameter(format!("there is no parameter ${}", i + 1))
            })?,
            Expr::Unary(op, e) => unary(*op, e.eval(env)?)?,
            Expr::Binary(BinaryOp::And, l, r) => {
                let a = l.eval(env)?.truth()?;
                if a == Some(false) {
                    return Ok(Value::Bool(false));
                }
                match (a, r.eval(env)?.truth()?) {
                    (_, Some(false)) => Value::Bool(false),
                    (Some(true), Some(true)) => Value::Bool(true),
                    _ => Value::Null,
                }
            }
            Expr::Binary(BinaryOp::Or, l, r) => {
                let a = l.eval(env)?.truth()?;
                if a == Some(true) {
                    return Ok(Value::Bool(true));
                }
                match (a, r.eval(env)?.truth()?) {
                    (_, Some(true)) => Value::Bool(true),
                    (Some(false), Some(false)) => Value::Bool(false),
                    _ => Value::Null,
                }
            }
            Expr::Binary(op, l, r) => binary(*op, l.eval(env)?, r.eval(env)?)?,
            Expr::IsNull(e, negated) => Value::Bool(e.eval(env)?.is_null() != *negated),
            Expr::InList(e, list, negated) => {
                let v = e.eval(env)?;
                let mut items = Vec::with_capacity(list.len());
                for item in list {
                    items.push(item.eval(env)?);
                }
                in_values(&v, items.iter(), *negated)?
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => match (expr.eval(env)?, pattern.eval(env)?) {
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                // A char(n) matches as stored, blanks included (as
                // PostgreSQL's bpchar LIKE).
                (Value::Text(s) | Value::Char(s), Value::Text(p) | Value::Char(p)) => {
                    let m = if *case_insensitive {
                        like(&s.to_lowercase(), &p.to_lowercase())
                    } else {
                        like(&s, &p)
                    };
                    Value::Bool(m != *negated)
                }
                (a, _) => {
                    return Err(Error::type_mismatch(format!(
                        "LIKE needs text, not {}",
                        type_name(&a)
                    )));
                }
            },
            Expr::Case {
                operand,
                whens,
                otherwise,
            } => {
                let op = operand.as_ref().map(|o| o.eval(env)).transpose()?;
                for (when, then) in whens {
                    let w = when.eval(env)?;
                    let hit = match &op {
                        Some(o) => o.sql_eq(&w)? == Some(true),
                        None => w.truth()? == Some(true),
                    };
                    if hit {
                        return then.eval(env);
                    }
                }
                match otherwise {
                    Some(e) => e.eval(env)?,
                    None => Value::Null,
                }
            }
            Expr::Cast(e, ty) => crate::enums::cast(env.ctx.txn, e.eval(env)?, *ty)?,
            Expr::Call(f, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(env)?);
                }
                if f.strict() && vals.iter().any(Value::is_null) {
                    Value::Null
                } else {
                    f.call(&CallContext::new(env.ctx.txn, env.ctx.registry), &vals)?
                }
            }
            Expr::Subquery(sub) => subquery(sub, env)?,
            Expr::Array(items) => {
                Value::Array(items.iter().map(|x| x.eval(env)).collect::<Result<_>>()?)
            }
            Expr::Subscript(a, i) => match (a.eval(env)?, i.eval(env)?) {
                (Value::Array(items), Value::Int(n)) if n >= 1 => items
                    .into_iter()
                    .nth((n - 1) as usize)
                    .unwrap_or(Value::Null),
                _ => Value::Null, // out of range, NULL, or not an array
            },
            Expr::AnyAll {
                left,
                op,
                right,
                all,
            } => {
                let l = left.eval(env)?;
                let items = match right.eval(env)? {
                    Value::Null => return Ok(Value::Null),
                    Value::Array(items) => items,
                    v => {
                        return Err(Error::type_mismatch(format!(
                            "op ANY/ALL (array) requires an array, not {}",
                            type_name(&v)
                        )));
                    }
                };
                // Three-valued: ANY is true on a match, ALL false on a miss;
                // a NULL comparison leaves it unknown.
                let mut unknown = false;
                for item in items {
                    match binary(*op, l.clone(), item)?.truth()? {
                        Some(b) if b != *all => return Ok(Value::Bool(b)),
                        None => unknown = true,
                        _ => {}
                    }
                }
                if unknown {
                    Value::Null
                } else {
                    Value::Bool(*all)
                }
            }
        })
    }

    /// Evaluates a condition: only `TRUE` passes (`NULL` does not).
    pub fn holds(&self, env: &Env) -> Result<bool> {
        Ok(self.eval(env)?.truth()? == Some(true))
    }

    /// Calls `f` on this expression and every expression inside it
    /// (subquery plans not included).
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        f(self);
        match self {
            Expr::Unary(_, e) | Expr::IsNull(e, _) | Expr::Cast(e, _) => e.walk(f),
            Expr::Binary(_, l, r) => {
                l.walk(f);
                r.walk(f);
            }
            Expr::InList(e, list, _) => {
                e.walk(f);
                list.iter().for_each(|x| x.walk(f));
            }
            Expr::Like { expr, pattern, .. } => {
                expr.walk(f);
                pattern.walk(f);
            }
            Expr::Case {
                operand,
                whens,
                otherwise,
            } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(o) = otherwise {
                    o.walk(f);
                }
            }
            Expr::Call(_, args) | Expr::Array(args) => args.iter().for_each(|x| x.walk(f)),
            Expr::Subscript(a, b)
            | Expr::AnyAll {
                left: a, right: b, ..
            } => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Subquery(s) => {
                if let SubKind::In(e, _) = &s.kind {
                    e.walk(f);
                }
            }
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer { .. } | Expr::Param(_) => {}
        }
    }

    /// Calls `f` on every expression inside this one, then on it (bottom
    /// up; subquery plans not entered, an `IN` subquery's operand is).
    pub fn walk_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Expr::Unary(_, e) | Expr::IsNull(e, _) | Expr::Cast(e, _) => e.walk_mut(f),
            Expr::Binary(_, l, r) => {
                l.walk_mut(f);
                r.walk_mut(f);
            }
            Expr::InList(e, list, _) => {
                e.walk_mut(f);
                list.iter_mut().for_each(|x| x.walk_mut(f));
            }
            Expr::Like { expr, pattern, .. } => {
                expr.walk_mut(f);
                pattern.walk_mut(f);
            }
            Expr::Case {
                operand,
                whens,
                otherwise,
            } => {
                if let Some(o) = operand {
                    o.walk_mut(f);
                }
                for (w, t) in whens {
                    w.walk_mut(f);
                    t.walk_mut(f);
                }
                if let Some(o) = otherwise {
                    o.walk_mut(f);
                }
            }
            Expr::Call(_, args) | Expr::Array(args) => args.iter_mut().for_each(|x| x.walk_mut(f)),
            Expr::Subscript(a, b)
            | Expr::AnyAll {
                left: a, right: b, ..
            } => {
                a.walk_mut(f);
                b.walk_mut(f);
            }
            Expr::Subquery(s) => {
                if let SubKind::In(e, _) = &mut s.kind {
                    e.walk_mut(f);
                }
            }
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer { .. } | Expr::Param(_) => {}
        }
        f(self);
    }

    /// This expression of a row, moved `by` subqueries down: its columns
    /// become references to the row, its references deeper.
    pub fn lifted(&self, by: usize) -> Expr {
        let mut e = self.clone();
        e.walk_mut(&mut |x| match x {
            Expr::Column(i) => {
                *x = Expr::Outer {
                    depth: by,
                    index: *i,
                }
            }
            Expr::Outer { depth, .. } => *depth += by,
            Expr::Subquery(s) => s.plan.deepen(1, by),
            _ => {}
        });
        e
    }

    /// The columns of this row the expression reads, those its correlated
    /// subqueries read too.
    pub fn reads(&self) -> Vec<usize> {
        let mut out = self.columns();
        self.walk(&mut |e| {
            if let Expr::Subquery(s) = e
                && s.correlated
            {
                level_refs(&s.plan, 1, &mut out);
            }
        });
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Rewrites column references (and nested expressions) bottom-up —
    /// correlated subqueries' references to this row too.
    pub fn map_columns(&self, f: &dyn Fn(usize) -> Expr) -> Expr {
        let m = |e: &Expr| Box::new(e.map_columns(f));
        match self {
            Expr::Column(i) => f(*i),
            Expr::Unary(op, e) => Expr::Unary(*op, m(e)),
            Expr::Binary(op, l, r) => Expr::Binary(*op, m(l), m(r)),
            Expr::IsNull(e, n) => Expr::IsNull(m(e), *n),
            Expr::Cast(e, t) => Expr::Cast(m(e), *t),
            Expr::InList(e, list, n) => {
                Expr::InList(m(e), list.iter().map(|x| x.map_columns(f)).collect(), *n)
            }
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => Expr::Like {
                expr: m(expr),
                pattern: m(pattern),
                negated: *negated,
                case_insensitive: *case_insensitive,
            },
            Expr::Case {
                operand,
                whens,
                otherwise,
            } => Expr::Case {
                operand: operand.as_ref().map(|o| m(o)),
                whens: whens
                    .iter()
                    .map(|(w, t)| (w.map_columns(f), t.map_columns(f)))
                    .collect(),
                otherwise: otherwise.as_ref().map(|o| m(o)),
            },
            Expr::Call(func, args) => Expr::Call(
                func.clone(),
                args.iter().map(|x| x.map_columns(f)).collect(),
            ),
            Expr::Array(items) => Expr::Array(items.iter().map(|x| x.map_columns(f)).collect()),
            Expr::Subscript(a, i) => Expr::Subscript(m(a), m(i)),
            Expr::AnyAll {
                left,
                op,
                right,
                all,
            } => Expr::AnyAll {
                left: m(left),
                op: *op,
                right: m(right),
                all: *all,
            },
            Expr::Subquery(s) => {
                // Columns inside the subquery are its own; the IN operand
                // belongs to this level, and so do its references here.
                let mut s = s.clone();
                if let SubKind::In(e, n) = &s.kind {
                    s.kind = SubKind::In(Box::new(e.map_columns(f)), *n);
                }
                if s.correlated {
                    s.plan.remap_level(1, f);
                }
                Expr::Subquery(s)
            }
            e => e.clone(),
        }
    }

    /// The columns this expression reads.
    pub fn columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Column(i) = e {
                out.push(*i);
            }
        });
        out
    }

    /// Whether the expression contains a subquery.
    pub fn has_subquery(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, Expr::Subquery(_)));
        found
    }
}

/// The columns of the row `nesting` subqueries above `plan` that it reads.
fn level_refs(plan: &Plan, nesting: usize, out: &mut Vec<usize>) {
    plan.for_each_expr(&mut |e| {
        e.walk(&mut |x| match x {
            Expr::Outer { depth, index } if *depth == nesting => out.push(*index),
            Expr::Subquery(s) => level_refs(&s.plan, nesting + 1, out),
            _ => {}
        })
    });
    for (c, below) in plan.child_levels() {
        level_refs(c, nesting + below, out);
    }
}

/// Correlated results kept per subquery at most, and rows per result.
const MEMO_ENTRIES: usize = 65_536;
const MEMO_ROWS: usize = 1024;

/// The values (level above `plan`, column) a subquery's plan reads of the
/// rows around it — 1 the row it runs for — or `None` if its result may
/// differ for the same ones.
fn outer_reads(plan: &Plan) -> Option<Vec<(usize, usize)>> {
    fn walk(plan: &Plan, nesting: usize, out: &mut Vec<(usize, usize)>, ok: &mut bool) {
        if matches!(plan, Plan::Function { .. }) {
            *ok = false;
        }
        plan.for_each_expr(&mut |e| {
            e.walk(&mut |x| match x {
                Expr::Outer { depth, index } if *depth >= nesting => {
                    out.push((depth - nesting + 1, *index));
                }
                Expr::Subquery(s) => walk(&s.plan, nesting + 1, out, ok),
                Expr::Call(f, _) if f.volatility() == crate::ext::Volatility::Volatile => {
                    *ok = false;
                }
                _ => {}
            })
        });
        for (c, below) in plan.child_levels() {
            walk(c, nesting + below, out, ok);
        }
    }
    let (mut out, mut ok) = (Vec::new(), true);
    walk(plan, 1, &mut out, &mut ok);
    out.sort_unstable();
    out.dedup();
    ok.then_some(out)
}

/// A correlated subquery's rows for `env`: kept by the outer values it
/// reads where the execution allows ([`Ctx::memoize`]).
fn correlated_rows(sub: &SubPlan, env: &Env) -> Result<std::rc::Rc<Vec<Row>>> {
    let run = || crate::exec::collect(&sub.plan, env.ctx, Some(env)).map(std::rc::Rc::new);
    if !env.ctx.memoize {
        return run();
    }
    let at = &sub.plan as *const Plan as usize;
    let reads = env
        .ctx
        .memo_reads
        .borrow_mut()
        .entry(at)
        .or_insert_with(|| outer_reads(&sub.plan))
        .clone();
    let Some(reads) = reads else { return run() };
    let mut key = Vec::with_capacity(reads.len());
    for (level, index) in reads {
        let mut e = Some(env);
        for _ in 1..level {
            e = e.and_then(|x| x.outer);
        }
        match e.and_then(|x| x.row.get(index)) {
            Some(v) => key.push(v.clone()),
            None => return run(),
        }
    }
    let key = (at, key);
    if let Some(rows) = env.ctx.memo.borrow().get(&key) {
        return Ok(rows.clone());
    }
    let rows = run()?;
    let mut memo = env.ctx.memo.borrow_mut();
    if rows.len() <= MEMO_ROWS && memo.len() < MEMO_ENTRIES {
        memo.insert(key, rows.clone());
    }
    Ok(rows)
}

fn subquery(sub: &SubPlan, env: &Env) -> Result<Value> {
    let rows = if sub.correlated {
        correlated_rows(sub, env)?
    } else {
        let key = &sub.plan as *const Plan as usize;
        let cached = env.ctx.cache.borrow().get(&key).cloned();
        match cached {
            Some(rows) => rows,
            None => {
                let rows = std::rc::Rc::new(crate::exec::collect(&sub.plan, env.ctx, Some(env))?);
                env.ctx.cache.borrow_mut().insert(key, rows.clone());
                rows
            }
        }
    };
    Ok(match &sub.kind {
        SubKind::Exists => Value::Bool(!rows.is_empty()),
        SubKind::Scalar => match rows.len() {
            0 => Value::Null,
            1 => rows[0].first().cloned().unwrap_or(Value::Null),
            _ => {
                return Err(Error::cardinality(
                    "more than one row returned by a subquery used as an expression",
                ));
            }
        },
        SubKind::In(e, negated) => {
            let v = e.eval(env)?;
            in_values(&v, rows.iter().map(|r| &r[0]), *negated)?
        }
        SubKind::Array => Value::Array(rows.iter().map(|r| r[0].clone()).collect()),
    })
}

/// A setting's value: the session's (`SET`), or a built-in parameter's.
pub fn setting(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    // The login's claims, which no SET can change.
    if let Some(claim) = lower.strip_prefix(crate::access::CLAIMS) {
        return crate::access::current().claims.get(claim).cloned();
    }
    if crate::access::CONTEXT_SETTINGS.contains(&lower.as_str()) {
        return crate::access::current().context_setting(&lower);
    }
    if let Some(v) = crate::access::current().settings.get(&lower) {
        return Some(v.clone());
    }
    Some(
        match lower.as_str() {
            "server_version" => "16.0 (nucleus)",
            "server_version_num" => "160000",
            "server_encoding" | "client_encoding" => "UTF8",
            "timezone" => "UTC",
            "datestyle" => "ISO, MDY",
            "search_path" => "\"$user\", public",
            "standard_conforming_strings" | "integer_datetimes" => "on",
            "max_identifier_length" => "63",
            "transaction_isolation" | "default_transaction_isolation" => "repeatable read",
            _ => return None,
        }
        .to_string(),
    )
}

/// `v IN (items)` with SQL's NULL rules.
fn in_values<'v>(
    v: &Value,
    items: impl Iterator<Item = &'v Value>,
    negated: bool,
) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for item in items {
        match v.sql_eq(item)? {
            Some(true) => return Ok(Value::Bool(!negated)),
            None => saw_null = true,
            Some(false) => {}
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Bool(negated)
    })
}

pub(crate) fn unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match (op, v) {
        (_, Value::Null) => Value::Null,
        (UnaryOp::Not, v) => Value::Bool(!v.truth()?.expect("not null")),
        (UnaryOp::Neg, Value::Int(i)) => Value::Int(
            i.checked_neg()
                .ok_or_else(|| Error::out_of_range("bigint out of range"))?,
        ),
        (UnaryOp::Neg, Value::Float(f)) => Value::Float(-f),
        (UnaryOp::Neg, Value::Real(f)) => Value::Real(-f),
        (UnaryOp::Neg, Value::Numeric(d)) => Value::Numeric(d.negate()),
        (UnaryOp::Neg, Value::Interval(i)) => Value::Interval(i.checked_neg()?),
        (
            UnaryOp::Plus,
            v @ (Value::Int(_)
            | Value::Real(_)
            | Value::Float(_)
            | Value::Numeric(_)
            | Value::Interval(_)),
        ) => v,
        (op, v) => {
            return Err(Error::undefined_function(format!(
                "operator does not exist: {}{}",
                if op == UnaryOp::Neg { "-" } else { "+" },
                type_name(&v)
            )));
        }
    })
}

/// A binary operator on two values (not `AND`/`OR`).
pub fn binary(op: BinaryOp, a: Value, b: Value) -> Result<Value> {
    use BinaryOp::*;
    use std::cmp::Ordering::*;
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let overflow = || Error::out_of_range("bigint out of range");
    Ok(match op {
        Eq | NotEq => {
            let eq = a.sql_eq(&b)?.expect("not null");
            Value::Bool(eq == (op == Eq))
        }
        Lt | LtEq | Gt | GtEq => {
            let o = a.sql_cmp(&b)?.expect("not null");
            Value::Bool(match op {
                Lt => o == Less,
                LtEq => o != Greater,
                Gt => o == Greater,
                _ => o != Less,
            })
        }
        Concat => match (a, b) {
            (Value::Bytea(x), Value::Bytea(y)) => Value::Bytea([&x[..], &y[..]].concat().into()),
            (Value::Array(mut x), Value::Array(y)) => {
                x.extend(y);
                Value::Array(x)
            }
            (Value::Array(mut x), e) => {
                x.push(e);
                Value::Array(x)
            }
            (e, Value::Array(mut y)) => {
                y.insert(0, e);
                Value::Array(y)
            }
            // A char(n) joins as text: without its trailing blanks.
            (a, b) => {
                let text = |v: &Value| match v {
                    Value::Char(s) => crate::types::unpadded(s).to_string(),
                    v => v.render(),
                };
                Value::Text(format!("{}{}", text(&a), text(&b)))
            }
        },
        Match | NotMatch | IMatch | NotIMatch => {
            let (Value::Text(s), Value::Text(p)) = (&a, &b) else {
                return Err(Error::undefined_function(format!(
                    "operator does not exist: {} {} {}",
                    type_name(&a),
                    op.symbol(),
                    type_name(&b)
                )));
            };
            let hit = regex_matches(p, matches!(op, IMatch | NotIMatch), s)?;
            Value::Bool(hit != matches!(op, NotMatch | NotIMatch))
        }
        Add | Sub | Mul | Div | Mod => match (a, b) {
            (Value::Int(x), Value::Int(y)) => Value::Int(match op {
                Add => x.checked_add(y).ok_or_else(overflow)?,
                Sub => x.checked_sub(y).ok_or_else(overflow)?,
                Mul => x.checked_mul(y).ok_or_else(overflow)?,
                Div if y == 0 => return Err(Error::division_by_zero("division by zero")),
                Div => x.checked_div(y).ok_or_else(overflow)?,
                _ if y == 0 => return Err(Error::division_by_zero("division by zero")),
                _ => x.checked_rem(y).unwrap_or(0),
            }),
            (x @ (Value::Int(_) | Value::Numeric(_)), y @ (Value::Int(_) | Value::Numeric(_))) => {
                let (x, y) = (as_decimal(&x), as_decimal(&y));
                Value::Numeric(match op {
                    Add => x.checked_add(y)?,
                    Sub => x.checked_sub(y)?,
                    Mul => x.checked_mul(y)?,
                    Div => x.checked_div(y)?,
                    _ => x.checked_rem(y)?,
                })
            }
            (Value::Real(x), Value::Real(y)) => {
                // real op real is computed in 32 bits, as PostgreSQL's.
                if matches!(op, Div | Mod) && y == 0.0 {
                    return Err(Error::division_by_zero("division by zero"));
                }
                let r = match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    Div => x / y,
                    _ => x % y,
                };
                check_float(
                    op,
                    r.is_infinite(),
                    r == 0.0,
                    [x.is_infinite(), y.is_infinite()],
                    [x == 0.0, y == 0.0],
                )?;
                Value::Real(r)
            }
            (x, y)
                if x.data_type().is_some_and(|t| t.is_number())
                    && y.data_type().is_some_and(|t| t.is_number()) =>
            {
                let (x, y) = (as_f64(&x), as_f64(&y));
                if matches!(op, Div | Mod) && y == 0.0 {
                    return Err(Error::division_by_zero("division by zero"));
                }
                let r = match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    Div => x / y,
                    _ => x % y,
                };
                check_float(
                    op,
                    r.is_infinite(),
                    r == 0.0,
                    [x.is_infinite(), y.is_infinite()],
                    [x == 0.0, y == 0.0],
                )?;
                Value::Float(r)
            }
            (x, y) => datetime_arith(op, x, y)?,
        },
        And | Or => unreachable!("handled with three-valued logic"),
    })
}

/// Arithmetic on dates, timestamps and intervals.
fn datetime_arith(op: BinaryOp, a: Value, b: Value) -> Result<Value> {
    use crate::datetime as dt;
    use BinaryOp::*;
    let num = |v: &Value| v.data_type().is_some_and(|t| t.is_number());
    Ok(match (op, a, b) {
        (Add, Value::Date(d), Value::Int(n)) | (Add, Value::Int(n), Value::Date(d)) => {
            Value::Date(dt::date_add_days(d, n)?)
        }
        (Sub, Value::Date(d), Value::Int(n)) => Value::Date(dt::date_add_days(
            d,
            n.checked_neg()
                .ok_or_else(|| Error::datetime_overflow("date out of range"))?,
        )?),
        (Sub, Value::Date(x), Value::Date(y)) => Value::Int(i64::from(x) - i64::from(y)),
        (Add, Value::Date(d), Value::Time(t)) | (Add, Value::Time(t), Value::Date(d)) => {
            Value::Timestamp(dt::date_to_ts(d) + t)
        }
        (Add, Value::Time(t), Value::Interval(i)) | (Add, Value::Interval(i), Value::Time(t)) => {
            Value::Time(dt::time_add(t, i))
        }
        (Sub, Value::Time(t), Value::Interval(i)) => Value::Time(dt::time_add(t, i.checked_neg()?)),
        (Sub, Value::Time(a), Value::Time(b)) => Value::Interval(dt::Interval {
            months: 0,
            days: 0,
            micros: a - b,
        }),
        (Add, Value::Interval(i), Value::Interval(j)) => Value::Interval(i.checked_add(j)?),
        (Sub, Value::Interval(i), Value::Interval(j)) => Value::Interval(i.checked_sub(j)?),
        (Add | Sub, t, Value::Interval(i)) | (Add, Value::Interval(i), t)
            if t.as_timestamp().is_some() =>
        {
            let i = if op == Sub { i.checked_neg()? } else { i };
            let ts = dt::ts_add(t.as_timestamp().expect("checked"), i)?;
            match t {
                Value::TimestampTz(_) => Value::TimestampTz(ts),
                _ => Value::Timestamp(ts),
            }
        }
        (Sub, x, y) if x.as_timestamp().is_some() && y.as_timestamp().is_some() => {
            Value::Interval(dt::ts_diff(
                x.as_timestamp().expect("checked"),
                y.as_timestamp().expect("checked"),
            )?)
        }
        (Mul, Value::Interval(i), n) | (Mul, n, Value::Interval(i)) if num(&n) => {
            Value::Interval(i.checked_mul(as_f64(&n))?)
        }
        (Div, Value::Interval(i), n) if num(&n) => Value::Interval(i.checked_div(as_f64(&n))?),
        (op, x, y) => {
            return Err(Error::undefined_function(format!(
                "operator does not exist: {} {} {}",
                type_name(&x),
                op.symbol(),
                type_name(&y)
            )));
        }
    })
}

fn as_decimal(v: &Value) -> Decimal {
    match v {
        Value::Int(i) => Decimal::from_i64(*i),
        Value::Numeric(d) => *d,
        _ => Decimal::ZERO,
    }
}

/// PostgreSQL's float result checks: an infinite result from finite
/// operands overflows; a zero product or quotient from nonzero operands
/// underflows (`22003`).
pub(crate) fn check_float(
    op: BinaryOp,
    inf: bool,
    zero: bool,
    inf_in: [bool; 2],
    zero_in: [bool; 2],
) -> Result<()> {
    use BinaryOp::*;
    let inf_ok = match op {
        Div => inf_in[0],
        _ => inf_in[0] || inf_in[1],
    };
    if inf && !inf_ok {
        return Err(crate::types::float_overflow());
    }
    let zero_ok = match op {
        Mul => zero_in[0] || zero_in[1],
        Div => zero_in[0] || inf_in[1],
        _ => true,
    };
    if zero && !zero_ok {
        return Err(crate::types::float_underflow());
    }
    Ok(())
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Real(f) => f64::from(*f),
        Value::Float(f) => *f,
        Value::Numeric(d) => d.to_f64(),
        _ => f64::NAN,
    }
}

thread_local! {
    /// Compiled patterns, by (pattern, case-insensitive); bounded.
    static REGEXES: RefCell<HashMap<(String, bool), regex::Regex>> = RefCell::new(HashMap::new());
}

/// Whether `s` contains a match of the POSIX-style regular expression `p`
/// (as PostgreSQL's `~`; `regex` crate syntax, which covers the common
/// part of PostgreSQL's: no back-references or look-around).
pub fn regex_matches(p: &str, case_insensitive: bool, s: &str) -> Result<bool> {
    with_regex(p, case_insensitive, |re| re.is_match(s))
}

/// Runs `f` with the compiled pattern.
pub fn with_regex<T>(
    p: &str,
    case_insensitive: bool,
    f: impl FnOnce(&regex::Regex) -> T,
) -> Result<T> {
    REGEXES.with(|cache| {
        let mut cache = cache.borrow_mut();
        let key = (p.to_string(), case_insensitive);
        if !cache.contains_key(&key) {
            let re = regex::RegexBuilder::new(p)
                .case_insensitive(case_insensitive)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| Error::invalid_regex(format!("invalid regular expression: {e}")))?;
            if cache.len() >= 64 {
                cache.clear();
            }
            cache.insert(key.clone(), re);
        }
        Ok(f(&cache[&key]))
    })
}

/// SQL `LIKE`: `%` any run, `_` any one character, `\` escapes.
pub fn like(s: &str, p: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = p.chars().collect();
    // Iterative matching with backtracking to the last `%`.
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if j < p.len() && p[j] == '\\' && j + 1 < p.len() && p[j + 1] == s[i] {
            i += 1;
            j += 2;
        } else if j < p.len() && (p[j] == '_' || (p[j] == s[i] && p[j] != '%' && p[j] != '\\')) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(st) = star {
            j = st + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

/// Evaluates built-in `f` (what [`crate::builtin`]'s functions call).
pub(crate) fn func(f: Func, mut args: Vec<Value>) -> Result<Value> {
    let text = |v: &Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            // A char(n) argument is text without its trailing blanks.
            Value::Char(s) => Ok(crate::types::unpadded(s).to_string()),
            v => Err(Error::undefined_function(format!(
                "function {}({}) does not exist",
                f.name(),
                type_name(v)
            ))),
        }
    };
    let int = |v: &Value| -> Result<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            Value::Float(x) => Ok(x.round_ties_even() as i64),
            Value::Numeric(d) => d.to_i64(),
            v => Err(Error::undefined_function(format!(
                "function {}({}) does not exist",
                f.name(),
                type_name(v)
            ))),
        }
    };
    match f {
        Func::Coalesce => {
            return Ok(args
                .into_iter()
                .find(|v| !v.is_null())
                .unwrap_or(Value::Null));
        }
        Func::Concat => {
            return Ok(Value::Text(
                args.iter()
                    .filter(|v| !v.is_null())
                    .map(Value::render)
                    .collect(),
            ));
        }
        Func::IsDistinctFrom => {
            let (a, b) = (&args[0], &args[1]);
            return Ok(Value::Bool(match (a.is_null(), b.is_null()) {
                (true, true) => false,
                (true, false) | (false, true) => true,
                _ => a.sql_eq(b)? != Some(true),
            }));
        }
        Func::Nullif => {
            let eq = args[0].sql_eq(&args[1])? == Some(true);
            return Ok(if eq { Value::Null } else { args.swap_remove(0) });
        }
        Func::Greatest | Func::Least => {
            let mut best: Option<Value> = None;
            for v in args.into_iter().filter(|v| !v.is_null()) {
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let o = v.sql_cmp(b)?.expect("not null");
                        if f == Func::Greatest {
                            o.is_gt()
                        } else {
                            o.is_lt()
                        }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
            return Ok(best.unwrap_or(Value::Null));
        }
        Func::Version => {
            return Ok(Value::Text(format!(
                "nucleus {}",
                env!("CARGO_PKG_VERSION")
            )));
        }
        Func::Now => return Ok(Value::TimestampTz(dt::now()?)),
        Func::ClockTimestamp => return Ok(Value::TimestampTz(dt::clock()?)),
        Func::CurrentDate => return Ok(Value::Date(dt::ts_to_date(dt::now()?))),
        Func::LocalTimestamp => return Ok(Value::Timestamp(dt::now()?)),
        Func::LocalTime => return Ok(Value::Time(dt::time_of_ts(dt::now()?))),
        Func::CurrentUser => return Ok(Value::Text(crate::access::current().user.clone())),
        Func::SessionUser => {
            return Ok(Value::Text(crate::access::current().session_user.clone()));
        }
        Func::CurrentSetting => {
            let Value::Text(name) = &args[0] else {
                return Ok(Value::Null);
            };
            let missing_ok = matches!(args.get(1), Some(Value::Bool(true)));
            return Ok(match setting(name) {
                Some(v) => Value::Text(v),
                None if missing_ok => Value::Null,
                None => {
                    return Err(Error::undefined_object(format!(
                        "unrecognized configuration parameter \"{name}\""
                    )));
                }
            });
        }
        Func::CurrentDatabase => return Ok(Value::Text("nucleus".into())),
        Func::CurrentSchema => return Ok(Value::Text("public".into())),
        _ => {}
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    Ok(match f {
        Func::Lower => Value::Text(text(&args[0])?.to_lowercase()),
        Func::Upper => Value::Text(text(&args[0])?.to_uppercase()),
        Func::Length => match &args[0] {
            Value::Bytea(b) => Value::Int(b.len() as i64),
            v => Value::Int(text(v)?.chars().count() as i64),
        },
        Func::Trim => Value::Text(text(&args[0])?.trim().to_string()),
        Func::Abs => match &args[0] {
            Value::Int(i) => Value::Int(
                i.checked_abs()
                    .ok_or_else(|| Error::out_of_range("bigint out of range"))?,
            ),
            Value::Float(x) => Value::Float(x.abs()),
            Value::Real(x) => Value::Real(x.abs()),
            Value::Numeric(d) => Value::Numeric(d.abs()),
            v => {
                return Err(Error::undefined_function(format!(
                    "function abs({}) does not exist",
                    type_name(v)
                )));
            }
        },
        Func::Substr => {
            let from = int(&args[1])?;
            let count = args.get(2).map(int).transpose()?;
            // SQL semantics: positions before 1 count against `count`.
            let start = from.max(1);
            let end = match count {
                Some(c) if c < 0 => {
                    return Err(Error::invalid_parameter(
                        "negative substring length not allowed",
                    ));
                }
                Some(c) => from.saturating_add(c).max(start),
                None => i64::MAX,
            };
            let range = |len: usize| {
                let (a, b) = (
                    (start - 1) as usize,
                    (end - 1).min(len as i64).max(0) as usize,
                );
                if a >= len || a >= b { 0..0 } else { a..b }
            };
            // Of a bytea, bytes; of text, characters.
            if let Value::Bytea(b) = &args[0] {
                return Ok(Value::Bytea(b[range(b.len())].into()));
            }
            let s: Vec<char> = text(&args[0])?.chars().collect();
            Value::Text(s[range(s.len())].iter().collect())
        }
        Func::Round => {
            let digits = args.get(1).map(int).transpose()?.unwrap_or(0);
            match &args[0] {
                Value::Int(i) if digits >= 0 => Value::Int(*i),
                Value::Numeric(d) => Value::Numeric(d.round(digits.clamp(-100, 100) as i32)?),
                v => {
                    let x = as_f64(v);
                    let m = 10f64.powi(digits as i32);
                    // Half to even, as PostgreSQL's `rint`.
                    let r = (x * m).round_ties_even() / m;
                    if matches!(v, Value::Int(_)) {
                        Value::Int(r as i64)
                    } else {
                        Value::Float(r)
                    }
                }
            }
        }
        Func::Floor | Func::Ceil => match &args[0] {
            Value::Int(i) => Value::Int(*i),
            Value::Numeric(d) => Value::Numeric(if f == Func::Floor {
                d.floor()?
            } else {
                d.ceil()?
            }),
            v => Value::Float(if f == Func::Floor {
                as_f64(v).floor()
            } else {
                as_f64(v).ceil()
            }),
        },
        Func::Replace => Value::Text(text(&args[0])?.replace(&text(&args[1])?, &text(&args[2])?)),
        Func::Repeat => {
            let s = text(&args[0])?;
            let n = usize::try_from(int(&args[1])?).unwrap_or(0);
            if s.len().saturating_mul(n) > 1 << 30 {
                return Err(Error::out_of_range("requested length too large"));
            }
            Value::Text(s.repeat(n))
        }
        Func::Strpos => {
            if let (Value::Bytea(s), Value::Bytea(sub)) = (&args[0], &args[1]) {
                let at = if sub.is_empty() {
                    Some(0)
                } else {
                    s.windows(sub.len()).position(|w| w == &sub[..])
                };
                return Ok(Value::Int(at.map_or(0, |i| i as i64 + 1)));
            }
            let (s, sub) = (text(&args[0])?, text(&args[1])?);
            Value::Int(
                s.find(&sub)
                    .map_or(0, |b| s[..b].chars().count() as i64 + 1),
            )
        }
        Func::Sqrt => {
            let x = as_f64(&args[0]);
            if x < 0.0 {
                return Err(Error::invalid_parameter(
                    "cannot take square root of a negative number",
                ));
            }
            Value::Float(x.sqrt())
        }
        Func::Power => Value::Float(as_f64(&args[0]).powf(as_f64(&args[1]))),
        Func::Trunc => {
            let digits = args.get(1).map(int).transpose()?.unwrap_or(0);
            match &args[0] {
                Value::Numeric(d) => Value::Numeric(d.trunc(digits.clamp(-100, 100) as i32)?),
                Value::Int(i) => Value::Int(*i),
                v => {
                    let m = 10f64.powi(digits as i32);
                    Value::Float((as_f64(v) * m).trunc() / m)
                }
            }
        }
        Func::Sign => match &args[0] {
            Value::Int(i) => Value::Int(i.signum()),
            Value::Numeric(d) => Value::Numeric(Decimal::from_i64(i64::from(d.signum()))),
            v => {
                let x = as_f64(v);
                Value::Float(if x == 0.0 || x.is_nan() {
                    x
                } else {
                    x.signum()
                })
            }
        },
        Func::Mod => binary(BinaryOp::Mod, args[0].clone(), args[1].clone())?,
        Func::Extract | Func::DatePart => {
            let unit = text(&args[0])?;
            let d = match &args[1] {
                Value::Date(d) => dt::extract_date(&unit, *d)?,
                Value::Timestamp(t) => dt::extract_ts(&unit, *t, false)?,
                Value::TimestampTz(t) => dt::extract_ts(&unit, *t, true)?,
                Value::Interval(i) => dt::extract_interval(&unit, *i)?,
                Value::Time(t) => dt::extract_time(&unit, *t)?,
                v => {
                    return Err(Error::undefined_function(format!(
                        "function {}(text, {}) does not exist",
                        f.name(),
                        type_name(v)
                    )));
                }
            };
            if f == Func::Extract {
                Value::Numeric(d)
            } else {
                Value::Float(d.to_f64())
            }
        }
        Func::DateTrunc => {
            let unit = text(&args[0])?;
            match &args[1] {
                Value::Timestamp(t) => Value::Timestamp(dt::date_trunc(&unit, *t)?),
                Value::TimestampTz(t) => Value::TimestampTz(dt::date_trunc(&unit, *t)?),
                Value::Date(d) => Value::TimestampTz(dt::date_trunc(&unit, dt::date_to_ts(*d))?),
                v => {
                    return Err(Error::undefined_function(format!(
                        "function date_trunc(text, {}) does not exist",
                        type_name(v)
                    )));
                }
            }
        }
        Func::MakeTime => {
            let (h, m) = (int(&args[0])?, int(&args[1])?);
            Value::Time(dt::make_time(h, m, as_f64(&args[2]))?)
        }
        Func::MakeDate => {
            let (y, m, d) = (int(&args[0])?, int(&args[1])?, int(&args[2])?);
            let field = |v: i64| u32::try_from(v).unwrap_or(0);
            Value::Date(dt::date_from_ymd(y, field(m), field(d))?)
        }
        Func::Age => {
            let ts = |v: &Value| {
                v.as_timestamp().ok_or_else(|| {
                    Error::undefined_function(format!(
                        "function age({}) does not exist",
                        type_name(v)
                    ))
                })
            };
            let (a, b) = match args.as_slice() {
                [b] => (dt::date_to_ts(dt::ts_to_date(dt::now()?)), ts(b)?),
                [a, b, ..] => (ts(a)?, ts(b)?),
                [] => unreachable!("arity checked"),
            };
            Value::Interval(dt::age(a, b))
        }
        Func::JustifyDays | Func::JustifyHours | Func::JustifyInterval => {
            let Value::Interval(i) = &args[0] else {
                return Err(Error::undefined_function(format!(
                    "function {}({}) does not exist",
                    f.name(),
                    type_name(&args[0])
                )));
            };
            Value::Interval(match f {
                Func::JustifyDays => dt::justify_days(*i),
                Func::JustifyHours => i.justify_hours(),
                _ => dt::justify_interval(*i),
            })
        }
        Func::ToTimestamp => {
            let secs = as_f64(&args[0]);
            if !secs.is_finite() {
                return Err(Error::datetime_overflow("timestamp out of range"));
            }
            Value::TimestampTz(dt::from_unix_micros((secs * 1e6).round() as i64)?)
        }
        Func::CharFit => {
            let Value::Char(s) = &args[0] else {
                return Ok(args[0].clone());
            };
            let n = u32::try_from(int(&args[1])?).unwrap_or(0);
            Value::Char(crate::types::fit_char(s, n, true)?)
        }
        Func::NumericFit => {
            let Value::Numeric(d) = &args[0] else {
                return Ok(args[0].clone());
            };
            let p = u32::try_from(int(&args[1])?).unwrap_or(0);
            let s = u32::try_from(int(&args[2])?).unwrap_or(0);
            Value::Numeric(d.fit(p, s)?)
        }
        // The rest before the NULL check above.
        _ => unreachable!("{} is evaluated above", f.name()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        for (s, p, want) in [
            ("hello", "h%", true),
            ("hello", "%llo", true),
            ("hello", "h_llo", true),
            ("hello", "h_lo", false),
            ("hello", "%", true),
            ("", "%", true),
            ("", "_", false),
            ("a%b", "a\\%b", true),
            ("axb", "a\\%b", false),
            ("abcabc", "%bc%bc", true),
            ("mississippi", "%iss%pi", true),
        ] {
            assert_eq!(like(s, p), want, "{s} LIKE {p}");
        }
    }

    #[test]
    fn arithmetic() {
        use BinaryOp::*;
        assert_eq!(
            binary(Div, Value::Int(7), Value::Int(2)).unwrap(),
            Value::Int(3)
        );
        assert_eq!(
            binary(Div, Value::Int(7), Value::Float(2.0)).unwrap(),
            Value::Float(3.5)
        );
        assert_eq!(
            binary(Add, Value::Int(i64::MAX), Value::Int(1))
                .unwrap_err()
                .code,
            "22003"
        );
        assert_eq!(
            binary(Mod, Value::Int(1), Value::Int(0)).unwrap_err().code,
            "22012"
        );
        assert_eq!(
            binary(Concat, Value::Text("a".into()), Value::Int(1)).unwrap(),
            Value::Text("a1".into())
        );
        assert!(binary(Eq, Value::Null, Value::Int(1)).unwrap().is_null());
    }
}
