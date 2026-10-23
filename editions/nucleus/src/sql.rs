//! `nucleus sql` / `ncsql`: the nucleus SQL shell (PLAN.md §M14.6) — over a
//! local database directory, or connected to a nucleus server.
//!
//! Its backslash commands read `nc_catalog`, nucleus's own catalog, so
//! they work the same locally and over the network (and show what psql's
//! `\d` cannot: table ids, key prefixes, statistics).

use std::io::{BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::Instant;

use nucleus_client::{Client, Config, Format, TxnStatus as RemoteStatus, Value};
use nucleus_kv::{Database, Options};
use nucleus_sql::{Output, Session, TxnStatus};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: ncsql [<dir>] [options]          (also: nucleus sql …)
  <dir>                     open a local database directory
  -h, --host <host[:port]>  connect to a nucleus server (default 127.0.0.1:20035)
  -p, --port <port>
  -U, --user <name>         (default nucleus)
  -d, --database <name>     (default nucleus)
      --password <pw>       or NCPASSWORD (default nucleus)
      --tls                 TLS, the server's certificate checked (Mozilla's roots)
      --tls-ca <pem file>   TLS, the certificate checked against this CA
      --tls-no-verify       TLS without checking the certificate (encrypted only)
      --tls-cert <pem file> --tls-key <pem file>
                            show the server a client certificate (mutual TLS)
      --token <jwt>         log in with an identity token (or NCTOKEN)
  -c <sql>                  run one command and exit
Without -c it reads statements from stdin (a shell on a terminal).
";

const HELP: &str = "\
Statements end with ;.  Commands (they read nc_catalog):
  \\d                 tables (also \\dt, \\tables)
  \\d <table>         a table: columns, keys, indexes, foreign keys, checks
  \\indexes [<table>] indexes (also \\di)
  \\fks [<table>]     foreign keys, from or to the table
  \\stats <table>     statistics gathered by ANALYZE
  \\lint <statement>  what in a statement is likely slow or wrong (ANALYZE QUERY)
  \\statements        the statements that took the most time (nc_catalog.statements)
  \\schemas           schemas (also \\dn)
  \\types             types (also \\dT)
  \\timing            show how long each command takes (toggle)
  \\conninfo          what this shell is connected to
  \\?                 this help
  \\q                 quit
";

/// A result table: column names and text cells (`None` = NULL).
struct Table {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
}

enum Res {
    Rows(Table),
    Command(String),
}

/// A failed statement.
struct Failure {
    code: String,
    message: String,
    hint: Option<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ERROR:  {} ({})", self.message, self.code)?;
        if let Some(h) = &self.hint {
            write!(f, "\nHINT:  {h}")?;
        }
        Ok(())
    }
}

/// Where statements run.
enum Backend {
    Local {
        session: Session,
        db: Database,
        dir: String,
    },
    Remote(Client),
}

