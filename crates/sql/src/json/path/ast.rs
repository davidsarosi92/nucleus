//! A path, parsed: an expression tree, accessors in chains.

use crate::decimal::Decimal;

/// A path: its mode and its expression (or predicate).
#[derive(Clone, Debug)]
pub struct Path {
    /// `strict` (structural errors are errors) or `lax` (the default:
    /// they are ignored, arrays are unwrapped and scalars wrapped).
    pub strict: bool,
    pub expr: Expr,
}

/// Arithmetic operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// Comparison operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// An expression or a predicate.
#[derive(Clone, Debug)]
pub enum Expr {
    /// `$`
    Root,
    /// `@`
    Current,
    /// `last`, in an array subscript.
    Last,
    /// `$name`
    Var(String),
    Null,
    Bool(bool),
    /// A number, and its json form.
    Num(Decimal, Box<[u8]>),
    /// A string, and its json form.
    Str(String, Box<[u8]>),
    /// A primary (never itself a chain) and its accessors.
    Chain(Box<Expr>, Vec<Step>),
    /// Unary `-` (true) or `+`.
    Neg(bool, Box<Expr>),
    Arith(Arith, Box<Expr>, Box<Expr>),
    // Predicates.
    Cmp(Cmp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsUnknown(Box<Expr>),
    Exists(Box<Expr>),
    /// `whole starts with initial` (a string or a variable).
    StartsWith(Box<Expr>, Box<Expr>),
    LikeRegex(Box<Expr>, Box<Regex>),
}

impl Expr {
    /// How deeply the tree nests (a leaf is 1).
    pub(crate) fn depth(&self) -> usize {
        1 + match self {
            Expr::Chain(head, steps) => steps
                .iter()
                .map(|s| match s {
                    Step::Index(subs) => subs
                        .iter()
                        .map(|(a, b)| a.depth().max(b.as_ref().map_or(0, Expr::depth)))
                        .max()
                        .unwrap_or(0),
                    Step::Filter(e) => e.depth(),
                    _ => 0,
                })
                .fold(head.depth(), usize::max),
            Expr::Neg(_, e)
            | Expr::Not(e)
            | Expr::IsUnknown(e)
            | Expr::Exists(e)
            | Expr::LikeRegex(e, _) => e.depth(),
            Expr::Arith(_, a, b)
            | Expr::Cmp(_, a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::StartsWith(a, b) => a.depth().max(b.depth()),
            _ => 0,
        }
    }

    /// Whether this is a predicate (its value true, false or unknown).
    pub fn is_predicate(&self) -> bool {
        matches!(
            self,
            Expr::Cmp(..)
                | Expr::And(..)
                | Expr::Or(..)
                | Expr::Not(_)
                | Expr::IsUnknown(_)
                | Expr::Exists(_)
                | Expr::StartsWith(..)
                | Expr::LikeRegex(..)
        )
    }
}

/// `like_regex`'s pattern, flags and compiled form.
#[derive(Clone, Debug)]
pub struct Regex {
    pub pattern: String,
    /// `i`, `s`, `m`, `q`: [`FLAG_I`] and the others.
    pub flags: u8,
    pub re: regex::Regex,
}

pub const FLAG_I: u8 = 1;
pub const FLAG_S: u8 = 2;
pub const FLAG_M: u8 = 4;
pub const FLAG_Q: u8 = 8;

/// An accessor.
#[derive(Clone, Debug)]
pub enum Step {
    /// `.key`
    Key(String),
    /// `.*`
    AnyKey,
    /// `[*]`
    AnyArray,
    /// `[i, j to k]`
    Index(Vec<(Expr, Option<Expr>)>),
    /// `.**{first to last}`, [`LAST`] for `last`.
    Any(u32, u32),
    /// `?(predicate)`
    Filter(Box<Expr>),
    Method(Method),
}

/// `last` as a `.**` level.
pub const LAST: u32 = u32::MAX;

/// Item methods.
#[derive(Clone, Debug)]
pub enum Method {
    Type,
    Size,
    Double,
    Ceiling,
    Floor,
    Abs,
    KeyValue,
    BigInt,
    Boolean,
    Integer,
    Number,
    String,
    /// `.decimal(precision, scale)`
    Decimal(Option<i32>, Option<i32>),
    /// `.datetime(template)`
    Datetime(Option<String>),
    Date,
    /// `.time(precision)`
    Time(Option<i32>),
    TimeTz(Option<i32>),
    Timestamp(Option<i32>),
    TimestampTz(Option<i32>),
}

impl Method {
    /// Its name, as error messages say it.
    pub fn name(&self) -> &'static str {
        match self {
            Method::Type => "type",
            Method::Size => "size",
            Method::Double => "double",
            Method::Ceiling => "ceiling",
            Method::Floor => "floor",
            Method::Abs => "abs",
            Method::KeyValue => "keyvalue",
            Method::BigInt => "bigint",
            Method::Boolean => "boolean",
            Method::Integer => "integer",
            Method::Number => "number",
            Method::String => "string",
            Method::Decimal(..) => "decimal",
            Method::Datetime(_) => "datetime",
            Method::Date => "date",
            Method::Time(_) => "time",
            Method::TimeTz(_) => "time_tz",
            Method::Timestamp(_) => "timestamp",
            Method::TimestampTz(_) => "timestamp_tz",
        }
    }
}
