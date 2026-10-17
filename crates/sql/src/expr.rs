//! Bound expressions — names resolved to positions — and their evaluation.

use std::cell::RefCell;
use std::collections::HashMap;

use nucleus_kv::KvTxn;

use crate::ast::{BinaryOp, UnaryOp};
use crate::datetime as dt;
use crate::decimal::Decimal;
use crate::error::{Error, Result};
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
    /// A scalar function.
    Func(Func, Vec<Expr>),
    /// A subquery.
    Subquery(Box<SubPlan>),
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
}

/// Scalar functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// `current_user`, `session_user`.
    CurrentUser,
    /// `current_database()`, `current_catalog`.
    CurrentDatabase,
    /// `current_schema()`.
    CurrentSchema,
}

impl Func {
    /// The function called `name`, with its accepted argument counts.
    pub fn lookup(name: &str) -> Option<(Func, std::ops::RangeInclusive<usize>)> {
        Some(match name {
            "lower" => (Func::Lower, 1..=1),
            "upper" => (Func::Upper, 1..=1),
            "length" | "char_length" | "character_length" => (Func::Length, 1..=1),
            "abs" => (Func::Abs, 1..=1),
            "coalesce" => (Func::Coalesce, 1..=usize::MAX),
            "nullif" => (Func::Nullif, 2..=2),
            "substr" | "substring" => (Func::Substr, 2..=3),
            "round" => (Func::Round, 1..=2),
            "floor" => (Func::Floor, 1..=1),
            "ceil" | "ceiling" => (Func::Ceil, 1..=1),
            "trim" | "btrim" => (Func::Trim, 1..=1),
            "concat" => (Func::Concat, 1..=usize::MAX),
            "greatest" => (Func::Greatest, 1..=usize::MAX),
            "least" => (Func::Least, 1..=usize::MAX),
            "__is_distinct_from" => (Func::IsDistinctFrom, 2..=2),
            "replace" => (Func::Replace, 3..=3),
            "strpos" => (Func::Strpos, 2..=2),
            "sqrt" => (Func::Sqrt, 1..=1),
            "power" | "pow" => (Func::Power, 2..=2),
            "version" => (Func::Version, 0..=0),
            "trunc" => (Func::Trunc, 1..=2),
            "sign" => (Func::Sign, 1..=1),
            "mod" => (Func::Mod, 2..=2),
            "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp" => {
                (Func::Now, 0..=0)
            }
            "clock_timestamp" => (Func::ClockTimestamp, 0..=0),
            "current_date" => (Func::CurrentDate, 0..=0),
            "localtimestamp" => (Func::LocalTimestamp, 0..=0),
            "extract" => (Func::Extract, 2..=2),
            "date_part" => (Func::DatePart, 2..=2),
            "date_trunc" => (Func::DateTrunc, 2..=2),
            "make_date" => (Func::MakeDate, 3..=3),
            "age" => (Func::Age, 1..=2),
            "justify_days" => (Func::JustifyDays, 1..=1),
            "justify_hours" => (Func::JustifyHours, 1..=1),
            "justify_interval" => (Func::JustifyInterval, 1..=1),
            "to_timestamp" => (Func::ToTimestamp, 1..=1),
            "__numeric_fit" => (Func::NumericFit, 3..=3),
            "current_user" | "session_user" | "user" | "current_role" => (Func::CurrentUser, 0..=0),
            "current_database" | "current_catalog" => (Func::CurrentDatabase, 0..=0),
            "current_schema" => (Func::CurrentSchema, 0..=0),
            _ => return None,
        })
    }

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
            Func::CurrentUser => "current_user",
            Func::CurrentDatabase => "current_database",
            Func::CurrentSchema => "current_schema",
        }
    }
}

