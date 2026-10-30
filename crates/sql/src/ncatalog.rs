//! `nc_catalog`: nucleus's own system catalog, as read-only tables
//! (PLAN.md §M14.5, D13).
//!
//! It describes the database in nucleus's terms — table and index ids,
//! the key prefixes their entries live under, foreign key actions,
//! statistics — and is what nucleus's own tools and documentation use.
//! Its views are registered by the built-in module ([`crate::builtin`]);
//! `pg_catalog` / `information_schema` are another module's, for
//! PostgreSQL clients ([`crate::pgcatalog`]).
//!
//! | table | one row per |
//! |-------|-------------|
//! | `nc_catalog.schemas` | schema (of the session's registry) |
//! | `nc_catalog.tables` | table |
//! | `nc_catalog.columns` | column |
//! | `nc_catalog.indexes` | index (the primary index is index 0) |
//! | `nc_catalog.constraints` | primary key, unique, check, foreign key |
//! | `nc_catalog.foreign_keys` | foreign key |
//! | `nc_catalog.statistics` | analyzed column (`ANALYZE`) |
//! | `nc_catalog.statistics_objects` | `CREATE STATISTICS` |
//! | `nc_catalog.column_groups` | set of columns counted together (`ANALYZE`) |
//! | `nc_catalog.dependencies` | functional dependency between two columns (`ANALYZE`) |
//! | `nc_catalog.operators` | operator outside the grammar's own: a type's distance, a module's, or taught (`ALTER TYPE … ADD OPERATOR`) |
//! | `nc_catalog.casts` | cast taught (`ALTER TYPE … ADD CAST`) |
//! | `nc_catalog.orderings` | ordering taught to a type (`ALTER TYPE … ADD ORDERING`) |
//! | `nc_catalog.types` | type (of the session's registry), with its abilities |

use std::sync::Arc;

use nucleus_kv::KvTxn;

use crate::catalog::{self, Column, ConstraintKind, Table};
use crate::error::{Error, Result};
use crate::ext::{CallContext, CatalogView, Registry};
use crate::stats::Stats;
use crate::types::{DataType, Row, Value};

const B: DataType = DataType::Bool;
const I: DataType = DataType::BigInt;
const T: DataType = DataType::Text;
const TA: DataType = DataType::Array(&DataType::Text);
const F: DataType = DataType::Float;

