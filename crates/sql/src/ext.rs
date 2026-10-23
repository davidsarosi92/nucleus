//! The extension API (PLAN.md principle 12, §M14.19): what a module adds to
//! the SQL layer, and the [`Registry`] it adds it to.
//!
//! A module is a [`Module`]: a name and a `register` that adds functions,
//! schemas and catalog views.  The built-in ones register the same way
//! ([`Registry::builtin`]); a session runs against one registry
//! ([`crate::Session::with_registry`]), so different front ends can offer
//! different modules — the PostgreSQL front end adds `pg-compat`, `ncp`
//! does not.
//!
//! ```
//! use std::sync::Arc;
//! use nucleus_sql::ext::*;
//! use nucleus_sql::{DataType, Value};
//!
//! struct Double;
//! impl ScalarFunction for Double {
//!     fn name(&self) -> &str { "double" }
//!     fn arity(&self) -> std::ops::RangeInclusive<usize> { 1..=1 }
//!     fn resolve(&self, _: &[ArgInfo]) -> nucleus_sql::Result<Signature> {
//!         Ok(Signature::new(vec![Arg::Untyped(DataType::BigInt)], Returns::Type(Some(DataType::BigInt))))
//!     }
//!     fn call(&self, _: &CallContext, args: &[Value]) -> nucleus_sql::Result<Value> {
//!         match args[0] { Value::Int(i) => Ok(Value::Int(i * 2)), _ => Ok(Value::Null) }
//!     }
//! }
//!
//! struct Sample;
//! impl Module for Sample {
//!     fn name(&self) -> &str { "sample" }
//!     fn register(&self, r: &mut Registry) -> nucleus_sql::Result<()> {
//!         r.add_function(&["double"], Arc::new(Double))
//!     }
//! }
//!
//! let mut registry = Registry::builtin();
//! registry.add(&Sample).unwrap();
//! ```
//!
//! The interfaces are versioned ([`API_VERSION`], principle 6); a change
//! that breaks a module raises it.
//!
//! **This module is the SQL layer's whole public API for modules**: a
//! module (`modules/`) uses `nucleus_sql::ext` and nothing else of this
//! crate — CI checks it (PLAN.md §M14.19) — so the rest can change freely.
//! Everything a module needs is re-exported here: sessions and statements,
//! values, the read-only catalog.

use std::collections::HashMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use nucleus_kv::KvTxn;

pub use crate::ast::Statement;
pub use crate::datetime::{
    Interval, parse_date, parse_interval, parse_time_of_day, parse_timestamp,
};
pub use crate::decimal::Decimal;
pub use crate::error::{Error, Result};
pub use crate::inet::Net;
pub use crate::parser::{parse, parse_with_text};
pub use crate::session::{OutColumn, Output, Session, TxnStatus, command_tag};
pub use crate::types::{DataType, Row, Value};

/// The catalog, read-only, as modules see it: table definitions and their
/// constraints.
pub mod catalog {
    pub use crate::ast::FkAction;
    pub use crate::catalog::{
        Column, Constraint, ConstraintKind, ForeignKey, Index, Table, constraints, tables,
    };
    pub use crate::fk::referenced_key;
    pub use crate::roles::{Membership, Role, all as roles, memberships};
}

/// Who the current statement runs as ([`crate::access`]).
pub use crate::access::{Context, Who, current as current_who};
/// Records a login refused before a session could be had ([`crate::audit`]).
pub use crate::session::audit_refused_login;

/// The version of this interface.
pub const API_VERSION: u32 = 1;

/// A module: a set of additions to the registry.
pub trait Module: Send + Sync {
    /// The module's name, unique in a registry (`builtin`, `pg-compat`).
    fn name(&self) -> &str;

    /// Adds the module's functions, schemas and views.
    fn register(&self, registry: &mut Registry) -> Result<()>;
}

/// What the binder knows of an argument before the call is typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgInfo {
    /// Its type (`None`: unknown — `NULL` or a parameter not yet typed).
    pub ty: Option<DataType>,
    /// A string literal, whose type can still be chosen (as PostgreSQL's
    /// `unknown`).
    pub text_literal: bool,
}

