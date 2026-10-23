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
use crate::ext::Registry;
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

/// The command tag of a statement's output, as clients show it: the
/// output's own for commands, else `INSERT 0 n`, `UPDATE n`, `DELETE n`
/// (with `RETURNING`), `SHOW`, `EXPLAIN` or `SELECT n`.
pub fn command_tag(stmt: &Statement, out: &Output) -> String {
    match out {
        Output::Command(tag) => tag.clone(),
        Output::Rows { rows, .. } => match stmt {
            Statement::Insert { .. } => format!("INSERT 0 {}", rows.len()),
            Statement::Update { .. } => format!("UPDATE {}", rows.len()),
            Statement::Delete { .. } => format!("DELETE {}", rows.len()),
            Statement::Show(_) => "SHOW".into(),
            Statement::Explain(_) => "EXPLAIN".into(),
            _ => format!("SELECT {}", rows.len()),
        },
    }
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
    registry: Arc<Registry>,
    block: Option<Block>,
    default_isolation: IsolationLevel,
    settings: BTreeMap<String, String>,
    /// The database's format record has been checked (or written).
    format_checked: bool,
    /// Who its statements run as.
    who: Arc<crate::access::Who>,
    /// Who it logged in as (`SET ROLE` changes `who`, not this).
    login: Arc<crate::access::Who>,
    /// Its place in `nc_catalog.sessions`, once a front end admitted it.
    listed: Option<crate::sessions::Entry>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// A query of a `WHEN` condition over a connection's context: `$1` the
/// client's address (`source`, an `inet`), `$2` the authentication method
/// (`auth`), `$3` the front end, `$4` TLS.
fn condition_query(condition: &str) -> String {
    format!(
        "SELECT ({condition}) FROM (SELECT $1::inet AS source, $2::text AS auth, \
         $3::text AS front_end, $4::boolean AS tls) AS connection"
    )
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
    /// A new session with the built-in functions and views
    /// ([`Registry::builtin`]).
    pub fn new(kv: Arc<dyn Kv>) -> Self {
        Self::with_registry(kv, Registry::shared_builtin())
    }

    /// A new session with the functions and views of `registry`.
    pub fn with_registry(kv: Arc<dyn Kv>, registry: Arc<Registry>) -> Self {
        Self {
            kv,
            registry,
            block: None,
            default_isolation: IsolationLevel::Snapshot,
            settings: BTreeMap::new(),
            format_checked: false,
            who: Arc::new(crate::access::Who::system()),
            login: Arc::new(crate::access::Who::system()),
            listed: None,
        }
    }

    /// Logs the session in as `user` (authenticated already): its
    /// statements run as that role from now on (`28000` if the database
    /// has no such role and is claimed — [`crate::roles`]).
    pub fn login(&mut self, user: &str) -> Result<()> {
        let txn = self.kv.begin(TxnOptions::read_only())?;
        let who = Arc::new(crate::roles::login_identity(&*txn, user)?);
        // CONNECT on the database (PUBLIC has it unless revoked).
        crate::access::with_who(&who, || {
            crate::privileges::Checker::new().require(
                &*txn,
                crate::privileges::Kind::Database,
                crate::privileges::DATABASE,
                crate::privileges::CONNECT,
                None,
            )
        })?;
        self.login = who;
        self.who = self.login.clone();
        Ok(())
    }

    /// Logs the session in as an identity provider's subject
    /// (authenticated already, [`crate::access::Identity`]): as its role,
    /// with the roles it was granted for the session — those that exist and
    /// are not administrators — and its claims.
    pub fn login_as(&mut self, identity: &crate::access::Identity) -> Result<()> {
        // The role must exist and may log in — even in an unclaimed
        // database, whose bootstrap admits any name as its administrator
        // (that is for the server's own credentials, not an issuer's).
        {
            let txn = self.kv.begin(TxnOptions::read_only())?;
            if identity.user.starts_with(crate::tokens::PREFIX)
                || !crate::roles::get(&*txn, &identity.user)?.is_some_and(|r| r.login)
            {
                return Err(Error::invalid_authorization(format!(
                    "role \"{}\" does not exist or may not log in",
                    identity.user
                )));
            }
        }
        self.login(&identity.user)?;
        let txn = self.kv.begin(TxnOptions::read_only())?;
        let mut who = (*self.login).clone();
        for r in &identity.roles {
            if crate::roles::get(&*txn, r)?.is_some_and(|role| !role.superuser) {
                who.session_roles.insert(r.clone());
            }
        }
        who.claims = identity
            .claims
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        self.login = Arc::new(who);
        self.who = self.login.clone();
        Ok(())
    }

    /// Admits the logged-in session as connected in `context`, if an
    /// access rule does ([`crate::rules`], `28000` otherwise), under the
    /// rule's ceiling; the context is the session's from then on.
    pub fn admit(&mut self, context: crate::access::Context) -> Result<()> {
        // Memberships under a WHEN condition the connection does not meet
        // are not held in this session.
        let mut suspended = std::collections::BTreeSet::new();
        let conditional: Vec<crate::roles::Membership> = {
            let txn = self.kv.begin(TxnOptions::read_only())?;
            crate::roles::memberships(&*txn)?
                .into_iter()
                .filter(|m| m.condition.is_some())
                .collect()
        };
        if !conditional.is_empty() {
            let mut probe = Session::with_registry(self.kv.clone(), self.registry.clone());
            let params = [
                context.client_addr.map_or(Value::Null, |a| {
                    Value::Text(crate::rules::host(a).host_text())
                }),
                Value::Text(context.auth_method.clone()),
                Value::Text(context.front_end.clone()),
                Value::Bool(context.tls),
            ];
            for m in conditional {
                let text = condition_query(m.condition.as_deref().unwrap_or("false"));
                let holds = parse(&text).and_then(|stmts| probe.run(&stmts[0], &params));
                let holds = matches!(
                    holds,
                    Ok(Output::Rows { rows, .. }) if rows.first().and_then(|r| r.first()) == Some(&Value::Bool(true))
                );
                if !holds {
                    suspended.insert((m.role, m.member));
                }
            }
        }
        let txn = self.kv.begin(TxnOptions::read_only())?;
        let rule = crate::rules::admit(&*txn, &self.login.session_user, &context, &suspended)?;
        let mut who = (*self.login).clone();
        who.suspended = suspended;
        if let Some(r) = &rule
            && let crate::rules::Action::Allow { read_only, as_role } = &r.action
        {
            who.ceiling.read_only |= *read_only;
            if as_role.is_some() {
                who.ceiling.as_role = as_role.clone();
            }
        }
        who.rule = rule.map(|r| r.name);
        who.context = context;
        let roles = crate::roles::effective_for(&*txn, &who)?;
        let started = crate::datetime::from_system(self.kv.wall_clock());
        self.listed = Some(crate::sessions::register(
            &self.kv,
            crate::sessions::Info {
                id: 0,
                user: who.user.clone(),
                session_user: who.session_user.clone(),
                context: who.context.clone(),
                rule: who.rule.clone(),
                read_only: who.ceiling.read_only,
                as_role: who.ceiling.as_role.clone(),
                roles: roles.into_iter().collect(),
                started,
            },
        ));
        self.login = Arc::new(who);
        self.who = self.login.clone();
        Ok(())
    }

    /// Who the session's statements run as.
    pub fn who(&self) -> &crate::access::Who {
        &self.who
    }

    /// The first statement of a session checks the database's format
    /// ([`catalog::check_format`]).
    fn check_format(&mut self, txn: &mut dyn KvTxn) -> Result<()> {
        if !self.format_checked {
            self.format_checked = crate::catalog::check_format(txn, &self.registry)?;
        }
        Ok(())
    }

    /// The session's registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
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
                if crate::access::CONTEXT_SETTINGS.contains(&name.as_str()) {
                    return Err(Error::cant_change(format!(
                        "parameter \"{name}\" cannot be changed: it is how the session connected"
                    )));
                }
                if name.starts_with(crate::access::CLAIMS) {
                    return Err(Error::cant_change(format!(
                        "parameter \"{name}\" cannot be changed: it is a claim of the login"
                    )));
                }
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
            Statement::Reset(name) => {
                match name {
                    Some(n) => {
                        self.settings.remove(n);
                        if n == "default_transaction_isolation" {
                            self.default_isolation = IsolationLevel::Snapshot;
                        }
                    }
                    None => {
                        self.settings.clear();
                        self.default_isolation = IsolationLevel::Snapshot;
                    }
                }
                Ok(Output::Command("RESET".into()))
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
            Statement::SetRole(role) => {
                let who = match self.block.as_ref().and_then(|b| b.txn.as_ref()) {
                    Some(txn) => {
                        crate::roles::set_role(txn.as_ref(), &self.login, role.as_deref())?
                    }
                    None => {
                        let txn = self.kv.begin(TxnOptions::read_only())?;
                        crate::roles::set_role(&*txn, &self.login, role.as_deref())?
                    }
                };
                if let Some(l) = &self.listed {
                    l.set_user(&who.user);
                }
                self.who = Arc::new(who);
                Ok(Output::Command("SET".into()))
            }
            _ => {
                let mut who = (*self.who).clone();
                who.settings = self.settings.clone();
                let who = Arc::new(who);
                crate::access::with_who(&who, || self.run_in_txn(stmt, params))
            }
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
                let mut opts = options(&block.mode, default, false);
                opts.read_only |= self.who.ceiling.read_only;
                block.txn = Some(self.kv.begin(opts)?);
                block.now = crate::datetime::from_system(self.kv.wall_clock());
            }
            let txn = block.txn.as_mut().expect("just started");
            let r = if self.format_checked {
                Ok(())
            } else {
                crate::catalog::check_format(txn.as_mut(), &self.registry)
                    .map(|ok| self.format_checked = ok)
            };
            let r = r.and_then(|()| {
                crate::datetime::with_now(block.now, &self.kv, || {
                    execute(txn.as_mut(), &self.registry, stmt, params)
                })
            });
            if r.is_err() {
                block.failed = true;
            }
            return r;
        }
        // Autocommit, retried on conflicts.
        let mut opts = options(&TxnMode::default(), default, read_only(stmt));
        opts.read_only |= self.who.ceiling.read_only;
        let mut attempt = 0;
        loop {
            let mut txn = self.kv.begin(opts)?;
            let now = crate::datetime::from_system(self.kv.wall_clock());
            let r = self
                .check_format(txn.as_mut())
                .and_then(|()| {
                    crate::datetime::with_now(now, &self.kv, || {
                        execute(txn.as_mut(), &self.registry, stmt, params)
                    })
                })
                .and_then(|out| {
                    txn.commit()?;
                    Ok(out)
                });
            match r {
                Err(e) if e.code == "40001" && attempt < 10 => {
                    attempt += 1;
                    self.kv.back_off(attempt);
                }
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
        let mut binder = Binder::new(txn, &self.registry).checked();
        let columns = match stmt {
            Statement::Query(q) => {
                let b = binder.query(q)?;
                Some(columns_of(b.scope.cols.into_iter().map(|c| (c.name, c.ty))))
            }
            Statement::Explain(_) => Some(vec![OutColumn {
                name: "QUERY PLAN".into(),
                ty: DataType::Text,
            }]),
            Statement::CreateToken { .. } => Some(columns_of(
                ["name", "login", "secret"].map(|c| (c.to_string(), Some(DataType::Text))),
            )),
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
                let types = targets.iter().map(|&c| t.columns[c].ty).collect();
                let _ = binder.insert_source(source, types)?;
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
pub fn execute(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    stmt: &Statement,
    params: &[Value],
) -> Result<Output> {
    crate::privileges::authorize_ddl(&*txn, reg, stmt)?;
    Ok(match stmt {
        Statement::Query(q) => {
            let ro: &dyn KvTxn = &*txn;
            let mut binder = Binder::new(ro, reg).checked();
            let bound = binder.query(q)?;
            let plan = optimize(bound.plan, ro);
            let ctx = Ctx::new(ro, reg, params);
            let rows = exec::collect(&plan, &ctx, None)?;
            Output::Rows {
                columns: columns_of(bound.scope.cols.into_iter().map(|c| (c.name, c.ty))),
                rows,
            }
        }
        Statement::Explain(inner) => {
            let ro: &dyn KvTxn = &*txn;
            let plan = match &**inner {
                Statement::Query(q) => optimize(Binder::new(ro, reg).checked().query(q)?.plan, ro),
                Statement::Update { table, filter, .. }
                | Statement::Delete { table, filter, .. } => {
                    dml::explain_scan(ro, reg, table, filter.as_ref())?
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
            create_table(txn, reg, name, columns, constraints)?;
            let owner = crate::access::current().user.clone();
            crate::privileges::on_create_table(txn, name, &owner)?;
            Output::Command("CREATE TABLE".into())
        }
        Statement::DropTable {
            names,
            if_exists,
            cascade,
        } => {
            let checker = crate::privileges::Checker::new();
            for name in names {
                match catalog::get_table(&*txn, name)? {
                    Some(mut t) => {
                        checker.require_owner(&*txn, crate::privileges::Kind::Table, name)?;
                        crate::privileges::forget(txn, crate::privileges::Kind::Table, name)?;
                        crate::policies::forget(txn, name)?;
                        crate::tokens::forget_table(txn, name)?;
                        // Foreign keys of other tables referencing it.
                        for (child_name, fk_name) in t.referenced_by.clone() {
                            if child_name == t.name || names.contains(&child_name) {
                                continue;
                            }
                            if !*cascade {
                                return Err(Error::dependent_objects(format!(
                                    "cannot drop table {name} because other objects depend on it: constraint {fk_name} on table {child_name}"
                                )));
                            }
                            let mut child = catalog::table(&*txn, &child_name)?;
                            if let Some(i) =
                                child.foreign_keys.iter().position(|f| f.name == fk_name)
                            {
                                child.foreign_keys.remove(i);
                                catalog::update_table(txn, &child)?;
                            }
                        }
                        // Its own foreign keys, from the tables they reference.
                        while let Some(i) = t.foreign_keys.iter().position(|f| f.table != t.name) {
                            crate::fk::detach(txn, &mut t, i)?;
                        }
                        catalog::drop_table(txn, &t)?
                    }
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
            orderings,
            unique,
            if_not_exists,
        } => {
            let name = &match name {
                Some(n) => n.clone(),
                None => {
                    // PostgreSQL's: table_columns_idx, numbered if taken.
                    let base = format!("{table}_{}_idx", columns.join("_"));
                    let mut n = base.clone();
                    let mut k = 1;
                    while catalog::find_index(&*txn, &n)?.is_some()
                        || catalog::get_table(&*txn, &n)?.is_some()
                    {
                        n = format!("{base}{k}");
                        k += 1;
                    }
                    n
                }
            };
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
            let mut plain = Vec::new();
            for (&c, o) in cols.iter().zip(orderings) {
                let Some(o) = o else {
                    plain.push(c);
                    continue;
                };
                if *unique {
                    return Err(Error::feature(
                        "a UNIQUE index in a taught ordering is not supported: its mapping may have ties",
                    ));
                }
                let ty = t.columns[c].ty;
                if let crate::binder::OrderingUse::Mapped { result, .. } =
                    Binder::new(&*txn, reg).ordering(Some(ty), o)?
                {
                    crate::binder::require(
                        result,
                        &[crate::binder::Need::KeyEncoding],
                        "an index in an ordering",
                    )?;
                } else {
                    plain.push(c);
                }
            }
            require_keys(&t.columns, &plain, *unique, "an index")?;
            let mut ix = Index::plain(t.next_index_id, name.clone(), cols, *unique);
            ix.orderings = orderings.clone();
            t.indexes.push(ix);
            t.next_index_id += 1;
            let i = t.indexes.len() - 1;
            dml::backfill_index(txn, reg, &t, i)?;
            catalog::update_table(txn, &t)?;
            Output::Command("CREATE INDEX".into())
        }
        Statement::AlterColumnType {
            table,
            column,
            ty,
            typmod,
            using,
        } => {
            alter_column_type(txn, reg, table, column, ty, typmod, using.as_ref())?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::AlterType { type_name, action } => {
            alter_type(txn, reg, type_name, action)?;
            Output::Command("ALTER TYPE".into())
        }
        Statement::CreateRole { .. }
        | Statement::AlterRole { .. }
        | Statement::RenameRole { .. }
        | Statement::DropRole { .. }
        | Statement::GrantRole { .. }
        | Statement::RevokeRole { .. } => {
            if let Statement::GrantRole {
                condition: Some(c), ..
            } = stmt
            {
                // The condition must make sense over a connection.
                let probe = crate::parser::parse(&condition_query(c))?;
                let crate::ast::Statement::Query(q) = &probe[0] else {
                    return Err(Error::syntax("a bad condition"));
                };
                let b = Binder::new(&*txn, reg).query(q)?;
                if b.scope
                    .cols
                    .first()
                    .and_then(|c| c.ty)
                    .is_some_and(|t| t != DataType::Bool)
                {
                    return Err(Error::type_mismatch("a WHEN condition must be a boolean"));
                }
            }
            Output::Command(crate::roles::run(txn, stmt)?)
        }
        Statement::CreateToken {
            name,
            role,
            read_only,
            tables,
            valid_until,
        } => {
            let (t, secret) = crate::tokens::create(
                txn,
                name,
                role.as_deref(),
                *read_only,
                tables.as_deref(),
                valid_until.as_deref(),
            )?;
            Output::Rows {
                columns: columns_of([
                    ("name".to_string(), Some(DataType::Text)),
                    ("login".to_string(), Some(DataType::Text)),
                    ("secret".to_string(), Some(DataType::Text)),
                ]),
                rows: vec![vec![
                    Value::Text(t.name),
                    Value::Text(t.login),
                    Value::Text(secret),
                ]],
            }
        }
        Statement::CreateAccessRule(rule, place) => {
            crate::rules::create(txn, rule.clone(), place)?;
            Output::Command("CREATE ACCESS RULE".into())
        }
        Statement::DropAccessRule { name, if_exists } => {
            crate::rules::drop(txn, name, *if_exists)?;
            Output::Command("DROP ACCESS RULE".into())
        }
        Statement::DropToken { name, if_exists } => {
            crate::tokens::drop(txn, name, *if_exists)?;
            Output::Command("DROP TOKEN".into())
        }
        Statement::SetRole(_) | Statement::Reset(_) => unreachable!("handled by the session"),
        Statement::SetRowSecurity {
            table,
            enable,
            force,
        } => {
            let mut t = catalog::table(&*txn, table)?;
            if let Some(e) = enable {
                t.row_security = *e;
            }
            if let Some(f) = force {
                t.force_row_security = *f;
            }
            catalog::update_table(txn, &t)?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::CreatePolicy(p) => {
            let t = catalog::table(&*txn, &p.table)?;
            if crate::policies::get(&*txn, &p.table, &p.name)?.is_some() {
                return Err(Error::duplicate_object(format!(
                    "policy \"{}\" for table \"{}\" already exists",
                    p.name, p.table
                )));
            }
            for r in &p.roles {
                if r != crate::privileges::PUBLIC && crate::roles::get(&*txn, r)?.is_none() {
                    return Err(Error::undefined_object(format!(
                        "role \"{r}\" does not exist"
                    )));
                }
            }
            use crate::policies::Command;
            if p.check.is_some() && matches!(p.command, Command::Select | Command::Delete) {
                return Err(Error::syntax(
                    "only WITH CHECK expression allowed for INSERT; SELECT and DELETE take USING",
                ));
            }
            if p.using.is_some() && p.command == Command::Insert {
                return Err(Error::syntax(
                    "only WITH CHECK expression allowed for INSERT",
                ));
            }
            for text in p.using.iter().chain(&p.check) {
                Binder::new(&*txn, reg).condition(&parse_expr(text)?, &dml::table_scope(&t))?;
            }
            crate::policies::put(txn, p)?;
            Output::Command("CREATE POLICY".into())
        }
        Statement::DropPolicy {
            name,
            table,
            if_exists,
        } => {
            catalog::table(&*txn, table)?;
            if crate::policies::get(&*txn, table, name)?.is_none() {
                if !*if_exists {
                    return Err(Error::undefined_object(format!(
                        "policy \"{name}\" for table \"{table}\" does not exist"
                    )));
                }
            } else {
                crate::policies::delete(txn, table, name)?;
            }
            Output::Command("DROP POLICY".into())
        }
        Statement::Grant {
            items,
            target,
            grantees,
            grant_option,
            revoke,
        } => {
            use crate::privileges::Kind;
            for o in &target.objects {
                let known = match target.kind {
                    Kind::Function => reg.function(o).is_some() || reg.aggregate(o).is_some(),
                    Kind::Type => {
                        reg.type_named(o).is_some() || crate::enums::get(&*txn, o)?.is_some()
                    }
                    _ => true,
                };
                if !known {
                    return Err(match target.kind {
                        Kind::Function => {
                            Error::undefined_function(format!("function {o} does not exist"))
                        }
                        _ => Error::undefined_object(format!("type \"{o}\" does not exist")),
                    });
                }
            }
            crate::privileges::grant(txn, items, target, grantees, *grant_option, *revoke)?;
            Output::Command(if *revoke { "REVOKE" } else { "GRANT" }.into())
        }
        Statement::AlterDefaultPrivileges {
            for_role,
            privileges,
            grantees,
            grant_option,
            revoke,
        } => {
            crate::privileges::alter_defaults(
                txn,
                for_role.as_deref(),
                *privileges,
                grantees,
                *grant_option,
                *revoke,
            )?;
            Output::Command("ALTER DEFAULT PRIVILEGES".into())
        }
        Statement::CreateEnum { name, labels } => {
            crate::enums::create(txn, reg, name, labels)?;
            Output::Command("CREATE TYPE".into())
        }
        Statement::DropType { name, if_exists } => {
            crate::enums::drop(txn, name, *if_exists)?;
            Output::Command("DROP TYPE".into())
        }
        Statement::DropIndex { name, if_exists } => {
            match catalog::find_index(&*txn, name)? {
                Some((mut t, i)) => {
                    if let Some((child, fk)) = crate::fk::index_users(&*txn, &t, i)?.first() {
                        return Err(Error::dependent_objects(format!(
                            "cannot drop index {name} because constraint {fk} on table {child} requires it"
                        )));
                    }
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
                ty: crate::binder::column_type(&*txn, reg, &column.ty)?,
                nullable: !column.not_null,
                default: column.default.clone(),
                max_len: column.max_len,
                numeric: column.numeric,
            });
            if let Some(check) = &column.check {
                t.checks
                    .push((format!("{table}_{}_check", column.name), check.clone()));
            }
            catalog::note_stored_types(txn, &t.columns)?;
            let w = dml::Writer::new(&*txn, reg, t.clone())?;
            let fill = {
                let ctx = Ctx::new(&*txn, reg, &[]);
                w.default_of(t.columns.len() - 1, &ctx)?
            };
            let has_rows = !exec::matching_rows(
                &t,
                &crate::plan::Access::Full,
                None,
                &Ctx::new(&*txn, reg, &[]),
            )?
            .is_empty();
            if has_rows {
                if fill.is_null() && column.not_null {
                    return Err(Error::not_null_violation(format!(
                        "column \"{}\" of relation \"{table}\" contains null values",
                        column.name
                    )));
                }
                if !fill.is_null() {
                    dml::rewrite_rows(txn, reg, &t, &fill)?;
                }
            }
            if let Some((name, r)) = &column.references {
                let fk = crate::fk::build(
                    &*txn,
                    &t,
                    name.as_deref(),
                    std::slice::from_ref(&column.name),
                    r,
                )?;
                crate::fk::validate_existing(&*txn, reg, &t, &fk)?;
                crate::fk::attach(txn, &mut t, fk)?;
            }
            catalog::update_table(txn, &t)?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::AddConstraint { table, constraint } => {
            add_constraint(txn, reg, table, constraint)?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::DropConstraint {
            table,
            name,
            if_exists,
        } => {
            drop_constraint(txn, table, name, *if_exists)?;
            Output::Command("ALTER TABLE".into())
        }
        Statement::Insert {
            table,
            columns,
            source,
            returning,
        } => {
            let w = dml::insert(txn, reg, table, columns, source, returning, params)?;
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
            dml::update(
                txn,
                reg,
                table,
                assignments,
                filter.as_ref(),
                returning,
                params,
            )?,
        ),
        Statement::Delete {
            table,
            filter,
            returning,
        } => written(
            "DELETE",
            dml::delete(txn, reg, table, filter.as_ref(), returning, params)?,
        ),
        Statement::Analyze(table) => {
            crate::stats::analyze(txn, reg, table.as_deref())?;
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
    reg: &Registry,
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
            ty: crate::binder::column_type(&*txn, reg, &s.ty)?,
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
            TableConstraint::ForeignKey { .. } => {} // after the table is built
            TableConstraint::Check(n, text) => {
                checks.push((
                    n.clone()
                        .unwrap_or_else(|| format!("{name}_check{}", checks.len() + 1)),
                    text.clone(),
                ));
            }
        }
    }
    require_keys(&columns, &pk, true, "a primary key")?;
    for (_, cols) in &uniques {
        require_keys(&columns, cols, true, "UNIQUE")?;
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
        foreign_keys: Vec::new(),
        referenced_by: Vec::new(),
        owner: Some(crate::access::current().user.clone()),
        row_security: false,
        force_row_security: false,
    };
    for (n, cols) in uniques {
        if catalog::find_index(&*txn, &n)?.is_some() {
            return Err(Error::duplicate_table(format!(
                "relation \"{n}\" already exists"
            )));
        }
        table
            .indexes
            .push(Index::plain(table.next_index_id, n, cols, true));
        table.next_index_id += 1;
    }
    // Defaults and checks must bind now, not at the first insert.
    let w = dml::Writer::new(&*txn, reg, table.clone())?;
    let ctx = Ctx::new(&*txn, reg, &[]);
    for (i, c) in table.columns.iter().enumerate() {
        if c.default.is_some() {
            w.default_of(i, &ctx)?;
        }
    }
    for (_, text) in &table.checks {
        let _ = Binder::new(&*txn, reg).condition(&parse_expr(text)?, &dml::table_scope(&table))?;
    }
    let _ = Scope::default();
    // Foreign keys: from columns, then table constraints.
    let mut fks = Vec::new();
    for s in specs {
        if let Some((n, r)) = &s.references {
            fks.push((n.clone(), vec![s.name.clone()], r.clone()));
        }
    }
    for c in constraints {
        if let TableConstraint::ForeignKey {
            name,
            columns,
            references,
        } = c
        {
            fks.push((name.clone(), columns.clone(), references.clone()));
        }
    }
    let mut built = Vec::new();
    for (n, cols, r) in &fks {
        let fk = crate::fk::build(&*txn, &table, n.as_deref(), cols, r)?;
        // Later names must not collide with this one.
        table.foreign_keys.push(fk.clone());
        built.push(fk);
    }
    table.foreign_keys.clear();
    // Self-references are recorded on the new table, others on the
    // referenced tables (which exist).
    catalog::note_stored_types(txn, &table.columns)?;
    let mut table = catalog::create_table(txn, table)?;
    for fk in built {
        crate::fk::attach(txn, &mut table, fk)?;
    }
    catalog::update_table(txn, &table)?;
    Ok(())
}

/// `ALTER TABLE … ADD CONSTRAINT`.
/// The indexes that use ordering `name` of type `type_name`: (table,
/// index position).
fn ordering_users(txn: &dyn KvTxn, type_name: &str, name: &str) -> Result<Vec<(Table, usize)>> {
    let mut out = Vec::new();
    for t in catalog::tables(txn)? {
        for (i, ix) in t.indexes.iter().enumerate() {
            let uses = ix.columns.iter().zip(&ix.orderings).any(|(&c, o)| {
                o.as_deref() == Some(name)
                    && crate::builtin::type_of(t.columns[c].ty).name() == type_name
            });
            if uses {
                out.push((t.clone(), i));
            }
        }
    }
    Ok(out)
}

/// Rebuilds index `i` of `t`: its entries again, from the rows.
fn rebuild_index(txn: &mut dyn KvTxn, reg: &Registry, t: &Table, i: usize) -> Result<()> {
    let prefix = t.index_prefix(t.indexes[i].id);
    catalog::delete_range(txn, &prefix, nucleus_kv::prefix_end(&prefix).as_deref())?;
    dml::backfill_index(txn, reg, t, i)
}

/// `ALTER TYPE`: teaching a type an ordering, redefining one (rebuilding
/// the indexes that use it), or dropping one (refused while used, unless
/// `CASCADE`, which drops those indexes).
fn alter_type(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    type_name: &str,
    action: &crate::ast::TypeAction,
) -> Result<()> {
    use crate::catalog::TaughtOrdering;
    match action {
        crate::ast::TypeAction::AddValue {
            label,
            if_not_exists,
            place,
        } => return crate::enums::add_value(txn, type_name, label, *if_not_exists, place),
        crate::ast::TypeAction::RenameValue { from, to } => {
            return crate::enums::rename_value(txn, type_name, from, to);
        }
        _ => {}
    }
    let ty = reg
        .type_named(type_name)
        .ok_or_else(|| Error::undefined_object(format!("type \"{type_name}\" does not exist")))?;
    let tname = crate::builtin::type_of(ty).name().to_string();
    let builtin_name = |n: &str| {
        ty == DataType::Text && crate::binder::BYTE_ORDER.contains(&n.to_lowercase().as_str())
    };
    match action {
        crate::ast::TypeAction::AddOrdering { name, mapping } => {
            if builtin_name(name) || catalog::get_ordering(&*txn, &tname, name)?.is_some() {
                return Err(Error::duplicate_object(format!(
                    "type {tname} already has an ordering {name}"
                )));
            }
            Binder::new(&*txn, reg).bind_mapping(ty, mapping)?;
            catalog::put_ordering(
                txn,
                &TaughtOrdering {
                    type_name: tname,
                    name: name.clone(),
                    mapping: mapping.clone(),
                },
            )?;
        }
        crate::ast::TypeAction::AlterOrdering { name, mapping } => {
            if catalog::get_ordering(&*txn, &tname, name)?.is_none() {
                return Err(Error::undefined_object(format!(
                    "type {tname} has no ordering {name}"
                )));
            }
            Binder::new(&*txn, reg).bind_mapping(ty, mapping)?;
            catalog::put_ordering(
                txn,
                &TaughtOrdering {
                    type_name: tname.clone(),
                    name: name.clone(),
                    mapping: mapping.clone(),
                },
            )?;
            for (t, i) in ordering_users(&*txn, &tname, name)? {
                rebuild_index(txn, reg, &t, i)?;
            }
        }
        crate::ast::TypeAction::DropOrdering {
            name,
            if_exists,
            cascade,
        } => {
            if catalog::get_ordering(&*txn, &tname, name)?.is_none() {
                if *if_exists {
                    return Ok(());
                }
                return Err(Error::undefined_object(format!(
                    "type {tname} has no ordering {name}"
                )));
            }
            let users = ordering_users(&*txn, &tname, name)?;
            if let Some((t, i)) = users.first()
                && !*cascade
            {
                return Err(Error::dependent_objects(format!(
                    "cannot drop ordering {name} of type {tname} because index {} on table {} uses it (DROP … CASCADE drops it too)",
                    t.indexes[*i].name, t.name
                )));
            }
            for (t, i) in users {
                let id = t.indexes[i].id;
                // Read afresh: an earlier drop may have changed the table.
                let mut t = catalog::table(&*txn, &t.name)?;
                let prefix = t.index_prefix(id);
                catalog::delete_range(txn, &prefix, nucleus_kv::prefix_end(&prefix).as_deref())?;
                t.indexes.retain(|ix| ix.id != id);
                catalog::update_table(txn, &t)?;
            }
            catalog::delete_ordering(txn, &tname, name)?;
        }
        crate::ast::TypeAction::AddOperator {
            symbol,
            left,
            right,
            function,
        } => {
            let (l, r) = (
                crate::binder::column_type(&*txn, reg, left)?,
                crate::binder::column_type(&*txn, reg, right)?,
            );
            if l != ty && r != ty {
                return Err(Error::invalid_parameter(format!(
                    "an operator taught to {tname} takes a {tname}"
                )));
            }
            let (ln, rn) = (type_name_of(l), type_name_of(r));
            let taken = reg.operator(symbol, l, r).is_some()
                || (l == r
                    && crate::builtin::type_of(l)
                        .distance()
                        .is_some_and(|d| d.operator() == symbol))
                || catalog::get_operator(&*txn, symbol, &ln, &rn)?.is_some();
            if taken {
                return Err(Error::duplicate_object(format!(
                    "operator {symbol} ({ln}, {rn}) already exists"
                )));
            }
            check_function(reg, function, &[l, r])?;
            catalog::put_operator(
                txn,
                &catalog::TaughtOperator {
                    symbol: symbol.clone(),
                    left: ln,
                    right: rn,
                    function: function.clone(),
                },
            )?;
        }
        crate::ast::TypeAction::DropOperator {
            symbol,
            left,
            right,
            if_exists,
        } => {
            let (ln, rn) = (
                type_name_of(crate::binder::column_type(&*txn, reg, left)?),
                type_name_of(crate::binder::column_type(&*txn, reg, right)?),
            );
            if catalog::get_operator(&*txn, symbol, &ln, &rn)?.is_none() {
                if *if_exists {
                    return Ok(());
                }
                return Err(Error::undefined_function(format!(
                    "operator {symbol} ({ln}, {rn}) was not taught"
                )));
            }
            catalog::delete_operator(txn, symbol, &ln, &rn)?;
        }
        crate::ast::TypeAction::AddCast {
            to,
            function,
            context,
        } => {
            let target = crate::binder::column_type(&*txn, reg, to)?;
            let tn = type_name_of(target);
            if target == ty
                || !(matches!(ty, DataType::Ext(_)) || matches!(target, DataType::Ext(_)))
            {
                return Err(Error::invalid_parameter(format!(
                    "casts between built-in types are fixed; {tname} to {tn} cannot be taught"
                )));
            }
            if catalog::get_cast(&*txn, &tname, &tn)?.is_some() {
                return Err(Error::duplicate_object(format!(
                    "cast from {tname} to {tn} already exists"
                )));
            }
            check_function(reg, function, &[ty])?;
            catalog::put_cast(
                txn,
                &catalog::TaughtCast {
                    from: tname,
                    to: tn,
                    function: function.clone(),
                    context: match context {
                        2 => catalog::CastContext::Implicit,
                        1 => catalog::CastContext::Assignment,
                        _ => catalog::CastContext::Explicit,
                    },
                },
            )?;
        }
        crate::ast::TypeAction::AddValue { .. } | crate::ast::TypeAction::RenameValue { .. } => {
            unreachable!("an enum's, handled above")
        }
        crate::ast::TypeAction::Upgrade => {
            let DataType::Ext(t) = ty else {
                return Err(Error::invalid_parameter(format!(
                    "{tname} is built in: its encoding is the build's"
                )));
            };
            catalog::upgrade_type(txn, reg, t)?;
        }
        crate::ast::TypeAction::DropCast { to, if_exists } => {
            let tn = type_name_of(crate::binder::column_type(&*txn, reg, to)?);
            if catalog::get_cast(&*txn, &tname, &tn)?.is_none() {
                if *if_exists {
                    return Ok(());
                }
                return Err(Error::undefined_object(format!(
                    "cast from {tname} to {tn} was not taught"
                )));
            }
            catalog::delete_cast(txn, &tname, &tn)?;
        }
    }
    Ok(())
}

/// `ALTER TABLE t ALTER COLUMN c TYPE ty [USING expr]`: every value
/// converted — by `expr` (over the row), else by a taught assignment or
/// implicit cast, else a built-in assignment cast — and every row, key and
/// index entry rewritten, the table's checks and keys holding afterwards.
/// One transaction: readers see the old column until it commits.
#[allow(clippy::too_many_arguments)]
fn alter_column_type(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    column: &str,
    ty: &crate::ast::SqlType,
    typmod: &crate::ast::TypeMod,
    using: Option<&crate::ast::Expr>,
) -> Result<()> {
    let old = catalog::table(&*txn, table)?;
    let c = old.column(column).ok_or_else(|| {
        Error::undefined_column(format!(
            "column \"{column}\" of relation \"{table}\" does not exist"
        ))
    })?;
    let in_fk = old.foreign_keys.iter().any(|fk| fk.columns.contains(&c))
        || old.referenced_by.iter().any(|(child, name)| {
            catalog::table(&*txn, child).is_ok_and(|ct| {
                ct.foreign_keys
                    .iter()
                    .any(|fk| &fk.name == name && fk.ref_columns.contains(&c))
            })
        });
    if in_fk {
        return Err(Error::feature(format!(
            "changing the type of column \"{column}\", which a foreign key uses, is not supported yet"
        )));
    }
    let from = old.columns[c].ty;
    let to = crate::binder::column_type(&*txn, reg, ty)?;
    let conversion = {
        let mut binder = Binder::new(&*txn, reg);
        let scope = dml::table_scope(&old);
        match using {
            Some(e) => binder.expr(e, &scope)?.0,
            None => {
                let column_expr = crate::expr::Expr::Column(c);
                let name = |t: DataType| crate::builtin::type_of(t).name().to_string();
                let taught = catalog::get_cast(&*txn, &name(from), &name(to))?
                    .filter(|k| k.context != catalog::CastContext::Explicit);
                if let Some(k) = taught {
                    let f = reg.function(&k.function).cloned().ok_or_else(|| {
                        Error::undefined_function(format!("function {} does not exist", k.function))
                    })?;
                    crate::expr::Expr::Call(f, vec![column_expr])
                } else if from == to {
                    column_expr
                } else if to == DataType::Text
                    || (from.is_number() && to.is_number())
                    || (from.is_datetime() && to.is_datetime())
                {
                    crate::expr::Expr::Cast(Box::new(column_expr), to)
                } else {
                    return Err(Error::type_mismatch(format!(
                        "column \"{column}\" cannot be cast automatically to type {}; give USING",
                        name(to)
                    )));
                }
            }
        }
    };
    let mut new = old.clone();
    new.columns[c].ty = to;
    new.columns[c].max_len = typmod.max_len;
    new.columns[c].numeric = typmod.numeric;
    if new.primary_key.contains(&c) {
        require_keys(&new.columns, &[c], true, "a primary key")?;
    }
    for ix in &new.indexes {
        if ix.columns.contains(&c) && !ix.mapped() {
            require_keys(&new.columns, &[c], ix.unique, "an index")?;
        }
    }
    // Convert every row (against the old definition), then check it
    // against the new one.
    let rows = {
        let ctx = Ctx::new(&*txn, reg, &[]);
        let mut rows = Vec::new();
        for (key, mut row) in exec::matching_rows(&old, &crate::plan::Access::Full, None, &ctx)? {
            let v = conversion.eval(&crate::expr::Env {
                row: &row,
                outer: None,
                ctx: &ctx,
            })?;
            row[c] = dml::assign(ctx.txn, v, &new, c)?;
            rows.push((key, row));
        }
        rows
    };
    let w = dml::Writer::new(&*txn, reg, new.clone())?;
    {
        let ctx = Ctx::new(&*txn, reg, &[]);
        for (_, row) in &rows {
            w.validate(row, &ctx)?;
        }
        if new.columns[c].default.is_some() {
            w.default_of(c, &ctx)?;
        }
    }
    dml::replace_rows(txn, reg, &new, rows)?;
    txn.delete(&catalog::stats_key(&new))?; // the old type's statistics
    catalog::note_stored_types(txn, &new.columns)?;
    catalog::update_table(txn, &new)
}

fn type_name_of(t: DataType) -> String {
    crate::builtin::type_of(t).name().to_string()
}

/// Refuses a function the registry lacks, or that does not take
/// arguments of these types.
fn check_function(reg: &Registry, name: &str, args: &[DataType]) -> Result<()> {
    let f = reg
        .function(name)
        .ok_or_else(|| Error::undefined_function(format!("function {name} does not exist")))?;
    if !f.arity().contains(&args.len()) {
        return Err(Error::undefined_function(format!(
            "function {name} does not take {} arguments",
            args.len()
        )));
    }
    let info: Vec<crate::ext::ArgInfo> = args
        .iter()
        .map(|t| crate::ext::ArgInfo {
            ty: Some(*t),
            text_literal: false,
        })
        .collect();
    f.resolve(&info).map(|_| ())
}

/// Refuses key columns whose types have no key encoding (and, for a
/// primary key or `UNIQUE`, no equality).
fn require_keys(columns: &[Column], cols: &[usize], unique: bool, what: &str) -> Result<()> {
    use crate::binder::{Need, require};
    let needs: &[Need] = if unique {
        &[Need::KeyEncoding, Need::Equality]
    } else {
        &[Need::KeyEncoding]
    };
    for &c in cols {
        require(Some(columns[c].ty), needs, what)?;
    }
    Ok(())
}

fn add_constraint(
    txn: &mut dyn KvTxn,
    reg: &Registry,
    table: &str,
    c: &TableConstraint,
) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    match c {
        TableConstraint::ForeignKey {
            name,
            columns,
            references,
        } => {
            let fk = crate::fk::build(&*txn, &t, name.as_deref(), columns, references)?;
            crate::fk::validate_existing(&*txn, reg, &t, &fk)?;
            crate::fk::attach(txn, &mut t, fk)?;
        }
        TableConstraint::Check(name, text) => {
            let name = name
                .clone()
                .unwrap_or_else(|| format!("{table}_check{}", t.checks.len() + 1));
            if t.checks.iter().any(|(n, _)| *n == name) {
                return Err(Error::duplicate_object(format!(
                    "constraint \"{name}\" for relation \"{table}\" already exists"
                )));
            }
            let check =
                Binder::new(&*txn, reg).condition(&parse_expr(text)?, &dml::table_scope(&t))?;
            let ctx = Ctx::new(&*txn, reg, &[]);
            for (_, row) in exec::matching_rows(&t, &crate::plan::Access::Full, None, &ctx)? {
                let env = crate::expr::Env {
                    row: &row,
                    outer: None,
                    ctx: &ctx,
                };
                if check.eval(&env)?.truth()? == Some(false) {
                    return Err(Error::check_violation(format!(
                        "check constraint \"{name}\" of relation \"{table}\" is violated by some row"
                    )));
                }
            }
            t.checks.push((name, text.clone()));
        }
        TableConstraint::Unique(name, cols) => {
            let name = name
                .clone()
                .unwrap_or_else(|| format!("{table}_{}_key", cols.join("_")));
            if catalog::find_index(&*txn, &name)?.is_some() {
                return Err(Error::duplicate_table(format!(
                    "relation \"{name}\" already exists"
                )));
            }
            let columns = cols
                .iter()
                .map(|c| {
                    t.column(c).ok_or_else(|| {
                        Error::undefined_column(format!(
                            "column \"{c}\" named in key does not exist"
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            require_keys(&t.columns, &columns, true, "UNIQUE")?;
            t.indexes
                .push(Index::plain(t.next_index_id, name, columns, true));
            t.next_index_id += 1;
            dml::backfill_index(txn, reg, &t, t.indexes.len() - 1)?;
        }
        TableConstraint::PrimaryKey(_) => {
            return Err(Error::feature(
                "ALTER TABLE … ADD PRIMARY KEY is not supported",
            ));
        }
    }
    catalog::update_table(txn, &t)
}

/// `ALTER TABLE … DROP CONSTRAINT`.
fn drop_constraint(txn: &mut dyn KvTxn, table: &str, name: &str, if_exists: bool) -> Result<()> {
    let mut t = catalog::table(&*txn, table)?;
    if let Some(i) = t.foreign_keys.iter().position(|f| f.name == name) {
        crate::fk::detach(txn, &mut t, i)?;
    } else if let Some(i) = t.checks.iter().position(|(n, _)| n == name) {
        t.checks.remove(i);
    } else if let Some(i) = t.indexes.iter().position(|ix| ix.name == name && ix.unique) {
        if let Some((child, fk)) = crate::fk::index_users(&*txn, &t, i)?.first() {
            return Err(Error::dependent_objects(format!(
                "cannot drop constraint {name} on table {table} because constraint {fk} on table {child} requires it"
            )));
        }
        let prefix = t.index_prefix(t.indexes[i].id);
        catalog::delete_range(txn, &prefix, nucleus_kv::prefix_end(&prefix).as_deref())?;
        t.indexes.remove(i);
    } else if name == format!("{table}_pkey") {
        return Err(Error::feature("dropping a primary key is not supported"));
    } else if if_exists {
        return Ok(());
    } else {
        return Err(Error::undefined_object(format!(
            "constraint \"{name}\" of relation \"{table}\" does not exist"
        )));
    }
    catalog::update_table(txn, &t)
}