impl Backend {
    /// Runs `sql`; stops at the first failing statement.  `Err` only for a
    /// lost connection.
    fn execute(&mut self, sql: &str) -> Result<Vec<std::result::Result<Res, Failure>>> {
        match self {
            Backend::Local { session, .. } => {
                let stmts = match nucleus_sql::parser::parse_with_text(sql) {
                    Ok(s) => s,
                    Err(e) => {
                        return Ok(vec![Err(Failure {
                            code: e.code.into(),
                            message: e.message,
                            hint: e.hint,
                        })]);
                    }
                };
                let mut out = Vec::new();
                for (stmt, text) in stmts {
                    session.note_text(&text);
                    match session.run(&stmt, &[]) {
                        Ok(Output::Command(tag)) => out.push(Ok(Res::Command(tag))),
                        Ok(Output::Rows { columns, rows }) => out.push(Ok(Res::Rows(Table {
                            columns: columns.into_iter().map(|c| c.name).collect(),
                            rows: rows
                                .into_iter()
                                .map(|r| {
                                    r.into_iter()
                                        .map(|v| (!v.is_null()).then(|| v.render()))
                                        .collect()
                                })
                                .collect(),
                        }))),
                        Err(e) => {
                            out.push(Err(Failure {
                                code: e.code.into(),
                                message: e.message,
                                hint: e.hint,
                            }));
                            break;
                        }
                    }
                }
                Ok(out)
            }
            Backend::Remote(c) => Ok(c
                .query(sql, &[], Format::Text)
                .map_err(|e| format!("connection lost: {e}"))?
                .into_iter()
                .map(|r| match r {
                    Ok(r) if r.has_rows => Ok(Res::Rows(Table {
                        columns: r.columns.into_iter().map(|c| c.name).collect(),
                        rows: r
                            .rows
                            .into_iter()
                            .map(|row| {
                                row.into_iter()
                                    .map(|v| match v {
                                        Value::Null => None,
                                        Value::Text(s) => Some(s),
                                        v => Some(v.to_string()),
                                    })
                                    .collect()
                            })
                            .collect(),
                    })),
                    // nucleus's command tags: the command, and the row
                    // count where it means something.
                    Ok(r) => Ok(Res::Command(match r.command.as_str() {
                        "INSERT" | "UPDATE" | "DELETE" => format!("{} {}", r.command, r.count),
                        _ => r.command,
                    })),
                    Err(e) => Err(Failure {
                        code: e.code,
                        message: e.message,
                        hint: e.hint,
                    }),
                })
                .collect()),
        }
    }

    /// The prompt's transaction mark: none, `*` in a block, `!` failed.
    fn mark(&self) -> &'static str {
        let (block, failed) = match self {
            Backend::Local { session, .. } => (
                session.status() == TxnStatus::InBlock,
                session.status() == TxnStatus::Failed,
            ),
            Backend::Remote(c) => (
                c.status() == RemoteStatus::InBlock,
                c.status() == RemoteStatus::Failed,
            ),
        };
        if failed {
            "!"
        } else if block {
            "*"
        } else {
            ""
        }
    }

    fn describe(&self) -> String {
        match self {
            Backend::Local { dir, .. } => format!("local database {dir}"),
            Backend::Remote(c) => {
                let cfg = c.config();
                format!(
                    "database \"{}\" as user \"{}\" on {}:{} ({}, session {})",
                    cfg.database,
                    cfg.user,
                    cfg.host,
                    cfg.port,
                    c.server_version(),
                    c.session_id()
                )
            }
        }
    }

    fn close(self) -> Result<()> {
        match self {
            Backend::Local { session, db, .. } => {
                drop(session);
                db.close().map_err(|e| e.to_string())
            }
            Backend::Remote(c) => c.close().map_err(|e| e.to_string()),
        }
    }
}

/// Prints a table in psql's aligned style.
fn print_table(out: &mut dyn Write, t: &Table, title: Option<&str>) -> std::io::Result<()> {
    let cells: Vec<Vec<&str>> = t
        .rows
        .iter()
        .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("")).collect())
        .collect();
    let mut widths: Vec<usize> = t.columns.iter().map(|c| c.chars().count()).collect();
    for r in &cells {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |vals: &[&str]| -> String {
        vals.iter()
            .zip(&widths)
            .map(|(v, w)| format!(" {v}{} ", " ".repeat(w - v.chars().count())))
            .collect::<Vec<_>>()
            .join("|")
    };
    if let Some(title) = title {
        writeln!(out, "{title}")?;
    }
    let names: Vec<&str> = t.columns.iter().map(String::as_str).collect();
    writeln!(out, "{}", line(&names))?;
    writeln!(
        out,
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    )?;
    for r in &cells {
        writeln!(out, "{}", line(r))?;
    }
    writeln!(
        out,
        "({} row{})",
        t.rows.len(),
        if t.rows.len() == 1 { "" } else { "s" }
    )
}

/// A SQL string literal.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

struct Shell {
    backend: Backend,
    timing: bool,
}