/// How the binder treats an argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    /// As it is.
    Keep,
    /// An untyped argument (a string literal, `NULL`, a parameter) takes
    /// this type; a typed one stays as it is.
    Untyped(DataType),
    /// Converted to this type (after [`Arg::Untyped`]'s typing).
    Convert(DataType),
    /// All `Common` arguments are converted to their common type.
    Common,
}

/// The result type of a call.
#[derive(Clone, Debug, PartialEq)]
pub enum Returns {
    /// This type (`None`: unknown).
    Type(Option<DataType>),
    /// The common type of the [`Arg::Common`] arguments.
    Common,
    /// The call is this constant (its value depends only on the argument
    /// types, as `pg_typeof`'s); the arguments are not evaluated.
    Constant(Value),
}

/// How a call is typed: per argument, and the result.
#[derive(Clone, Debug, PartialEq)]
pub struct Signature {
    /// One per argument.
    pub args: Vec<Arg>,
    /// The result.
    pub returns: Returns,
}

impl Signature {
    /// A signature.
    pub fn new(args: Vec<Arg>, returns: Returns) -> Self {
        Self { args, returns }
    }

    /// Every argument kept, returning `ty`.
    pub fn keep(n: usize, ty: Option<DataType>) -> Self {
        Self::new(vec![Arg::Keep; n], Returns::Type(ty))
    }
}

/// What a function or view sees when it runs.
pub struct CallContext<'a> {
    txn: &'a dyn KvTxn,
    registry: &'a Registry,
}

impl<'a> CallContext<'a> {
    /// A context.
    pub fn new(txn: &'a dyn KvTxn, registry: &'a Registry) -> Self {
        Self { txn, registry }
    }

    /// The statement's transaction (catalog reads go through it).
    pub fn txn(&self) -> &'a dyn KvTxn {
        self.txn
    }

    /// The session's registry.
    pub fn registry(&self) -> &'a Registry {
        self.registry
    }

    /// Fills `buf` with unpredictable bytes, from the database's platform
    /// (never the host directly: a simulation stays reproducible).
    pub fn random(&self, buf: &mut [u8]) -> Result<()> {
        crate::datetime::random_bytes(buf)
    }

    /// The transaction's start (`now()`), as µs since 2000-01-01 UTC.
    pub fn now(&self) -> Result<i64> {
        crate::datetime::now()
    }

    /// The time now (`clock_timestamp()`), as µs since 2000-01-01 UTC.
    pub fn clock(&self) -> Result<i64> {
        crate::datetime::clock()
    }
}

/// Whether a function's result depends only on its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Volatility {
    /// The same arguments give the same result, always: a mapping that
    /// defines an ordering or an index may use it.
    Immutable,
    /// The same within a statement (`now()`, catalog lookups).
    Stable,
    /// Anything (`gen_random_uuid()`, `clock_timestamp()`).
    Volatile,
}

/// A scalar function.
pub trait ScalarFunction: Send + Sync {
    /// The name `EXPLAIN` shows.
    fn name(&self) -> &str;

    /// The argument counts accepted.
    fn arity(&self) -> RangeInclusive<usize>;

    /// Types a call, from what is known of its arguments; an error refuses
    /// the call at bind time.
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature>;

    /// Whether a `NULL` argument makes the result `NULL` without a call
    /// (the default); `false` if the function handles `NULL`s itself.
    fn strict(&self) -> bool {
        true
    }

    /// How its result may vary (the default, `Volatile`, assumes the
    /// worst).
    fn volatility(&self) -> Volatility {
        Volatility::Volatile
    }

    /// For a function of two arguments that can be true only when its
    /// first lies in a range computed from its second (`inet <<= cidr`):
    /// two functions of the second giving that range's bounds, inclusive,
    /// in the first's type and order.  The binder adds `first >= low(second)
    /// AND first <= high(second)` beside the call, which an index on the
    /// first can scan.  The default: none.
    fn implied_range(&self) -> Option<(Arc<dyn ScalarFunction>, Arc<dyn ScalarFunction>)> {
        None
    }

    /// Computes the value.
    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value>;
}

/// A registered function, as a bound expression holds it.  Two are equal
/// when their names are (a registry has one function per name).
#[derive(Clone)]
pub struct FnRef(Arc<dyn ScalarFunction>);

impl FnRef {
    /// A reference to `f`.
    pub fn new(f: Arc<dyn ScalarFunction>) -> Self {
        Self(f)
    }
}

