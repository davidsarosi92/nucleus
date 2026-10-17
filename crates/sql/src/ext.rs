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
//!         Ok(Signature::new(vec![Arg::Untyped(DataType::Int)], Returns::Type(Some(DataType::Int))))
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

use std::collections::HashMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::{Arc, OnceLock};

use nucleus_kv::KvTxn;

pub use crate::error::{Error, Result};
pub use crate::types::{DataType, Row, Value};

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
    schemas: Vec<Schema>,
    views: Vec<ViewRef>,
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
        module.register(&mut next)?;
        next.modules.push(module.name().to_string());
        *self = next;
        Ok(())
    }

    /// The modules added, in order.
    pub fn modules(&self) -> &[String] {
        &self.modules
    }

    /// Registers `f` under each of `names`.
    pub fn add_function(&mut self, names: &[&str], f: Arc<dyn ScalarFunction>) -> Result<()> {
        if let Some(n) = names.iter().find(|n| self.functions.contains_key(**n)) {
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

    /// Every function name, sorted.
    pub fn function_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.functions.keys().map(String::as_str).collect();
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
}