impl Shell {
    /// Runs SQL text and prints the results; whether everything succeeded.
    fn run_sql(&mut self, sql: &str, out: &mut dyn Write) -> Result<bool> {
        let started = Instant::now();
        let results = self.backend.execute(sql)?;
        let mut ok = true;
        for r in results {
            match r {
                Ok(Res::Command(tag)) => writeln!(out, "{tag}"),
                Ok(Res::Rows(t)) => print_table(out, &t, None),
                Err(f) => {
                    ok = false;
                    writeln!(out, "{f}")
                }
            }
            .map_err(|e| e.to_string())?;
        }
        if self.timing {
            writeln!(
                out,
                "Time: {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(ok)
    }

    /// Runs a catalog query for a command; `None` if it failed (printed).
    fn fetch(&mut self, sql: &str, out: &mut dyn Write) -> Result<Option<Table>> {
        for r in self.backend.execute(sql)? {
            match r {
                Ok(Res::Rows(t)) => return Ok(Some(t)),
                Ok(Res::Command(_)) => {}
                Err(f) => {
                    writeln!(out, "{f}").map_err(|e| e.to_string())?;
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    fn show(&mut self, sql: &str, title: Option<&str>, out: &mut dyn Write) -> Result<bool> {
        Ok(match self.fetch(sql, out)? {
            Some(t) => {
                print_table(out, &t, title).map_err(|e| e.to_string())?;
                true
            }
            None => false,
        })
    }

    /// A backslash command; whether it succeeded (`None`: quit).
    fn command(&mut self, line: &str, out: &mut dyn Write) -> Result<Option<bool>> {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let arg = words.next().map(|a| a.trim_matches('"').to_string());
        let filter = |col: &str| {
            arg.as_ref()
                .map_or(String::new(), |t| format!(" WHERE {col} = {}", quote(t)))
        };
        let ok = match (cmd, &arg) {
            ("\\q", _) => return Ok(None),
            ("\\?", _) => {
                write!(out, "{HELP}").map_err(|e| e.to_string())?;
                true
            }
            ("\\d" | "\\dt" | "\\tables", None) => self.show(
                "SELECT name AS \"table\", columns, indexes, foreign_keys AS \"foreign keys\", \
                 referenced_by AS \"referenced by\", analyzed_rows AS \"analyzed rows\" \
                 FROM nc_catalog.tables ORDER BY name",
                Some("Tables"),
                out,
            )?,
            ("\\d", Some(t)) => self.describe_table(t, out)?,
            ("\\indexes" | "\\di", _) => self.show(
                &format!(
                    "SELECT table_name AS \"table\", name AS index, index_id AS id, columns, \
                     is_unique AS \"unique\", is_primary AS \"primary\" FROM nc_catalog.indexes{} \
                     ORDER BY table_name, index_id",
                    filter("table_name")
                ),
                Some("Indexes"),
                out,
            )?,
            ("\\fks", _) => self.show(
                &format!(
                    "SELECT table_name AS \"table\", name, columns, referenced_table AS \"references\", \
                     referenced_columns AS \"referenced columns\", on_delete AS \"on delete\", \
                     on_update AS \"on update\" FROM nc_catalog.foreign_keys{} ORDER BY table_name, name",
                    arg.as_ref().map_or(String::new(), |t| format!(
                        " WHERE table_name = {q} OR referenced_table = {q}",
                        q = quote(t)
                    ))
                ),
                Some("Foreign keys"),
                out,
            )?,
            ("\\stats", Some(t)) => self.show(
                &format!(
                    "SELECT column_name AS \"column\", rows, nulls, distinct_values AS \"distinct\", \
                     histogram FROM nc_catalog.statistics WHERE table_name = {} ORDER BY column_name",
                    quote(t)
                ),
                Some(&format!("Statistics of \"{t}\" (from ANALYZE)")),
                out,
            )?,
            ("\\schemas" | "\\dn", _) => {
                self.show("SELECT name, description FROM nc_catalog.schemas ORDER BY name", Some("Schemas"), out)?
            }
            ("\\types" | "\\dT", _) => self.show(
                "SELECT name, category, aliases FROM nc_catalog.types",
                Some("Types"),
                out,
            )?,
            ("\\timing", _) => {
                self.timing = !self.timing;
                writeln!(out, "Timing is {}.", if self.timing { "on" } else { "off" })
                    .map_err(|e| e.to_string())?;
                true
            }
            ("\\conninfo", _) => {
                writeln!(out, "Connected to {}.", self.backend.describe()).map_err(|e| e.to_string())?;
                if matches!(self.backend, Backend::Remote(_))
                    && let Some(t) = self.fetch(
                        "SELECT current_user, coalesce(current_setting('nucleus.client_addr', true), '?'), \
                         current_setting('nucleus.auth_method'), current_setting('nucleus.tls'), \
                         coalesce(current_setting('nucleus.access_rule', true), '(no rules)')",
                        out,
                    )?
                    && let Some(r) = t.rows.first()
                {
                    let cell = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                    writeln!(
                        out,
                        "Acting as \"{}\", seen from {} — authenticated by {}, TLS {}, admitted by {}.",
                        cell(0),
                        cell(1),
                        cell(2),
                        cell(3),
                        cell(4)
                    )
                    .map_err(|e| e.to_string())?;
                }
                true
            }
            ("\\lint", Some(_)) => {
                let statement = line.trim_start()[cmd.len()..].trim().trim_end_matches(';');
                match self.fetch(&format!("ANALYZE QUERY {statement}"), out)? {
                    Some(t) if t.rows.is_empty() => {
                        writeln!(out, "No findings.").map_err(|e| e.to_string())?;
                        true
                    }
                    Some(t) => {
                        print_table(out, &t, Some("Findings")).map_err(|e| e.to_string())?;
                        true
                    }
                    None => false,
                }
            }
            ("\\lint", None) => {
                writeln!(out, "\\lint needs a statement").map_err(|e| e.to_string())?;
                false
            }
            ("\\statements", _) => self.show(
                "SELECT query, calls, errors, round(total_ms::numeric, 3) AS \"total ms\", \
                 round(mean_ms::numeric, 3) AS \"mean ms\", rows_read AS \"rows read\", \
                 rows_returned AS \"rows returned\" FROM nc_catalog.statements \
                 ORDER BY total_ms DESC LIMIT 20",
                Some("Statements (most time first)"),
                out,
            )?,
            ("\\stats", None) => {
                writeln!(out, "\\stats needs a table name").map_err(|e| e.to_string())?;
                false
            }
            _ => {
                writeln!(out, "invalid command {cmd}; try \\? for help").map_err(|e| e.to_string())?;
                false
            }
        };
        Ok(Some(ok))
    }

    /// `\d table`, from nc_catalog.
    fn describe_table(&mut self, t: &str, out: &mut dyn Write) -> Result<bool> {
        let q = quote(t);
        let Some(info) = self.fetch(
            &format!(
                "SELECT table_id, key_prefix, analyzed_rows, has_row_id, comment FROM nc_catalog.tables WHERE name = {q}"
            ),
            out,
        )?
        else {
            return Ok(false);
        };
        let Some(row) = info.rows.first() else {
            writeln!(out, "Did not find a table named \"{t}\".").map_err(|e| e.to_string())?;
            return Ok(false);
        };
        let cell = |i: usize| row[i].clone().unwrap_or_default();
        let analyzed = match &row[2] {
            Some(n) => format!("analyzed: {n} rows"),
            None => "not analyzed".into(),
        };
        let title = format!(
            "Table \"{t}\" (id {}, key prefix {}, {analyzed}{})",
            cell(0),
            cell(1),
            if cell(3) == "t" || cell(3) == "true" {
                ", hidden row id"
            } else {
                ""
            }
        );
        let Some(cols) = self.fetch(
            &format!(
                "SELECT name AS \"column\", type, CASE WHEN nullable THEN '' ELSE 'not null' END AS nullable, \
                 default_expr AS \"default\", coalesce(comment, '') AS comment FROM nc_catalog.columns \
                 WHERE table_name = {q} ORDER BY position"
            ),
            out,
        )?
        else {
            return Ok(false);
        };
        print_table(out, &cols, Some(&title)).map_err(|e| e.to_string())?;
        if let Some(comment) = &row[4] {
            writeln!(out, "Comment: {comment}").map_err(|e| e.to_string())?;
        }
        let sections = [
            (
                "Indexes",
                format!(
                    "SELECT name, CASE WHEN is_primary THEN 'PRIMARY KEY ' WHEN is_unique THEN 'UNIQUE ' ELSE '' END \
                     || '(' || array_to_string(columns, ', ') || ')' || ', index ' || index_id \
                     FROM nc_catalog.indexes WHERE table_name = {q} ORDER BY index_id"
                ),
            ),
            (
                "Foreign keys",
                format!(
                    "SELECT name, '(' || array_to_string(columns, ', ') || ') -> ' || referenced_table || ' (' \
                     || array_to_string(referenced_columns, ', ') || ')' \
                     || CASE WHEN on_delete <> 'no action' THEN ' on delete ' || on_delete ELSE '' END \
                     || CASE WHEN on_update <> 'no action' THEN ' on update ' || on_update ELSE '' END \
                     FROM nc_catalog.foreign_keys WHERE table_name = {q} ORDER BY name"
                ),
            ),
            (
                "Referenced by",
                format!(
                    "SELECT name, table_name || ' (' || array_to_string(columns, ', ') || ') -> (' \
                     || array_to_string(referenced_columns, ', ') || ')' \
                     FROM nc_catalog.foreign_keys WHERE referenced_table = {q} ORDER BY table_name, name"
                ),
            ),
            (
                "Checks",
                format!(
                    "SELECT name, definition FROM nc_catalog.constraints \
                     WHERE table_name = {q} AND kind = 'check' ORDER BY name"
                ),
            ),
        ];
        for (heading, sql) in sections {
            let Some(rows) = self.fetch(&sql, out)? else {
                return Ok(false);
            };
            if rows.rows.is_empty() {
                continue;
            }
            writeln!(out, "{heading}:").map_err(|e| e.to_string())?;
            for r in &rows.rows {
                let name = r[0].clone().unwrap_or_default();
                let what = r[1].clone().unwrap_or_default();
                writeln!(out, "    {name}  {what}").map_err(|e| e.to_string())?;
            }
        }
        Ok(true)
    }

    fn interactive(&mut self, out: &mut dyn Write) -> Result<()> {
        let stdin = std::io::stdin();
        let interactive = stdin.is_terminal();
        if interactive {
            writeln!(
                out,
                "ncsql (nucleus {}) — {}\nEnd statements with ;   \\? for help, \\q to quit.",
                env!("CARGO_PKG_VERSION"),
                self.backend.describe()
            )
            .map_err(|e| e.to_string())?;
        }
        let mut buf = String::new();
        let mut line = String::new();
        let mut ok = true;
        loop {
            if interactive {
                let prompt = format!(
                    "nucleus{}{} ",
                    self.backend.mark(),
                    if buf.trim().is_empty() { "=>" } else { "->" }
                );
                write!(out, "{prompt}")
                    .and_then(|()| out.flush())
                    .map_err(|e| e.to_string())?;
            }
            line.clear();
            if stdin
                .lock()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                break;
            }
            if buf.trim().is_empty() {
                let t = line.trim();
                if matches!(t, "quit" | "exit") {
                    break;
                }
                if t.starts_with('\\') {
                    match self.command(t, out)? {
                        None => break,
                        Some(r) => ok &= r,
                    }
                    continue;
                }
            }
            buf.push_str(&line);
            if buf.trim_end().ends_with(';') {
                ok &= self.run_sql(&buf, out)?;
                buf.clear();
            }
        }
        if !buf.trim().is_empty() {
            ok &= self.run_sql(&buf, out)?;
        }
        if ok || interactive {
            Ok(())
        } else {
            Err("a statement failed".into())
        }
    }
}

/// `ncsql …` / `nucleus sql …`.
pub fn run(args: &[String]) -> Result<()> {
    let mut dir = None;
    let mut command = None;
    let mut cfg = Config {
        client_name: "ncsql".into(),
        ..Config::default()
    };
    let mut remote = false;
    let mut explicit_password = None;
    let (mut tls, mut tls_cert, mut tls_key) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "-c" => command = Some(value("a command")?),
            "-h" | "--host" => {
                let h = value("a host")?;
                match h.rsplit_once(':') {
                    Some((host, port)) if port.parse::<u16>().is_ok() && !host.contains(':') => {
                        cfg.host = host.to_string();
                        cfg.port = port.parse().expect("checked");
                    }
                    _ => cfg.host = h,
                }
                remote = true;
            }
            "-p" | "--port" => {
                cfg.port = value("a port")?
                    .parse()
                    .map_err(|_| "bad port".to_string())?;
                remote = true;
            }
            "-U" | "--user" => cfg.user = value("a user name")?,
            "-d" | "--database" => cfg.database = value("a database name")?,
            "--password" => explicit_password = Some(value("a password")?),
            "--tls" => tls = Some(nucleus_tls::ClientTls::new()),
            "--tls-ca" => {
                let file = value("a CA file")?;
                tls = Some(
                    nucleus_tls::ClientTls::with_ca_file(file.as_ref())
                        .map_err(|e| format!("--tls-ca: {e}"))?,
                );
            }
            "--tls-no-verify" => tls = Some(nucleus_tls::ClientTls::without_verification()),
            "--tls-cert" => tls_cert = Some(value("a certificate file")?),
            "--token" => cfg.token = Some(value("a token")?),
            "--tls-key" => tls_key = Some(value("a key file")?),
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            s => {
                if dir.replace(s.to_string()).is_some() {
                    return Err(format!("more than one database directory\n{USAGE}"));
                }
            }
        }
    }
    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let t = tls.unwrap_or_default();
            tls = Some(
                t.with_client_certificate_files(cert.as_ref(), key.as_ref())
                    .map_err(|e| format!("--tls-cert: {e}"))?,
            );
        }
        (None, None) => {}
        _ => return Err("--tls-cert and --tls-key go together".into()),
    }
    cfg.tls = tls.map(|t| Arc::new(t) as Arc<dyn nucleus_client::Tls>);
    if dir.is_some() && remote {
        return Err("give a directory or a server, not both".into());
    }
    if let Some(pw) = explicit_password.or_else(|| std::env::var("NCPASSWORD").ok()) {
        cfg.password = Some(pw);
    }
    if cfg.token.is_none() {
        cfg.token = std::env::var("NCTOKEN").ok();
    }
    let backend = match dir {
        Some(dir) => {
            let db = Database::open(&dir, Options::default()).map_err(|e| e.to_string())?;
            Backend::Local {
                session: Session::new(Arc::new(db.clone())),
                db,
                dir,
            }
        }
        None => Backend::Remote(
            Client::connect(cfg.clone())
                .map_err(|e| format!("cannot connect to {}:{}: {e}", cfg.host, cfg.port))?,
        ),
    };
    let mut shell = Shell {
        backend,
        timing: false,
    };
    let mut out = std::io::stdout().lock();
    let result = match command {
        Some(c) => {
            let ok = if c.trim_start().starts_with('\\') {
                shell.command(c.trim(), &mut out)?.unwrap_or(true)
            } else {
                shell.run_sql(&c, &mut out)?
            };
            if ok {
                Ok(())
            } else {
                Err("the command failed".into())
            }
        }
        None => shell.interactive(&mut out),
    };
    let closed = shell.backend.close();
    result.and(closed)
}