impl std::ops::Deref for FnRef {
    type Target = dyn ScalarFunction;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for FnRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.name() == other.0.name()
    }
}

impl fmt::Debug for FnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

/// The common type of arguments, as the binder unifies [`Arg::Common`]
/// ones: string literals take the others' type, or `text` if all are.
pub fn common_type(args: &[ArgInfo]) -> Result<Option<DataType>> {
    let mut ty = None;
    for a in args.iter().filter(|a| !a.text_literal) {
        ty = crate::binder::unify(ty, a.ty)?;
    }
    if ty.is_none() && args.iter().any(|a| a.text_literal) {
        ty = Some(DataType::Text);
    }
    Ok(ty)
}

/// An aggregate function.  The engine groups rows, skips rows whose first
/// argument is `NULL` (unless [`Aggregate::keeps_nulls`]), applies
/// `DISTINCT` and `ORDER BY` inside the call, and feeds each group's rows
/// to a fresh [`Accumulator`].
pub trait Aggregate: Send + Sync {
    /// Its name.
    fn name(&self) -> &str;

    /// The argument counts accepted.
    fn arity(&self) -> RangeInclusive<usize>;

    /// Whether `name(*)` is accepted (fed with no arguments).
    fn accepts_star(&self) -> bool {
        false
    }

    /// Whether rows whose first argument is `NULL` are fed too.
    fn keeps_nulls(&self) -> bool {
        false
    }

    /// Types a call (as [`ScalarFunction::resolve`]; [`Returns::Constant`]
    /// is not allowed).
    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature>;

    /// A fresh state, for one group.
    fn start(&self) -> Box<dyn Accumulator>;
}

/// One group's state of an aggregate.
pub trait Accumulator {
    /// Takes one row's arguments.
    fn add(&mut self, args: &[Value]) -> Result<()>;

    /// The result, after the last row (or none).
    fn finish(&mut self) -> Result<Value>;
}

/// A registered aggregate, as a plan holds it.  Equal when names are.
#[derive(Clone)]
pub struct AggRef(Arc<dyn Aggregate>);

impl AggRef {
    /// A reference to `a`.
    pub fn new(a: Arc<dyn Aggregate>) -> Self {
        Self(a)
    }
}

impl std::ops::Deref for AggRef {
    type Target = dyn Aggregate;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for AggRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.name() == other.0.name()
    }
}

impl fmt::Debug for AggRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

/// How a table function call is typed: per argument, and its columns (a
/// single column is named after the function, or its alias).
#[derive(Clone, Debug, PartialEq)]
pub struct TableSignature {
    /// One per argument.
    pub args: Vec<Arg>,
    /// The output columns: name and type.
    pub columns: Vec<(String, Option<DataType>)>,
}

/// A function in `FROM`: `generate_series(1, 10)`, `unnest(array)`.
pub trait TableFunction: Send + Sync {
    /// Its name.
    fn name(&self) -> &str;

    /// The argument counts accepted.
    fn arity(&self) -> RangeInclusive<usize>;

    /// Types a call.
    fn resolve(&self, args: &[ArgInfo]) -> Result<TableSignature>;

    /// Its rows, for these argument values.
    fn rows(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Vec<Row>>;
}

/// A registered table function, as a plan holds it.  Equal when names are.
#[derive(Clone)]
pub struct TableFnRef(Arc<dyn TableFunction>);

impl std::ops::Deref for TableFnRef {
    type Target = dyn TableFunction;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for TableFnRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.name() == other.0.name()
    }
}

impl fmt::Debug for TableFnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

/// A read-only table computed when it is scanned: `nc_catalog.tables`,
/// `pg_catalog.pg_class`, …
pub trait CatalogView: Send + Sync {
    /// Its schema (registered with [`Registry::add_schema`]).
    fn schema(&self) -> &str;

    /// Its name.
    fn name(&self) -> &str;

    /// Its columns.
    fn columns(&self) -> Vec<(String, DataType)>;

    /// Its rows, now.
    fn rows(&self, cx: &CallContext<'_>) -> Result<Vec<Row>>;
}

/// A registered view, as a plan holds it.  Equal when schema and name are.
#[derive(Clone)]
pub struct ViewRef(Arc<dyn CatalogView>);

impl std::ops::Deref for ViewRef {
    type Target = dyn CatalogView;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for ViewRef {
    fn eq(&self, other: &Self) -> bool {
        (self.0.schema(), self.0.name()) == (other.0.schema(), other.0.name())
    }
}

impl fmt::Debug for ViewRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0.schema(), self.0.name())
    }
}