/// What an execution shares: the transaction, the parameters, and cached
/// results of uncorrelated subqueries.
pub struct Ctx<'a> {
    /// The transaction.
    pub txn: &'a dyn KvTxn,
    /// Parameter values.
    pub params: &'a [Value],
    /// Uncorrelated subquery results, by plan address.
    pub cache: RefCell<HashMap<usize, std::rc::Rc<Vec<Row>>>>,
    /// CTE results, by CTE id.
    pub ctes: RefCell<HashMap<usize, std::rc::Rc<Vec<Row>>>>,
    /// The current round's rows of each recursive CTE being computed.
    pub work: RefCell<HashMap<usize, std::rc::Rc<Vec<Row>>>>,
}

impl<'a> Ctx<'a> {
    /// A context.
    pub fn new(txn: &'a dyn KvTxn, params: &'a [Value]) -> Self {
        Self {
            txn,
            params,
            cache: RefCell::new(HashMap::new()),
            ctes: RefCell::new(HashMap::new()),
            work: RefCell::new(HashMap::new()),
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
                (Value::Text(s), Value::Text(p)) => {
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
                        Some(o) => o.sql_cmp(&w)? == Some(std::cmp::Ordering::Equal),
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
            Expr::Cast(e, ty) => e.eval(env)?.cast(*ty)?,
            Expr::Func(f, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(a.eval(env)?);
                }
                func(*f, vals)?
            }
            Expr::Subquery(sub) => subquery(sub, env)?,
        })
    }

    /// Evaluates a condition: only `TRUE` passes (`NULL` does not).
    pub fn holds(&self, env: &Env) -> Result<bool> {
        Ok(self.eval(env)?.truth()? == Some(true))
    }

    /// Calls `f` on this expression and every expression inside it
    /// (subquery plans not included).
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
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
            Expr::Func(_, args) => args.iter().for_each(|x| x.walk(f)),
            Expr::Subquery(s) => {
                if let SubKind::In(e, _) = &s.kind {
                    e.walk(f);
                }
            }
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer { .. } | Expr::Param(_) => {}
        }
    }

    /// Rewrites column references (and nested expressions) bottom-up.
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
            Expr::Func(func, args) => {
                Expr::Func(*func, args.iter().map(|x| x.map_columns(f)).collect())
            }
            Expr::Subquery(s) => {
                // Columns inside the subquery are its own; only the IN operand
                // belongs to this level.
                let mut s = s.clone();
                if let SubKind::In(e, n) = &s.kind {
                    s.kind = SubKind::In(Box::new(e.map_columns(f)), *n);
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

fn subquery(sub: &SubPlan, env: &Env) -> Result<Value> {
    let rows = if sub.correlated {
        std::rc::Rc::new(crate::exec::collect(&sub.plan, env.ctx, Some(env))?)
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
    })
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
        match v.sql_cmp(item)? {
            Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(!negated)),
            None => saw_null = true,
            _ => {}
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Bool(negated)
    })
}

