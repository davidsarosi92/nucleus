//! A SQL session: runs statements, with PostgreSQL's transaction rules.
//!
//! - Outside `BEGIN`, each statement runs in its own transaction
//!   (autocommit) and is retried on a serialization conflict, up to 10
//!   times — safe because the statement is all the transaction does.
//! - Inside `BEGIN … COMMIT`, an error aborts the transaction: later
//!   statements fail with `25P02` until `ROLLBACK` (or `COMMIT`, which then
//!   rolls back).  A conflict at `COMMIT` is `40001`; the client retries.
//! - The KV transaction starts with the first statement after `BEGIN`, so
//!   `SET TRANSACTION` may follow `BEGIN`.
//! - DDL is transactional: the catalog lives in the same store.

use std::collections::BTreeMap;
use std::sync::Arc;

use nucleus_kv::{Isolation, Kv, KvTxn, TxnOptions};

use crate::ast::{IsolationLevel, Statement, TableConstraint, TxnMode};
use crate::binder::{Binder, Scope};
use crate::catalog::{self, Column, Index, Table};
use crate::dml;
use crate::error::{Error, Result};
use crate::exec;
use crate::expr::Ctx;
use crate::optimizer::optimize;
use crate::parser::{parse, parse_expr};
use crate::types::{DataType, Row, Value};

/// A result column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutColumn {
    /// Name.
    pub name: String,
    /// Type (`text` when unknown).
    pub ty: DataType,
}

/// What a statement returns.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Rows.
    Rows {
        /// The columns.
        columns: Vec<OutColumn>,
        /// The rows.
        rows: Vec<Row>,
    },
    /// A command tag: `CREATE TABLE`, `INSERT 0 3`, `BEGIN`, …
    Command(String),
}

/// Where the session is with transactions (PostgreSQL's
/// `ReadyForQuery` status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnStatus {
    /// Not in a transaction block.
    Idle,
    /// In a transaction block.
    InBlock,
    /// In a failed transaction block.
    Failed,
}

struct Block {
    txn: Option<Box<dyn KvTxn>>,
    /// `now()` of the transaction: when it started.
    now: i64,
    mode: TxnMode,
    failed: bool,
}

/// A session over a database.
pub struct Session {
    kv: Arc<dyn Kv>,
    block: Option<Block>,
    default_isolation: IsolationLevel,
    settings: BTreeMap<String, String>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn columns_of(cols: impl IntoIterator<Item = (String, Option<DataType>)>) -> Vec<OutColumn> {
    cols.into_iter()
        .map(|(name, ty)| OutColumn {
            name,
            ty: ty.unwrap_or(DataType::Text),
        })
        .collect()
}

fn options(mode: &TxnMode, default: IsolationLevel, read_only: bool) -> TxnOptions {
    let level = mode.isolation.unwrap_or(default);
    TxnOptions {
        isolation: if level == IsolationLevel::Serializable {
            Isolation::Serializable
        } else {
            Isolation::Snapshot
        },
        read_only: mode.read_only.unwrap_or(read_only),
        at: None,
    }
}

/// Whether a statement only reads.
fn read_only(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Query(_) | Statement::Explain(_) | Statement::Show(_)
    )
}

impl Session {
    /// A new session.
    pub fn new(kv: Arc<dyn Kv>) -> Self {
        Self {
            kv,
            block: None,
            default_isolation: IsolationLevel::Snapshot,
            settings: BTreeMap::new(),
        }
    }

    /// The transaction status.
    pub fn status(&self) -> TxnStatus {
        match &self.block {
            None => TxnStatus::Idle,
            Some(b) if b.failed => TxnStatus::Failed,
            Some(_) => TxnStatus::InBlock,
        }
    }