/// A schema: a namespace of catalog views.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    /// Its name.
    pub name: String,
    /// What it holds.
    pub description: String,
    /// Whether unqualified names find its views (after the user's tables),
    /// as PostgreSQL's `pg_catalog`.
    pub searched: bool,
}

/// Everything a session can use: functions, schemas and catalog views, by
/// name, from the modules added.
#[derive(Clone, Default)]
pub struct Registry {
    modules: Vec<String>,
    functions: HashMap<String, FnRef>,
    aggregates: HashMap<String, AggRef>,
    table_functions: HashMap<String, TableFnRef>,
    schemas: Vec<Schema>,
    views: Vec<ViewRef>,
    /// Types by name and alias.
    types: HashMap<String, DataType>,
    /// Every type, in registration order, with the module that registered
    /// it.
    type_list: Vec<(DataType, String)>,
    /// Operators outside the grammar's own, by symbol and operand types.
    operators: HashMap<(String, DataType, DataType), FnRef>,
    /// The module whose `register` is running.
    registering: String,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("modules", &self.modules)
            .finish_non_exhaustive()
    }
}

fn duplicate(what: &str, name: &str) -> Error {
    Error::duplicate_object(format!("{what} \"{name}\" is already registered"))
}

impl Registry {
    /// A registry with nothing in it — not even the built-ins.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The built-in module alone: every built-in function, `public` and
    /// `nc_catalog`.
    pub fn builtin() -> Self {
        let mut r = Self::empty();
        r.add(&crate::builtin::Builtin)
            .expect("the built-in module registers");
        r
    }

    /// [`Registry::builtin`], built once and shared.
    pub fn shared_builtin() -> Arc<Registry> {
        static R: OnceLock<Arc<Registry>> = OnceLock::new();
        R.get_or_init(|| Arc::new(Registry::builtin())).clone()
    }

    /// Adds a module.  Fails if one of that name, or anything it registers,
    /// is already there; the registry is then unchanged.
    pub fn add(&mut self, module: &dyn Module) -> Result<()> {
        if self.modules.iter().any(|m| m == module.name()) {
            return Err(duplicate("module", module.name()));
        }
        let mut next = self.clone();
        next.registering = module.name().to_string();
        module.register(&mut next)?;
        next.registering.clear();
        next.modules.push(module.name().to_string());
        *self = next;
        Ok(())
    }

    /// The modules added, in order.
    pub fn modules(&self) -> &[String] {
        &self.modules
    }

    /// Whether a scalar function or aggregate is called `name` (they share
    /// one namespace: a call does not say which it is).
    fn callable(&self, name: &str) -> bool {
        self.functions.contains_key(name) || self.aggregates.contains_key(name)
    }

    /// Registers `f` under each of `names`.
    pub fn add_function(&mut self, names: &[&str], f: Arc<dyn ScalarFunction>) -> Result<()> {
        if let Some(n) = names.iter().find(|n| self.callable(n)) {
            return Err(duplicate("function", n));
        }
        let f = FnRef(f);
        for n in names {
            self.functions.insert((*n).to_string(), f.clone());
        }
        Ok(())
    }

    /// The function called `name`.
    pub fn function(&self, name: &str) -> Option<&FnRef> {
        self.functions.get(name)
    }

    /// Registers aggregate `a` under each of `names`.
    pub fn add_aggregate(&mut self, names: &[&str], a: Arc<dyn Aggregate>) -> Result<()> {
        if let Some(n) = names.iter().find(|n| self.callable(n)) {
            return Err(duplicate("function", n));
        }
        let a = AggRef(a);
        for n in names {
            self.aggregates.insert((*n).to_string(), a.clone());
        }
        Ok(())
    }

    /// The aggregate called `name`.
    pub fn aggregate(&self, name: &str) -> Option<&AggRef> {
        self.aggregates.get(name)
    }

    /// Registers table function `f`.
    pub fn add_table_function(&mut self, f: Arc<dyn TableFunction>) -> Result<()> {
        if self.table_functions.contains_key(f.name()) {
            return Err(duplicate("table function", f.name()));
        }
        self.table_functions
            .insert(f.name().to_string(), TableFnRef(f));
        Ok(())
    }