fn unary(op: UnaryOp, v: Value) -> Result<Value> {
    Ok(match (op, v) {
        (_, Value::Null) => Value::Null,
        (UnaryOp::Not, v) => Value::Bool(!v.truth()?.expect("not null")),
        (UnaryOp::Neg, Value::Int(i)) => Value::Int(
            i.checked_neg()
                .ok_or_else(|| Error::out_of_range("bigint out of range"))?,
        ),
        (UnaryOp::Neg, Value::Float(f)) => Value::Float(-f),
        (UnaryOp::Neg, Value::Numeric(d)) => Value::Numeric(d.negate()),
        (UnaryOp::Neg, Value::Interval(i)) => Value::Interval(i.checked_neg()?),
        (
            UnaryOp::Plus,
            v @ (Value::Int(_) | Value::Float(_) | Value::Numeric(_) | Value::Interval(_)),
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
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let o = a.sql_cmp(&b)?.expect("not null");
            Value::Bool(match op {
                Eq => o == Equal,
                NotEq => o != Equal,
                Lt => o == Less,
                LtEq => o != Greater,
                Gt => o == Greater,
                _ => o != Less,
            })
        }
        Concat => Value::Text(format!("{}{}", a.render(), b.render())),
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
            (x, y)
                if x.data_type().is_some_and(|t| t.is_number())
                    && y.data_type().is_some_and(|t| t.is_number()) =>
            {
                let (x, y) = (as_f64(&x), as_f64(&y));
                if matches!(op, Div | Mod) && y == 0.0 {
                    return Err(Error::division_by_zero("division by zero"));
                }
                Value::Float(match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    Div => x / y,
                    _ => x % y,
                })
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

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Numeric(d) => d.to_f64(),
        _ => f64::NAN,
    }
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

fn func(f: Func, mut args: Vec<Value>) -> Result<Value> {
    let text = |v: &Value| -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
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
            Value::Float(x) => Ok(x.round() as i64),
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
                _ => a.sql_cmp(b)? != Some(std::cmp::Ordering::Equal),
            }));
        }
        Func::Nullif => {
            let eq = args[0].sql_cmp(&args[1])? == Some(std::cmp::Ordering::Equal);
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
        Func::Now => return Ok(Value::TimestampTz(dt::now())),
        Func::ClockTimestamp => return Ok(Value::TimestampTz(dt::clock())),
        Func::CurrentDate => return Ok(Value::Date(dt::ts_to_date(dt::now()))),
        Func::LocalTimestamp => return Ok(Value::Timestamp(dt::now())),
        Func::CurrentUser => return Ok(Value::Text("nucleus".into())),
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
        Func::Length => Value::Int(text(&args[0])?.chars().count() as i64),
        Func::Trim => Value::Text(text(&args[0])?.trim().to_string()),
        Func::Abs => match &args[0] {
            Value::Int(i) => Value::Int(
                i.checked_abs()
                    .ok_or_else(|| Error::out_of_range("bigint out of range"))?,
            ),
            Value::Float(x) => Value::Float(x.abs()),
            Value::Numeric(d) => Value::Numeric(d.abs()),
            v => {
                return Err(Error::undefined_function(format!(
                    "function abs({}) does not exist",
                    type_name(v)
                )));
            }
        },
        Func::Substr => {
            let s: Vec<char> = text(&args[0])?.chars().collect();
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
            let (a, b) = (
                (start - 1) as usize,
                (end - 1).min(s.len() as i64).max(0) as usize,
            );
            Value::Text(if a >= s.len() || a >= b {
                String::new()
            } else {
                s[a..b].iter().collect()
            })
        }
        Func::Round => {
            let digits = args.get(1).map(int).transpose()?.unwrap_or(0);
            match &args[0] {
                Value::Int(i) if digits >= 0 => Value::Int(*i),
                Value::Numeric(d) => Value::Numeric(d.round(digits.clamp(-100, 100) as i32)?),
                v => {
                    let x = as_f64(v);
                    let m = 10f64.powi(digits as i32);
                    let r = (x * m).round() / m;
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
        Func::Strpos => {
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
                [b] => (dt::date_to_ts(dt::ts_to_date(dt::now())), ts(b)?),
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
        Func::NumericFit => {
            let Value::Numeric(d) = &args[0] else {
                return Ok(args[0].clone());
            };
            let p = u32::try_from(int(&args[1])?).unwrap_or(0);
            let s = u32::try_from(int(&args[2])?).unwrap_or(0);
            Value::Numeric(d.fit(p, s)?)
        }
        Func::Now
        | Func::ClockTimestamp
        | Func::CurrentDate
        | Func::LocalTimestamp
        | Func::CurrentUser
        | Func::CurrentDatabase
        | Func::CurrentSchema
        | Func::Coalesce
        | Func::Concat
        | Func::IsDistinctFrom
        | Func::Nullif
        | Func::Greatest
        | Func::Least
        | Func::Version => {
            unreachable!("handled above")
        }
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