    /// Parses and runs every statement in `sql`; stops at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<Output>> {
        let mut out = Vec::new();
        for stmt in parse(sql)? {
            out.push(self.run(&stmt, &[])?);
        }
        Ok(out)
    }

    /// Runs one statement with parameter values.
    pub fn run(&mut self, stmt: &Statement, params: &[Value]) -> Result<Output> {
        match stmt {
            Statement::Begin(mode) => {
                if self.block.is_none() {
                    self.block = Some(Block {
                        txn: None,
                        now: 0,
                        mode: *mode,
                        failed: false,
                    });
                }
                Ok(Output::Command("BEGIN".into()))
            }
            Statement::Commit => {
                let Some(block) = self.block.take() else {
                    return Ok(Output::Command("COMMIT".into()));
                };
                if block.failed {
                    return Ok(Output::Command("ROLLBACK".into()));
                }
                if let Some(txn) = block.txn {
                    txn.commit()?;
                }
                Ok(Output::Command("COMMIT".into()))
            }
            Statement::Rollback => {
                if let Some(Block { txn: Some(t), .. }) = self.block.take() {
                    t.rollback();
                }
                Ok(Output::Command("ROLLBACK".into()))
            }
            Statement::SetTransaction(mode) => {
                match &mut self.block {
                    Some(b) if b.txn.is_some() => {
                        return Err(Error::active_txn(
                            "SET TRANSACTION ISOLATION LEVEL must be called before any query",
                        ));
                    }
                    Some(b) => {
                        b.mode.isolation = mode.isolation.or(b.mode.isolation);
                        b.mode.read_only = mode.read_only.or(b.mode.read_only);
                    }
                    None => {}
                }
                Ok(Output::Command("SET".into()))
            }
            Statement::Set { name, value } => {
                if name == "default_transaction_isolation" || name == "transaction_isolation" {
                    self.default_isolation = match value.to_ascii_lowercase().trim_matches('\'') {
                        "serializable" => IsolationLevel::Serializable,
                        "repeatable read" | "read committed" | "read uncommitted" | "snapshot" => {
                            IsolationLevel::Snapshot
                        }
                        v => {
                            return Err(Error::invalid_parameter(format!(
                                "invalid value for parameter \"{name}\": \"{v}\""
                            )));
                        }
                    };
                }
                self.settings.insert(name.clone(), value.clone());
                Ok(Output::Command("SET".into()))
            }
            Statement::Show(name) => {
                let value = match name.as_str() {
                    "transaction_isolation" | "default_transaction_isolation" => {
                        let level = self
                            .block
                            .as_ref()
                            .and_then(|b| b.mode.isolation)
                            .unwrap_or(self.default_isolation);
                        if level == IsolationLevel::Serializable {
                            "serializable"
                        } else {
                            "repeatable read"
                        }
                        .to_string()
                    }
                    "server_version" => "16.0 (nucleus)".into(),
                    "server_encoding" | "client_encoding" => "UTF8".into(),
                    other => self.settings.get(other).cloned().ok_or_else(|| {
                        Error::undefined_object(format!(
                            "unrecognized configuration parameter \"{other}\""
                        ))
                    })?,
                };
                Ok(Output::Rows {
                    columns: vec![OutColumn {
                        name: name.clone(),
                        ty: DataType::Text,
                    }],
                    rows: vec![vec![Value::Text(value)]],
                })
            }
            _ => self.run_in_txn(stmt, params),
        }
    }

    fn run_in_txn(&mut self, stmt: &Statement, params: &[Value]) -> Result<Output> {
        let default = self.default_isolation;
        if let Some(block) = &mut self.block {
            if block.failed {
                return Err(Error::in_failed_txn(
                    "current transaction is aborted, commands ignored until end of transaction block",
                ));
            }
            if block.txn.is_none() {
                block.txn = Some(self.kv.begin(options(&block.mode, default, false))?);
                block.now = crate::datetime::clock();
            }
            let txn = block.txn.as_mut().expect("just started");
            let r = crate::datetime::with_now(block.now, || execute(txn.as_mut(), stmt, params));
            if r.is_err() {
                block.failed = true;
            }
            return r;
        }
        // Autocommit, retried on conflicts.
        let opts = options(&TxnMode::default(), default, read_only(stmt));
        let mut attempt = 0;
        loop {
            let mut txn = self.kv.begin(opts)?;
            let now = crate::datetime::clock();
            let r = crate::datetime::with_now(now, || execute(txn.as_mut(), stmt, params))
                .and_then(|out| {
                    txn.commit()?;
                    Ok(out)
                });
            match r {
                Err(e) if e.code == "40001" && attempt < 10 => attempt += 1,
                r => return r,
            }
        }
    }

    /// The parameter types and result columns of a statement, without
    /// running it (for the extended query protocol).
    pub fn describe(
        &mut self,
        stmt: &Statement,
    ) -> Result<(Vec<Option<DataType>>, Option<Vec<OutColumn>>)> {
        let own;
        let txn: &dyn KvTxn = match self.block.as_ref().and_then(|b| b.txn.as_deref()) {
            Some(t) => t,
            None => {
                own = self.kv.begin(TxnOptions::read_only())?;
                own.as_ref()
            }
        };
        let mut binder = Binder::new(txn);
        let columns = match stmt {
            Statement::Query(q) => {
                let b = binder.query(q)?;
                Some(columns_of(b.scope.cols.into_iter().map(|c| (c.name, c.ty))))
            }
            Statement::Explain(_) => Some(vec![OutColumn {
                name: "QUERY PLAN".into(),
                ty: DataType::Text,
            }]),
            Statement::Show(name) => Some(vec![OutColumn {
                name: name.clone(),
                ty: DataType::Text,
            }]),
            Statement::Insert {
                table,
                columns,
                source,
                returning,
            } => {
                // `VALUES ($1, …)`: a parameter takes its target column's type.
                let t = catalog::table(txn, table)?;
                let targets: Vec<usize> = match columns {
                    None => (0..t.columns.len()).collect(),
                    Some(names) => names.iter().filter_map(|n| t.column(n)).collect(),
                };
                if let crate::ast::SetExpr::Values(rows) = &source.body {
                    for row in rows {
                        for (j, e) in row.iter().enumerate() {
                            if let (crate::ast::Expr::Param(n), Some(&c)) = (e, targets.get(j)) {
                                binder.note_param(n - 1, Some(t.columns[c].ty));
                            }
                        }
                    }
                }
                let _ = binder.query(source)?;
                returning_columns(&mut binder, txn, table, returning)?
            }
            Statement::Update {
                table,
                assignments,
                filter,
                returning,
            } => {
                let t = catalog::table(txn, table)?;
                let scope = dml::table_scope(&t);
                for (col, e) in assignments {
                    // A parameter assigned to a column takes its type.
                    if let (crate::ast::Expr::Param(n), Some(c)) = (e, t.column(col)) {
                        binder.note_param(n - 1, Some(t.columns[c].ty));
                    }
                    binder.expr(e, &scope)?;
                }
                if let Some(f) = filter {
                    binder.condition(f, &scope)?;
                }
                returning_columns(&mut binder, txn, table, returning)?
            }
            Statement::Delete {
                table,
                filter,
                returning,
            } => {
                let t = catalog::table(txn, table)?;
                if let Some(f) = filter {
                    binder.condition(f, &dml::table_scope(&t))?;
                }
                returning_columns(&mut binder, txn, table, returning)?
            }
            _ => None,
        };
        Ok((binder.params, columns))
    }
}