    /// The table function called `name`.
    pub fn table_function(&self, name: &str) -> Option<&TableFnRef> {
        self.table_functions.get(name)
    }

    /// Every function name, sorted.
    pub fn function_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.functions.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// Every aggregate name, sorted.
    pub fn aggregate_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.aggregates.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// Registers a schema.
    pub fn add_schema(&mut self, name: &str, description: &str, searched: bool) -> Result<()> {
        if self.schema(name).is_some() {
            return Err(duplicate("schema", name));
        }
        self.schemas.push(Schema {
            name: name.into(),
            description: description.into(),
            searched,
        });
        Ok(())
    }

    /// The schema `name`.
    pub fn schema(&self, name: &str) -> Option<&Schema> {
        self.schemas.iter().find(|s| s.name == name)
    }

    /// Every schema, in registration order.
    pub fn schemas(&self) -> &[Schema] {
        &self.schemas
    }

    /// Registers a view; its schema must be registered.
    pub fn add_view(&mut self, v: Arc<dyn CatalogView>) -> Result<()> {
        if self.schema(v.schema()).is_none() {
            return Err(Error::invalid_schema(format!(
                "schema \"{}\" is not registered",
                v.schema()
            )));
        }
        if self.view(Some(v.schema()), v.name()).is_some() {
            return Err(duplicate("view", &format!("{}.{}", v.schema(), v.name())));
        }
        self.views.push(ViewRef(v));
        Ok(())
    }

    /// The view `schema.name`; without a schema, the first one in a
    /// searched schema.
    pub fn view(&self, schema: Option<&str>, name: &str) -> Option<&ViewRef> {
        match schema {
            Some(s) => self
                .views
                .iter()
                .find(|v| v.schema() == s && v.name() == name),
            None => self.schemas.iter().filter(|s| s.searched).find_map(|s| {
                self.views
                    .iter()
                    .find(|v| v.schema() == s.name && v.name() == name)
            }),
        }
    }

    /// Every view, in registration order.
    pub fn views(&self) -> &[ViewRef] {
        &self.views
    }

    /// Registers a type under its name and aliases.
    pub fn add_type(&mut self, t: Arc<dyn Type>) -> Result<()> {
        let names: Vec<String> = std::iter::once(t.name())
            .chain(t.aliases().iter().copied())
            .map(str::to_string)
            .collect();
        if let Some(n) = names.iter().find(|n| self.types.contains_key(*n)) {
            return Err(duplicate("type", n));
        }
        let r = intern(t)?;
        lawful(r.imp())?;
        self.add_type_as(&names, DataType::Ext(r));
        Ok(())
    }

    /// Registers a built-in type (its values are native variants).
    pub(crate) fn add_builtin_type(&mut self, names: &[&str], ty: DataType) {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        self.add_type_as(&names, ty);
    }

    fn add_type_as(&mut self, names: &[String], ty: DataType) {
        for n in names {
            self.types.insert(n.clone(), ty);
        }
        self.type_list.push((ty, self.registering.clone()));
    }

    /// Registers operator `symbol` for operands of types `left` and
    /// `right`, computed by `f` (called with the two operands).  The
    /// grammar's own operators (`+`, `=`, `||`, …) are not taken.
    pub fn add_operator(
        &mut self,
        symbol: &str,
        left: DataType,
        right: DataType,
        f: Arc<dyn ScalarFunction>,
    ) -> Result<()> {
        if !is_user_operator(symbol) {
            return Err(Error::invalid_parameter(format!(
                "{symbol} is not an operator a module can bring (the grammar has its own)"
            )));
        }
        let key = (symbol.to_string(), left, right);
        if self.operators.contains_key(&key) {
            return Err(duplicate("operator", symbol));
        }
        self.operators.insert(key, FnRef(f));
        Ok(())
    }

    /// The operator `symbol` for these operand types.
    pub fn operator(&self, symbol: &str, left: DataType, right: DataType) -> Option<&FnRef> {
        self.operators.get(&(symbol.to_string(), left, right))
    }