const DEFS: &[(&str, &[(&str, DataType)])] = &[
    ("schemas", &[("name", T), ("description", T)]),
    (
        "tables",
        &[
            ("table_id", I),
            ("name", T),
            ("columns", I),
            ("primary_key", TA),
            ("has_row_id", B),
            ("indexes", I),
            ("checks", I),
            ("foreign_keys", I),
            ("referenced_by", I),
            ("key_prefix", T),
            ("analyzed_rows", I),
            ("owner", T),
            ("row_security", B),
            ("force_row_security", B),
            ("comment", T),
            ("storage", T),
        ],
    ),
    ("storages", &[("name", T)]),
    (
        "partitions",
        &[
            ("table_name", T),
            ("partition", T),
            ("strategy", T),
            ("key", TA),
            ("bound", T),
            ("storage", T),
        ],
    ),
    (
        "segments",
        &[
            ("table_name", T),
            ("segment", T),
            ("rows", I),
            ("deleted", I),
            ("bytes", I),
        ],
    ),
    (
        "policies",
        &[
            ("table_name", T),
            ("name", T),
            ("permissive", B),
            ("command", T),
            ("roles", TA),
            ("using", T),
            ("with_check", T),
        ],
    ),
    (
        "roles",
        &[
            ("name", T),
            ("superuser", B),
            ("login", B),
            ("inherit", B),
            ("createrole", B),
            ("connection_limit", I),
            ("has_password", B),
            ("valid_until", DataType::TimestampTz),
        ],
    ),
    (
        "audit",
        &[
            ("seq", I),
            ("at", DataType::TimestampTz),
            ("kind", T),
            ("ok", B),
            ("code", T),
            ("session_user_name", T),
            ("user_name", T),
            ("client_addr", T),
            ("front_end", T),
            ("auth_method", T),
            ("tls", B),
            ("access_rule", T),
            ("detail", T),
            ("hash", T),
        ],
    ),
    (
        "audit_chain",
        &[
            ("records", I),
            ("intact", B),
            ("broken_at", I),
            ("problem", T),
        ],
    ),
    (
        "database_settings",
        &[
            ("name", T),
            ("value", T),
            ("is_set", B),
            ("default", T),
            ("description", T),
        ],
    ),
    (
        "sessions",
        &[
            ("id", I),
            ("user_name", T),
            ("session_user_name", T),
            ("client_addr", T),
            ("front_end", T),
            ("auth_method", T),
            ("tls", B),
            ("access_rule", T),
            ("read_only", B),
            ("as_role", T),
            ("roles", TA),
            ("started", DataType::TimestampTz),
        ],
    ),
    (
        "statements",
        &[
            ("user_name", T),
            ("query", T),
            ("calls", I),
            ("errors", I),
            ("total_ms", F),
            ("mean_ms", F),
            ("min_ms", F),
            ("max_ms", F),
            ("rows_read", I),
            ("rows_returned", I),
            ("last_call", DataType::TimestampTz),
        ],
    ),
    (
        "index_advice",
        &[
            ("table_name", T),
            ("columns", TA),
            ("create_statement", T),
            ("statements", I),
            ("calls", I),
            ("rows_read", I),
            ("rows_read_with_index", I),
            ("example_query", T),
            ("plan", T),
        ],
    ),
    (
        "access_rules",
        &[
            ("position", I),
            ("name", T),
            ("roles", TA),
            ("addresses", TA),
            ("front_ends", TA),
            ("auth_methods", TA),
            ("tls", B),
            ("action", T),
            ("read_only", B),
            ("as_role", T),
        ],
    ),
    (
        "tokens",
        &[
            ("name", T),
            ("login", T),
            ("role", T),
            ("read_only", B),
            ("tables", TA),
            ("valid_until", DataType::TimestampTz),
            ("created", DataType::TimestampTz),
            ("created_by", T),
            ("max_rows", I),
            ("max_scan", I),
            ("timeout", DataType::Interval),
            ("rate_per_minute", I),
        ],
    ),
    (
        "role_members",
        &[
            ("role", T),
            ("member", T),
            ("admin_option", B),
            ("condition", T),
        ],
    ),
    (
        "grants",
        &[
            ("kind", T),
            ("object", T),
            ("column", T),
            ("grantee", T),
            ("privilege", T),
            ("grantable", B),
        ],
    ),
    (
        "columns",
        &[
            ("table_id", I),
            ("table_name", T),
            ("position", I),
            ("name", T),
            ("type", T),
            ("nullable", B),
            ("default_expr", T),
            ("in_primary_key", B),
            ("comment", T),
        ],
    ),
    (
        "indexes",
        &[
            ("table_id", I),
            ("table_name", T),
            ("index_id", I),
            ("name", T),
            ("columns", TA),
            ("is_unique", B),
            ("is_primary", B),
            ("key_prefix", T),
            ("orderings", TA),
        ],
    ),
    (
        "constraints",
        &[
            ("table_id", I),
            ("table_name", T),
            ("name", T),
            ("kind", T),
            ("definition", T),
        ],
    ),
    (
        "foreign_keys",
        &[
            ("table_id", I),
            ("table_name", T),
            ("name", T),
            ("columns", TA),
            ("referenced_table", T),
            ("referenced_columns", TA),
            ("on_delete", T),
            ("on_update", T),
        ],
    ),
    (
        "statistics",
        &[
            ("table_id", I),
            ("table_name", T),
            ("column_name", T),
            ("rows", I),
            ("nulls", I),
            ("distinct_values", I),
            ("histogram", TA),
        ],
    ),
    (
        "statistics_objects",
        &[
            ("table_id", I),
            ("table_name", T),
            ("name", T),
            ("columns", TA),
        ],
    ),
    (
        "column_groups",
        &[
            ("table_id", I),
            ("table_name", T),
            ("columns", TA),
            ("distinct_values", I),
        ],
    ),
    (
        "dependencies",
        &[
            ("table_id", I),
            ("table_name", T),
            ("from_column", T),
            ("to_column", T),
            ("degree", F),
        ],
    ),
    (
        "operators",
        &[
            ("symbol", T),
            ("left_type", T),
            ("right_type", T),
            ("function", T),
            ("source", T),
        ],
    ),
    (
        "casts",
        &[
            ("from_type", T),
            ("to_type", T),
            ("function", T),
            ("context", T),
        ],
    ),
    (
        "orderings",
        &[("type", T), ("name", T), ("mapping", T), ("used_by", TA)],
    ),
    (
        "enums",
        &[("type", T), ("label", T), ("position", I), ("sort_key", T)],
    ),
    (
        "types",
        &[
            ("name", T),
            ("aliases", TA),
            ("category", T),
            ("module", T),
            ("equality", B),
            ("hashing", B),
            ("ordering", B),
            ("key_encoding", B),
            ("distance", T),
            ("encoding_version", I),
            ("stored_version", I),
        ],
    ),
];