fn returning_columns(
    binder: &mut Binder,
    txn: &dyn KvTxn,
    table: &str,
    items: &[crate::ast::SelectItem],
) -> Result<Option<Vec<OutColumn>>> {
    if items.is_empty() {
        return Ok(None);
    }
    let t = catalog::table(txn, table)?;
    let scope = dml::table_scope(&t);
    let mut cols = Vec::new();
    for item in items {
        match item {
            crate::ast::SelectItem::Expr { expr, alias } => {
                let (_, ty) = binder.expr(expr, &scope)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    crate::ast::Expr::Column { name, .. } => name.clone(),
                    _ => "?column?".into(),
                });
                cols.push((name, ty));
            }
            _ => cols.extend(t.columns.iter().map(|c| (c.name.clone(), Some(c.ty)))),
        }
    }
    Ok(Some(columns_of(cols)))
}

fn written(tag: &str, w: dml::Written) -> Output {
    match w.returning {
        Some((cols, rows)) => Output::Rows {
            columns: columns_of(cols),
            rows,
        },
        None => Output::Command(format!("{tag} {}", w.count)),
    }
}

/// Runs a statement inside `txn`.
pub fn execute(txn: &mut dyn KvTxn, stmt: &Statement, params: &[Value]) -> Result<Output> {
    Ok(match stmt {
        Statement::Query(q) => {
            let ro: &dyn KvTxn = &*txn;
            let mut binder = Binder::new(ro);
            let bound = binder.query(q)?;
            let plan = optimize(bound.plan, ro);
            let ctx = Ctx::new(ro, params);
            let rows = exec::collect(&plan, &ctx, None)?;
            Output::Rows {
                columns: columns_of(bound.scope.cols.into_iter().map(|c| (c.name, c.ty))),
                rows,
            }
        }
        Statement::Explain(inner) => {
            let ro: &dyn KvTxn = &*txn;
            let plan = match &**inner {
                Statement::Query(q) => optimize(Binder::new(ro).query(q)?.plan, ro),
                Statement::Update { table, filter, .. }
                | Statement::Delete { table, filter, .. } => {
                    dml::explain_scan(ro, table, filter.as_ref())?
                }
                _ => {
                    return Err(Error::feature(
                        "EXPLAIN supports queries, UPDATE and DELETE",
                    ));
                }
            };
            let plan = crate::stats::annotate(ro, plan)?;
            Output::Rows {
                columns: vec![OutColumn {
                    name: "QUERY PLAN".into(),
                    ty: DataType::Text,
                }],
                rows: plan
                    .explain()
                    .into_iter()
                    .map(|l| vec![Value::Text(l)])
                    .collect(),
            }
        }
        Statement::CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
        } => {
            if catalog::get_table(&*txn, name)?.is_some() {
                if *if_not_exists {
                    return Ok(Output::Command("CREATE TABLE".into()));
                }
                return Err(Error::duplicate_table(format!(
                    "relation \"{name}\" already exists"
                )));
            }
            create_table(txn, name, columns, constraints)?;
            Output::Command("CREATE TABLE".into())
        }
        Statement::DropTable { names, if_exists } => {
            for name in names {
                match catalog::get_table(&*txn, name)? {
                    Some(t) => catalog::drop_table(txn, &t)?,
                    None if *if_exists => {}
                    None => {
                        return Err(Error::undefined_table(format!(
                            "table \"{name}\" does not exist"
                        )));
                    }
                }
            }
            Output::Command("DROP TABLE".into())
        }
        Statement::CreateIndex {
            name,
            table,
            columns,
            unique,
            if_not_exists,
        } => {
            if catalog::find_index(&*txn, name)?.is_some()
                || catalog::get_table(&*txn, name)?.is_some()
            {
                if *if_not_exists {
                    return Ok(Output::Command("CREATE INDEX".into()));
                }
                return Err(Error::duplicate_table(format!(
                    "relation \"{name}\" already exists"
                )));
            }
            let mut t = catalog::table(&*txn, table)?;
            let cols = columns
                .iter()
                .map(|c| {
                    t.column(c).ok_or_else(|| {
                        Error::undefined_column(format!("column \"{c}\" does not exist"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            t.indexes.push(Index {
                id: t.next_index_id,
                name: name.clone(),
                columns: cols,
                unique: *unique,
            });
            t.next_index_id += 1;
            let i = t.indexes.len() - 1;
            dml::backfill_index(txn, &t, i)?;
            catalog::update_table(txn, &t)?;
            Output::Command("CREATE INDEX".into())
        }
        Statement::DropIndex { name, if_exists } => {
            match catalog::find_index(&*txn, name)? {
                Some((mut t, i)) => {
                    let prefix = t.index_prefix(t.indexes[i].id);
                    catalog::delete_range(
                        txn,
                        &prefix,
                        nucleus_kv::prefix_end(&prefix).as_deref(),
                    )?;
                    t.indexes.remove(i);
                    catalog::update_table(txn, &t)?;
                }
                None if *if_exists => {}
                None => {
                    return Err(Error::undefined_object(format!(
                        "index \"{name}\" does not exist"
                    )));
                }
            }
            Output::Command("DROP INDEX".into())
        }
        Statement::AddColumn { table, column } => {
            let mut t = catalog::table(&*txn, table)?;
            if t.column(&column.name).is_some() {
                return Err(Error::duplicate_column(format!(
                    "column \"{}\" of relation \"{table}\" already exists",
                    column.name
                )));
            }
            if column.primary_key || column.unique {
                return Err(Error::feature(
                    "ADD COLUMN with PRIMARY KEY or UNIQUE is not supported yet",
                ));
            }
            t.columns.push(Column {
                name: column.name.clone(),
                ty: column.ty,
                nullable: !column.not_null,
                default: column.default.clone(),
                max_len: column.max_len,
                numeric: column.numeric,
            });
            if let Some(check) = &column.check {
                t.checks
                    .push((format!("{table}_{}_check", column.name), check.clone()));
            }
            let w = dml::Writer::new(&*txn, t.clone())?;
            let fill = {
                let ctx = Ctx::new(&*txn, &[]);
                w.default_of(t.columns.len() - 1, &ctx)?
            };
            let has_rows =
                !exec::matching_rows(&t, &crate::plan::Access::Full, None, &Ctx::new(&*txn, &[]))?
                    .is_empty();
            if has_rows {
                if fill.is_null() && column.not_null {
                    return Err(Error::not_null_violation(format!(
                        "column \"{}\" of relation \"{table}\" contains null values",
                        column.name
                    )));
                }
                if !fill.is_null() {
                    dml::rewrite_rows(txn, &t, &fill)?;
                }
            }
            catalog::update_table(txn, &t)?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::Insert {
            table,
            columns,
            source,
            returning,
        } => {
            let w = dml::insert(txn, table, columns, source, returning, params)?;
            match w.returning {
                Some(_) => written("INSERT 0", w),
                None => Output::Command(format!("INSERT 0 {}", w.count)),
            }
        }
        Statement::Update {
            table,
            assignments,
            filter,
            returning,
        } => written(
            "UPDATE",
            dml::update(txn, table, assignments, filter.as_ref(), returning, params)?,
        ),
        Statement::Delete {
            table,
            filter,
            returning,
        } => written(
            "DELETE",
            dml::delete(txn, table, filter.as_ref(), returning, params)?,
        ),
        Statement::Analyze(table) => {
            crate::stats::analyze(txn, table.as_deref())?;
            Output::Command("ANALYZE".into())
        }
        Statement::Begin(_)
        | Statement::Commit
        | Statement::Rollback
        | Statement::SetTransaction(_)
        | Statement::Set { .. }
        | Statement::Show(_) => {
            return Err(Error::internal("session statement reached the executor"));
        }
    })
}

fn create_table(
    txn: &mut dyn KvTxn,
    name: &str,
    specs: &[crate::ast::ColumnSpec],
    constraints: &[TableConstraint],
) -> Result<()> {
    let mut columns = Vec::new();
    let mut checks = Vec::new();
    let mut pk: Vec<usize> = Vec::new();
    let mut uniques: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        if columns.iter().any(|c: &Column| c.name == s.name) {
            return Err(Error::duplicate_column(format!(
                "column \"{}\" specified more than once",
                s.name
            )));
        }
        columns.push(Column {
            name: s.name.clone(),
            ty: s.ty,
            nullable: !s.not_null,
            default: s.default.clone(),
            max_len: s.max_len,
            numeric: s.numeric,
        });
        if s.primary_key {
            if !pk.is_empty() {
                return Err(Error::syntax(format!(
                    "multiple primary keys for table \"{name}\" are not allowed"
                )));
            }
            pk = vec![i];
        }
        if s.unique {
            uniques.push((format!("{name}_{}_key", s.name), vec![i]));
        }
        if let Some(c) = &s.check {
            checks.push((format!("{name}_{}_check", s.name), c.clone()));
        }
    }
    let find = |c: &String| -> Result<usize> {
        columns.iter().position(|x| &x.name == c).ok_or_else(|| {
            Error::undefined_column(format!("column \"{c}\" named in key does not exist"))
        })
    };
    for c in constraints {
        match c {
            TableConstraint::PrimaryKey(cols) => {
                if !pk.is_empty() {
                    return Err(Error::syntax(format!(
                        "multiple primary keys for table \"{name}\" are not allowed"
                    )));
                }
                pk = cols.iter().map(find).collect::<Result<_>>()?;
            }
            TableConstraint::Unique(n, cols) => {
                let idx: Vec<usize> = cols.iter().map(find).collect::<Result<_>>()?;
                let n = n
                    .clone()
                    .unwrap_or_else(|| format!("{name}_{}_key", cols.join("_")));
                uniques.push((n, idx));
            }
            TableConstraint::Check(n, text) => {
                checks.push((
                    n.clone()
                        .unwrap_or_else(|| format!("{name}_check{}", checks.len() + 1)),
                    text.clone(),
                ));
            }
        }
    }
    for &p in &pk {
        columns[p].nullable = false;
    }
    let mut table = Table {
        id: 0,
        name: name.to_string(),
        columns,
        primary_key: pk,
        indexes: Vec::new(),
        checks,
        next_index_id: 1,
    };
    for (n, cols) in uniques {
        if catalog::find_index(&*txn, &n)?.is_some() {
            return Err(Error::duplicate_table(format!(
                "relation \"{n}\" already exists"
            )));
        }
        table.indexes.push(Index {
            id: table.next_index_id,
            name: n,
            columns: cols,
            unique: true,
        });
        table.next_index_id += 1;
    }
    // Defaults and checks must bind now, not at the first insert.
    let w = dml::Writer::new(&*txn, table.clone())?;
    let ctx = Ctx::new(&*txn, &[]);
    for (i, c) in table.columns.iter().enumerate() {
        if c.default.is_some() {
            w.default_of(i, &ctx)?;
        }
    }
    for (_, text) in &table.checks {
        let _ = Binder::new(&*txn).condition(&parse_expr(text)?, &dml::table_scope(&table))?;
    }
    let _ = Scope::default();
    catalog::create_table(txn, table)?;
    Ok(())
}