    /// Every registered operator: symbol, operand types, function.
    pub fn operators(&self) -> Vec<(&str, DataType, DataType, &FnRef)> {
        let mut v: Vec<_> = self
            .operators
            .iter()
            .map(|((s, l, r), f)| (s.as_str(), *l, *r, f))
            .collect();
        v.sort_by(|a, b| (a.0, a.1.name(), a.2.name()).cmp(&(b.0, b.1.name(), b.2.name())));
        v
    }

    /// Makes `name` another name for type `ty` (pg-compat's `int4`, `oid`).
    pub fn add_alias(&mut self, name: &str, ty: DataType) -> Result<()> {
        if self.types.contains_key(name) {
            return Err(duplicate("type", name));
        }
        self.types.insert(name.to_string(), ty);
        Ok(())
    }

    /// The type a name (or alias) means.
    pub fn type_named(&self, name: &str) -> Option<DataType> {
        self.types.get(name).copied()
    }

    /// Every type, in registration order, with the module that registered
    /// it.
    pub fn types(&self) -> &[(DataType, String)] {
        &self.type_list
    }

    /// Every name the registry knows `ty` by, sorted.
    pub fn type_aliases(&self, ty: DataType) -> Vec<String> {
        let mut v: Vec<String> = self
            .types
            .iter()
            .filter(|(_, t)| **t == ty)
            .map(|(n, _)| n.clone())
            .collect();
        v.sort();
        v
    }
}

// ------------------------------------------------------------------ types

/// A type: its names, its text form and its abilities (PLAN.md §M14.19,
/// "Type capabilities").  Every type is one — the built-in ones too, whose
/// values are native [`Value`] variants; a registered type's values are
/// [`Value::Ext`]: the type and the value's **canonical** bytes.
///
/// Each ability is its own interface, and a type has it or not; the
/// engine asks before it relies on one, and refuses at bind or DDL time
/// — naming the ability — what a type cannot do.
pub trait Type: std::any::Any + Send + Sync {
    /// Its name, unique (`uuid`): what SQL and the catalog call it.
    fn name(&self) -> &str;

    /// Other names SQL accepts for it.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Its category, for `nc_catalog.types` (`number`, `text`, …).
    fn category(&self) -> &str {
        "other"
    }

    /// A value from its text form (literals, casts from `text`, the text
    /// wire format).  A registered type returns [`Value::ext`].
    fn input(&self, text: &str) -> Result<Value>;

    /// A value's text form.  `input(output(v))` is `v` (a law).
    fn output(&self, v: &Value) -> String;

    /// `=`, `<>`, `IN`, `DISTINCT`, `GROUP BY`, set operations, `UNIQUE`.
    fn equality(&self) -> Option<&dyn Equality> {
        None
    }

    /// Hashing consistent with equality: hash joins and hash aggregation.
    fn hashing(&self) -> Option<&dyn Hashing> {
        None
    }

    /// A total order: `<` …, `ORDER BY`, `min` / `max`, range predicates.
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        None
    }

    /// An order-preserving key encoding: primary keys, indexes, index
    /// range scans.
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        None
    }

    /// A distance (`ORDER BY a <-> b`).
    fn distance(&self) -> Option<&dyn Distance> {
        None
    }

    /// The version of its stored form (rows record it per value).
    fn encoding_version(&self) -> u16 {
        1
    }

    /// A value's canonical bytes now, from the bytes an older encoding
    /// version stored: values are upgraded as they are read, and written
    /// in the current version (`ALTER TYPE t UPGRADE` rewrites them all).
    fn upgrade(&self, from: u16, bytes: &[u8]) -> Result<Vec<u8>> {
        let _ = bytes;
        Err(Error::feature(format!(
            "type {} cannot read its encoding version {from}",
            self.name()
        )))
    }

    /// Whether values upgraded from version `from` have other keys than
    /// before (their key encoding changed too): then the keys holding them
    /// are rewritten before anything uses them — by the first transaction
    /// that may write.
    fn upgrade_changes_keys(&self, from: u16) -> bool {
        let _ = from;
        false
    }

    /// Some of its values — edge cases first — on which registration
    /// checks the laws ([`crate::laws`]): a type that breaks one is not
    /// registered.  (The test kit checks many more, generated.)
    fn examples(&self) -> Vec<Value> {
        Vec::new()
    }
}