/// An `nc_catalog` view.
struct NcView {
    name: &'static str,
    columns: &'static [(&'static str, DataType)],
}

impl CatalogView for NcView {
    fn schema(&self) -> &str {
        "nc_catalog"
    }

    fn name(&self) -> &str {
        self.name
    }

    fn columns(&self) -> Vec<(String, DataType)> {
        self.columns
            .iter()
            .map(|(n, t)| ((*n).to_string(), *t))
            .collect()
    }

    fn rows(&self, cx: &CallContext<'_>) -> Result<Vec<Row>> {
        rows(self.name, cx.txn(), cx.registry())
    }
}

/// Every `nc_catalog` view.
pub fn views() -> Vec<Arc<dyn CatalogView>> {
    DEFS.iter()
        .map(|(name, columns)| Arc::new(NcView { name, columns }) as Arc<dyn CatalogView>)
        .collect()
}

/// A column's type as nucleus writes it.
pub fn type_text(c: &Column) -> String {
    match (c.ty, c.max_len, c.numeric) {
        (DataType::Text, Some(n), _) => format!("varchar({n})"),
        (DataType::Char, Some(n), _) => format!("char({n})"),
        (DataType::Numeric, _, Some((p, s))) => format!("numeric({p},{s})"),
        (DataType::Timestamp, ..) => "timestamp".into(),
        (DataType::TimestampTz, ..) => "timestamptz".into(),
        (t, ..) => t.name().into(),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

fn names(t: &Table, cols: &[usize]) -> Value {
    Value::Array(
        cols.iter()
            .map(|&c| text(t.columns[c].name.clone()))
            .collect(),
    )
}

fn id(t: &Table) -> Value {
    Value::Int(i64::from(t.id))
}

/// The rows of `nc_catalog.<name>`.
fn rows(name: &str, txn: &dyn KvTxn, registry: &Registry) -> Result<Vec<Row>> {
    let tables = catalog::tables(txn)?;
    let mut out: Vec<Row> = Vec::new();
    match name {
        "schemas" => {
            for s in registry.schemas() {
                out.push(vec![text(s.name.clone()), text(s.description.clone())]);
            }
        }
        "tables" => {
            let stats = Stats::new(txn);
            for t in &tables {
                out.push(vec![
                    id(t),
                    text(t.name.clone()),
                    Value::Int(t.columns.len() as i64),
                    names(t, &t.primary_key),
                    Value::Bool(t.primary_key.is_empty()),
                    Value::Int(t.indexes.len() as i64),
                    Value::Int(t.checks.len() as i64),
                    Value::Int(t.foreign_keys.len() as i64),
                    Value::Int(t.referenced_by.len() as i64),
                    text(hex(&t.data_prefix())),
                    stats
                        .of(t)
                        .map_or(Value::Null, |s| Value::Int(s.rows as i64)),
                    t.owner.clone().map_or(Value::Null, text),
                    Value::Bool(t.row_security),
                    Value::Bool(t.force_row_security),
                    t.comment.clone().map_or(Value::Null, text),
                    text(t.storage.clone()),
                ]);
            }
        }
        "partitions" => {
            for t in &tables {
                let Some(spec) = &t.partition_by else {
                    continue;
                };
                let strategy = match spec.kind {
                    crate::partition::Kind::Range => "range",
                    crate::partition::Kind::List => "list",
                    crate::partition::Kind::Hash => "hash",
                };
                for p in crate::partition::partitions(txn, t)? {
                    let bound = p
                        .partition_of
                        .as_ref()
                        .map(|(_, b)| b.render())
                        .unwrap_or_default();
                    out.push(vec![
                        text(t.name.clone()),
                        text(p.name.clone()),
                        text(strategy),
                        names(t, &spec.columns),
                        text(bound),
                        text(p.storage.clone()),
                    ]);
                }
            }
        }
        "storages" => {
            for name in crate::storage::names() {
                out.push(vec![text(name)]);
            }
        }
        "segments" => {
            for t in tables
                .iter()
                .filter(|t| t.storage == crate::column_store::COLUMN)
            {
                for (id, rows, gone, bytes) in crate::column_store::segment_info(txn, t)? {
                    out.push(vec![
                        text(t.name.clone()),
                        text(format!("{id:016x}")),
                        Value::Int(rows as i64),
                        Value::Int(gone as i64),
                        Value::Int(bytes as i64),
                    ]);
                }
            }
        }
        "roles" => {
            for r in crate::roles::all(txn)? {
                out.push(vec![
                    text(r.name),
                    Value::Bool(r.superuser),
                    Value::Bool(r.login),
                    Value::Bool(r.inherit),
                    Value::Bool(r.createrole),
                    r.connection_limit
                        .map_or(Value::Null, |n| Value::Int(i64::from(n))),
                    Value::Bool(r.password.is_some()),
                    r.valid_until.map_or(Value::Null, Value::TimestampTz),
                ]);
            }
        }
        // The audit log is an administrator's.
        "audit" if crate::access::current().superuser => {
            for r in crate::audit::all(txn)? {
                out.push(vec![
                    Value::Int(r.seq as i64),
                    Value::TimestampTz(r.at),
                    text(r.kind),
                    Value::Bool(r.ok),
                    if r.code.is_empty() {
                        Value::Null
                    } else {
                        text(r.code)
                    },
                    text(r.session_user),
                    text(r.user),
                    r.context
                        .client_addr
                        .map_or(Value::Null, |a| text(crate::rules::host(a).host_text())),
                    text(r.context.front_end),
                    text(r.context.auth_method),
                    Value::Bool(r.context.tls),
                    r.rule.map_or(Value::Null, text),
                    text(r.detail),
                    text(crate::audit::hex(&r.hash)),
                ]);
            }
        }
        "audit_chain" if crate::access::current().superuser => {
            let c = crate::audit::check(txn)?;
            out.push(vec![
                Value::Int(c.records as i64),
                Value::Bool(c.intact),
                c.broken_at.map_or(Value::Null, |n| Value::Int(n as i64)),
                c.problem.map_or(Value::Null, text),
            ]);
        }
        "audit" | "audit_chain" => {}
        "database_settings" => {
            for (k, value, set) in crate::dbsettings::all(txn)? {
                out.push(vec![
                    text(k.name.to_string()),
                    text(value),
                    Value::Bool(set),
                    text(k.default.to_string()),
                    text(k.description.to_string()),
                ]);
            }
        }
        "sessions" => {
            for s in crate::sessions::visible() {
                out.push(vec![
                    Value::Int(s.id as i64),
                    text(s.user),
                    text(s.session_user),
                    s.context
                        .client_addr
                        .map_or(Value::Null, |a| text(crate::rules::host(a).host_text())),
                    text(s.context.front_end),
                    text(s.context.auth_method),
                    Value::Bool(s.context.tls),
                    s.rule.map_or(Value::Null, text),
                    Value::Bool(s.read_only),
                    s.as_role.map_or(Value::Null, text),
                    Value::Array(s.roles.into_iter().map(text).collect()),
                    Value::TimestampTz(s.started),
                ]);
            }
        }
        "statements" => {
            let who = crate::access::current();
            let kv = crate::datetime::current_kv();
            let ms = |d: std::time::Duration| Value::Float(d.as_secs_f64() * 1000.0);
            for s in kv
                .map(|kv| crate::statements::all(&kv, (!who.superuser).then_some(&who.user)))
                .unwrap_or_default()
            {
                out.push(vec![
                    text(s.user),
                    text(s.query),
                    Value::Int(s.calls as i64),
                    Value::Int(s.errors as i64),
                    ms(s.total),
                    Value::Float(s.total.as_secs_f64() * 1000.0 / s.calls.max(1) as f64),
                    ms(s.min),
                    ms(s.max),
                    Value::Int(s.rows_read as i64),
                    Value::Int(s.rows_returned as i64),
                    Value::TimestampTz(s.last),
                ]);
            }
        }
        "index_advice" => {
            let who = crate::access::current();
            let stats = crate::datetime::current_kv()
                .map(|kv| crate::statements::all(&kv, (!who.superuser).then_some(&who.user)))
                .unwrap_or_default();
            for a in crate::advisor::advise(txn, registry, &stats)? {
                out.push(vec![
                    text(a.table),
                    Value::Array(a.columns.into_iter().map(text).collect()),
                    text(a.create),
                    Value::Int(a.statements as i64),
                    Value::Int(a.calls as i64),
                    Value::Int(a.rows_read.round() as i64),
                    Value::Int(a.rows_read_with.round() as i64),
                    text(a.example),
                    text(a.plan.join("\n")),
                ]);
            }
        }
        "access_rules" => {
            let list = |v: Vec<String>| Value::Array(v.into_iter().map(text).collect());
            for r in crate::rules::all(txn)? {
                let (action, read_only, as_role) = match r.action {
                    crate::rules::Action::Reject => ("REJECT", false, None),
                    crate::rules::Action::Allow { read_only, as_role } => {
                        ("ALLOW", read_only, as_role)
                    }
                };
                out.push(vec![
                    Value::Int(i64::from(r.position)),
                    text(r.name),
                    list(r.roles),
                    list(r.from),
                    list(r.via),
                    list(r.auth),
                    r.tls.map_or(Value::Null, Value::Bool),
                    text(action.to_string()),
                    Value::Bool(read_only),
                    as_role.map_or(Value::Null, text),
                ]);
            }
        }
        "tokens" => {
            for t in crate::tokens::visible(txn)? {
                out.push(vec![
                    text(t.name),
                    text(t.login),
                    text(t.role),
                    Value::Bool(t.read_only),
                    t.tables.map_or(Value::Null, |ts| {
                        Value::Array(ts.into_iter().map(text).collect())
                    }),
                    t.valid_until.map_or(Value::Null, Value::TimestampTz),
                    Value::TimestampTz(t.created),
                    text(t.created_by),
                    t.budget.rows.map_or(Value::Null, |n| Value::Int(n as i64)),
                    t.budget.scan.map_or(Value::Null, |n| Value::Int(n as i64)),
                    t.budget.timeout.map_or(Value::Null, |us| {
                        Value::Interval(crate::datetime::Interval {
                            months: 0,
                            days: 0,
                            micros: us,
                        })
                    }),
                    t.budget
                        .rate
                        .map_or(Value::Null, |n| Value::Int(i64::from(n))),
                ]);
            }
        }
        "policies" => {
            for p in crate::policies::all(txn)? {
                out.push(vec![
                    text(p.table),
                    text(p.name),
                    Value::Bool(p.permissive),
                    text(p.command.word()),
                    Value::Array(p.roles.into_iter().map(text).collect()),
                    p.using.map_or(Value::Null, text),
                    p.check.map_or(Value::Null, text),
                ]);
            }
        }
        "grants" => {
            for (kind, object, acl) in crate::privileges::all(txn)? {
                for (grantee, e) in acl {
                    let mut row = |column: Option<&str>, bits: u32, grantable: u32| {
                        for name in crate::privileges::names(bits) {
                            let bit = crate::privileges::by_name(name).expect("a privilege");
                            out.push(vec![
                                text(kind.word()),
                                text(object.clone()),
                                column.map_or(Value::Null, text),
                                text(grantee.clone()),
                                text(name),
                                Value::Bool(grantable & bit != 0),
                            ]);
                        }
                    };
                    row(None, e.privileges, e.grantable);
                    for (c, (p, g)) in &e.columns {
                        row(Some(c), *p, *g);
                    }
                }
            }
        }
        "role_members" => {
            for m in crate::roles::memberships(txn)? {
                out.push(vec![
                    text(m.role),
                    text(m.member),
                    Value::Bool(m.admin_option),
                    m.condition.map_or(Value::Null, text),
                ]);
            }
        }
        "columns" => {
            for t in &tables {
                for (c, col) in t.columns.iter().enumerate() {
                    let in_pk = t.primary_key.contains(&c);
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(c as i64 + 1),
                        text(col.name.clone()),
                        text(type_text(col)),
                        Value::Bool(col.nullable && !in_pk),
                        col.default.clone().map_or(Value::Null, text),
                        Value::Bool(in_pk),
                        col.comment.clone().map_or(Value::Null, text),
                    ]);
                }
            }
        }
        "indexes" => {
            for t in &tables {
                if !t.primary_key.is_empty() {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(0),
                        text(format!("{}_pkey", t.name)),
                        names(t, &t.primary_key),
                        Value::Bool(true),
                        Value::Bool(true),
                        text(hex(&t.index_prefix(0))),
                        Value::Array(vec![Value::Null; t.primary_key.len()]),
                    ]);
                }
                for ix in &t.indexes {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        Value::Int(i64::from(ix.id)),
                        text(ix.name.clone()),
                        names(t, &ix.columns),
                        Value::Bool(ix.unique),
                        Value::Bool(false),
                        text(hex(&t.index_prefix(ix.id))),
                        Value::Array(
                            ix.orderings
                                .iter()
                                .map(|o| o.clone().map_or(Value::Null, text))
                                .collect(),
                        ),
                    ]);
                }
            }
        }
        "constraints" => {
            for c in catalog::constraints(&tables) {
                let kind = match c.kind {
                    ConstraintKind::PrimaryKey => "primary key",
                    ConstraintKind::Unique(_) => "unique",
                    ConstraintKind::Check(_) => "check",
                    ConstraintKind::ForeignKey(_) => "foreign key",
                };
                out.push(vec![
                    id(c.table),
                    text(c.table.name.clone()),
                    text(c.name),
                    text(kind),
                    text(c.definition),
                ]);
            }
        }
        "foreign_keys" => {
            for t in &tables {
                for fk in &t.foreign_keys {
                    let parent = tables.iter().find(|p| p.name == fk.table);
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        text(fk.name.clone()),
                        names(t, &fk.columns),
                        text(fk.table.clone()),
                        parent.map_or(Value::Null, |p| names(p, &fk.ref_columns)),
                        text(fk.on_delete.sql().to_lowercase()),
                        text(fk.on_update.sql().to_lowercase()),
                    ]);
                }
            }
        }
        "statistics" => {
            let stats = Stats::new(txn);
            for t in &tables {
                let Some(s) = stats.of(t) else { continue };
                for (c, cs) in s.columns.iter().enumerate() {
                    let Some(col) = t.columns.get(c) else {
                        continue;
                    };
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        text(col.name.clone()),
                        Value::Int(s.rows as i64),
                        Value::Int(cs.nulls as i64),
                        Value::Int(cs.distinct as i64),
                        Value::Array(cs.bounds.iter().map(|b| text(b.render())).collect()),
                    ]);
                }
            }
        }
        "statistics_objects" => {
            for t in &tables {
                for st in &t.statistics {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        text(st.name.clone()),
                        names(t, &st.columns),
                    ]);
                }
            }
        }
        "column_groups" => {
            let stats = Stats::new(txn);
            for t in &tables {
                let Some(s) = stats.of(t) else { continue };
                for g in &s.groups {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        names(t, &g.columns),
                        Value::Int(g.distinct as i64),
                    ]);
                }
            }
        }
        "dependencies" => {
            let stats = Stats::new(txn);
            for t in &tables {
                let Some(s) = stats.of(t) else { continue };
                let name = |c: usize| {
                    t.columns
                        .get(c)
                        .map_or(Value::Null, |col| text(col.name.clone()))
                };
                for d in &s.dependencies {
                    out.push(vec![
                        id(t),
                        text(t.name.clone()),
                        name(d.from),
                        name(d.to),
                        Value::Float(d.degree),
                    ]);
                }
            }
        }
        "operators" => {
            let name = |t: DataType| text(crate::builtin::type_of(t).name());
            for (ty, _) in registry.types() {
                if let Some(d) = crate::builtin::type_of(*ty).distance() {
                    let f = crate::ext::DistanceFn::of(*ty);
                    out.push(vec![
                        text(d.operator()),
                        name(*ty),
                        name(*ty),
                        text(f.name()),
                        text("distance"),
                    ]);
                }
            }
            for (symbol, l, r, f) in registry.operators() {
                out.push(vec![
                    text(symbol),
                    name(l),
                    name(r),
                    text(f.name()),
                    text("registered"),
                ]);
            }
            for o in catalog::operators(txn)? {
                out.push(vec![
                    text(o.symbol),
                    text(o.left),
                    text(o.right),
                    text(o.function),
                    text("taught"),
                ]);
            }
        }
        "enums" => {
            for e in crate::enums::all(txn)? {
                for (i, (key, label)) in e.labels.iter().enumerate() {
                    out.push(vec![
                        text(e.name.clone()),
                        text(label.clone()),
                        Value::Int(i as i64 + 1),
                        text(hex(key)),
                    ]);
                }
            }
        }
        "casts" => {
            for c in catalog::casts(txn)? {
                out.push(vec![
                    text(c.from),
                    text(c.to),
                    text(c.function),
                    text(c.context.sql()),
                ]);
            }
        }
        "orderings" => {
            for o in catalog::orderings(txn)? {
                let mut users = Vec::new();
                for t in &tables {
                    for ix in &t.indexes {
                        let uses = ix.columns.iter().zip(&ix.orderings).any(|(&c, u)| {
                            u.as_deref() == Some(o.name.as_str())
                                && crate::builtin::type_of(t.columns[c].ty).name() == o.type_name
                        });
                        if uses {
                            users.push(text(ix.name.clone()));
                        }
                    }
                }
                out.push(vec![
                    text(o.type_name),
                    text(o.name),
                    text(o.mapping),
                    Value::Array(users),
                ]);
            }
        }
        "types" => {
            for (ty, module) in registry.types() {
                let ty = *ty;
                let t = crate::builtin::type_of(ty);
                let mut aliases: Vec<Value> = t.aliases().iter().map(|a| text(*a)).collect();
                aliases.extend(
                    registry
                        .type_aliases(ty)
                        .into_iter()
                        .filter(|a| a != t.name() && !t.aliases().contains(&a.as_str()))
                        .map(text),
                );
                out.push(vec![
                    text(t.name()),
                    Value::Array(aliases),
                    text(t.category()),
                    text(module.clone()),
                    Value::Bool(t.equality().is_some()),
                    Value::Bool(t.hashing().is_some()),
                    Value::Bool(t.ordering().is_some()),
                    Value::Bool(t.key_encoding().is_some()),
                    t.distance().map_or(Value::Null, |d| text(d.operator())),
                    Value::Int(i64::from(t.encoding_version())),
                    match ty {
                        DataType::Ext(r) => catalog::stored_version(txn, r.name())?
                            .map_or(Value::Null, |v| Value::Int(i64::from(v))),
                        _ => Value::Null,
                    },
                ]);
            }
        }
        _ => return Err(Error::internal(format!("no nc_catalog table {name}"))),
    }
    Ok(out)
}