/// Equality.  A registered type's is on canonical bytes
/// ([`CanonicalBytes`]), which is what makes hashing agree with it.
pub trait Equality: Send + Sync {
    /// Whether `a` and `b` (non-`NULL`, of the type) are equal.
    fn equal(&self, a: &Value, b: &Value) -> Result<bool>;
}

/// Hashing, consistent with [`Equality`].
pub trait Hashing: Send + Sync {
    /// Feeds `v` to `state`.
    fn hash(&self, v: &Value, state: &mut dyn std::hash::Hasher);
}

/// A total order, consistent with [`Equality`].
pub trait TotalOrder: Send + Sync {
    /// Compares two non-`NULL` values of the type.
    fn compare(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering>;
}

/// An order-preserving, self-delimiting key encoding: bytewise order of the
/// encodings is the type's [`TotalOrder`].
pub trait KeyEncoding: Send + Sync {
    /// Appends `v`'s encoding to `out`.
    fn encode(&self, v: &Value, out: &mut Vec<u8>);

    /// The value at the start of `b` and the bytes it took.
    fn decode(&self, b: &[u8]) -> Result<(Value, usize)>;
}

/// A distance.
pub trait Distance: Send + Sync {
    /// The operator SQL writes it with (`<->`).
    fn operator(&self) -> &str;

    /// The distance between two non-`NULL` values of the type.
    fn distance(&self, a: &Value, b: &Value) -> Result<f64>;
}

/// Equality and hashing on a registered type's canonical bytes — the only
/// equality such a type may have, since [`Value`]'s own comparison of
/// [`Value::Ext`] is on its bytes too.
pub struct CanonicalBytes;

impl Equality for CanonicalBytes {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool> {
        Ok(a.ext_bytes() == b.ext_bytes())
    }
}

impl Hashing for CanonicalBytes {
    fn hash(&self, v: &Value, state: &mut dyn std::hash::Hasher) {
        state.write(v.ext_bytes().unwrap_or_default());
    }
}

/// A registered type, as a [`DataType::Ext`] or [`Value::Ext`] holds it:
/// interned once per name, so it is small and `Copy`.  Equal when the
/// names are.
#[derive(Clone, Copy)]
pub struct TypeRef(&'static TypeEntry);

/// An interned type.
pub struct TypeEntry {
    name: &'static str,
    id: u32,
    imp: Arc<dyn Type>,
}

/// A registered type's stable id: the FNV-1a hash of its name — the same
/// in every process and database, so rows and keys can carry it (two names
/// with one id are refused at registration).
pub fn type_id_of(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

impl TypeRef {
    /// The type's name.
    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// Its implementation.
    pub fn imp(self) -> &'static dyn Type {
        &*self.0.imp
    }

    /// Its stable id ([`type_id_of`] its name).
    pub fn id(self) -> u32 {
        self.0.id
    }

    /// The interned type called `name`, if a registry has registered one
    /// in this process.
    pub fn named(name: &str) -> Option<TypeRef> {
        let t = interned().lock().unwrap_or_else(|e| e.into_inner());
        t.by_name.get(name).copied()
    }

    /// The interned `name`, or a stand-in for a type this process has no
    /// implementation of (a database naming a module's type that is not
    /// linked in): it can be named, and every use of a value fails.
    pub fn named_or_missing(name: &str) -> TypeRef {
        Self::named(name).unwrap_or_else(|| {
            intern(Arc::new(Missing(name.to_string()))).expect("not yet interned")
        })
    }

    /// The interned type with stable id `id`, or a stand-in (named by the
    /// id) when this process has none.
    pub fn by_id(id: u32) -> TypeRef {
        let found = {
            let t = interned().lock().unwrap_or_else(|e| e.into_inner());
            t.by_id.get(&id).copied()
        };
        found.unwrap_or_else(|| Self::named_or_missing(&format!("type #{id:08x}")))
    }
}

impl PartialEq for TypeRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.name == other.0.name
    }
}

impl Eq for TypeRef {}

impl std::hash::Hash for TypeRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.name.hash(state);
    }
}

impl fmt::Debug for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name)
    }
}

/// Whether `symbol` is an operator outside the grammar's own: one the
/// lexer reads as a [`crate::lexer::Token::UserOp`].
pub fn is_user_operator(symbol: &str) -> bool {
    matches!(
        crate::lexer::tokenize(symbol).as_deref(),
        Ok([(crate::lexer::Token::UserOp(s), 0)]) if s == symbol
    )
}

/// A type's [`Distance`], as the function its operator calls.
pub struct DistanceFn {
    ty: DataType,
    name: String,
}

impl DistanceFn {
    /// The distance of `ty` (which must have one).
    pub fn of(ty: DataType) -> FnRef {
        let t = crate::builtin::type_of(ty);
        let op = t.distance().map_or("", |d| d.operator()).to_string();
        FnRef::new(Arc::new(DistanceFn {
            ty,
            name: format!("{op}({})", t.name()),
        }))
    }
}

impl ScalarFunction for DistanceFn {
    fn name(&self) -> &str {
        &self.name
    }
    fn arity(&self) -> RangeInclusive<usize> {
        2..=2
    }
    fn resolve(&self, _: &[ArgInfo]) -> Result<Signature> {
        Ok(Signature::new(
            vec![Arg::Untyped(self.ty); 2],
            Returns::Type(Some(DataType::Float)),
        ))
    }
    fn volatility(&self) -> Volatility {
        Volatility::Immutable
    }
    fn call(&self, _: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        let d = crate::builtin::type_of(self.ty)
            .distance()
            .ok_or_else(|| Error::internal("a distance without one"))?;
        Ok(Value::Float(d.distance(&args[0], &args[1])?))
    }
}

/// Refuses a type whose examples break a law.
pub(crate) fn lawful(t: &dyn Type) -> Result<()> {
    let problems = crate::laws::check_values(t, &t.examples());
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::invalid_parameter(format!(
        "type {} breaks the laws its abilities promise: {}",
        t.name(),
        problems.join("; ")
    )))
}

/// Every interned type, by name and by id.
#[derive(Default)]
struct Interned {
    by_name: HashMap<String, TypeRef>,
    by_id: HashMap<u32, TypeRef>,
}

fn interned() -> &'static std::sync::Mutex<Interned> {
    static T: OnceLock<std::sync::Mutex<Interned>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Interns `t`: the same implementation under a name returns the same
/// [`TypeRef`]; another one under a taken name is refused.  A stand-in for
/// a missing type gives way to the real one.
fn intern(t: Arc<dyn Type>) -> Result<TypeRef> {
    let mut map = interned().lock().unwrap_or_else(|e| e.into_inner());
    let rust_type = |t: &dyn Type| (t as &dyn std::any::Any).type_id();
    let missing = std::any::TypeId::of::<Missing>();
    if let Some(r) = map.by_name.get(t.name()) {
        let existing = r.imp();
        if rust_type(existing) == rust_type(&*t) {
            return Ok(*r);
        }
        if rust_type(existing) != missing {
            return Err(duplicate("type", t.name()));
        }
    }
    let id = type_id_of(t.name());
    if let Some(other) = map.by_id.get(&id)
        && other.name() != t.name()
        && rust_type(other.imp()) != missing
    {
        return Err(Error::duplicate_object(format!(
            "type {} has the id of type {} (rename one)",
            t.name(),
            other.name()
        )));
    }
    let entry: &'static TypeEntry = Box::leak(Box::new(TypeEntry {
        name: Box::leak(t.name().to_string().into_boxed_str()),
        id,
        imp: t,
    }));
    let r = TypeRef(entry);
    map.by_name.insert(entry.name.to_string(), r);
    if rust_type(r.imp()) != missing || !map.by_id.contains_key(&id) {
        map.by_id.insert(id, r);
    }
    Ok(r)
}

/// Whether `t` is the stand-in for a type this process has no
/// implementation of.
pub fn is_missing(t: TypeRef) -> bool {
    (t.imp() as &dyn std::any::Any).type_id() == std::any::TypeId::of::<Missing>()
}

/// The stand-in for a type a database names but this process lacks.
struct Missing(String);

impl Missing {
    fn error(&self) -> Error {
        Error::undefined_object(format!(
            "type {} is not available: no module registered it in this build",
            self.0
        ))
    }
}

impl Type for Missing {
    fn name(&self) -> &str {
        &self.0
    }
    fn input(&self, _: &str) -> Result<Value> {
        Err(self.error())
    }
    fn output(&self, v: &Value) -> String {
        let hex: String = v
            .ext_bytes()
            .unwrap_or_default()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("\\x{hex}")
    }
}
