//! A [Model Context Protocol](https://modelcontextprotocol.io) server for
//! nucleus (nucleus PLAN.md §M14.10): an AI model — through any MCP client
//! — lists and describes the tables (their comments too), samples rows,
//! runs read-only queries under a row and time limit, explains and lints
//! them, and, when allowed, writes.
//!
//! The protocol only: JSON-RPC 2.0 over a line-delimited stream (MCP's
//! stdio transport), the tools, and the SQL they run.  The database is a
//! [`Database`] — the edition gives a local directory or a server.  A read
//! tool's SQL goes to the database marked read-only, which refuses
//! anything but reads (so `COMMIT; INSERT …` cannot slip out), and runs in
//! a `BEGIN READ ONLY` transaction rolled back after it.

use std::io::{self, BufRead, Write};

use serde_json::{Value as Json, json};

/// What a statement gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Rows: column names and types, and cells as text (`None`: NULL).
    Rows {
        /// Column names.
        columns: Vec<String>,
        /// Their types, by name: `boolean`, `smallint`, `integer`,
        /// `bigint`, `real` and `double precision` become JSON booleans
        /// and numbers; any other is a string (`numeric` too, exact).
        types: Vec<String>,
        /// The rows.
        rows: Vec<Vec<Option<String>>>,
    },
    /// A command's tag (`INSERT 0 1`, `CREATE TABLE`).
    Command(String),
}

/// A failed statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// SQLSTATE.
    pub code: String,
    /// The message.
    pub message: String,
    /// What to do about it.
    pub hint: Option<String>,
}

/// A database to serve: SQL text in, one answer per statement out (up to
/// the first failure).
pub trait Database {
    /// Runs `sql`.  With `read_only`, any statement in it that is not a
    /// read (a query, `EXPLAIN`, `SHOW`, `ANALYZE QUERY`) is refused before
    /// anything runs — `25006`, a failure of its own.
    fn run(&mut self, sql: &str, read_only: bool) -> Vec<Result<Answer, Failure>>;
}

/// What the server allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The `execute` tool (writes) is offered.
    pub allow_writes: bool,
    /// The most rows a tool returns (the rest are counted, not sent).
    pub max_rows: usize,
    /// Each statement's time limit, in milliseconds (0: none).
    pub timeout_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            allow_writes: false,
            max_rows: 100,
            timeout_ms: 5000,
        }
    }
}

/// The protocol versions spoken, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
A nucleus database (SQL, PostgreSQL-like dialect; nucleus's own catalog is \
nc_catalog).  Start with list_tables, then describe_table for the ones you \
need: comments explain what tables and columns mean.  query runs read-only \
SQL (parameters as $1, $2 …); errors come with a hint (the name meant, the \
cast that would work).  explain shows a plan; lint lists what in a statement \
is likely slow or wrong.";

/// A cell as JSON, by its column's type.
fn cell(ty: Option<&String>, text: Option<String>) -> Json {
    let Some(text) = text else {
        return Json::Null;
    };
    match ty.map(String::as_str) {
        Some("boolean") => match text.as_str() {
            "t" | "true" => Json::Bool(true),
            "f" | "false" => Json::Bool(false),
            _ => Json::String(text),
        },
        Some("smallint" | "integer" | "bigint") => text
            .parse::<i64>()
            .map_or(Json::String(text.clone()), Json::from),
        Some("real" | "double precision") => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Json::String(text), Json::Number),
        _ => Json::String(text),
    }
}

/// A row as JSON.
fn row(types: &[String], r: Vec<Option<String>>) -> Vec<Json> {
    r.into_iter()
        .enumerate()
        .map(|(i, c)| cell(types.get(i), c))
        .collect()
}

/// A SQL string literal.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A SQL identifier, quoted.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// The server's state.
pub struct Server<'a> {
    db: &'a mut dyn Database,
    options: Options,
    prepared: bool,
}

/// Why a tool call failed: the database's error, or the call's.
enum ToolError {
    Sql(Failure),
    Call(String),
}

impl From<Failure> for ToolError {
    fn from(f: Failure) -> Self {
        ToolError::Sql(f)
    }
}

type ToolResult = Result<Json, ToolError>;

impl<'a> Server<'a> {
    /// A server for `db`.
    pub fn new(db: &'a mut dyn Database, options: Options) -> Self {
        Self {
            db,
            options,
            prepared: false,
        }
    }

    /// Serves requests from `input` until it ends, answers to `output`.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// One message in, its answer out (`None` for a notification).
    pub fn handle(&mut self, line: &str) -> Option<Json> {
        let msg: Json = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => return Some(error(Json::Null, -32700, &format!("parse error: {e}"))),
        };
        // A batch (2025-03-26): each in turn.
        if let Json::Array(items) = msg {
            let replies: Vec<Json> = items.iter().filter_map(|m| self.message(m)).collect();
            return (!replies.is_empty()).then_some(Json::Array(replies));
        }
        self.message(&msg)
    }

    fn message(&mut self, msg: &Json) -> Option<Json> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Json::as_str);
        let (Some(id), Some(method)) = (id, method) else {
            return None; // a notification, or a response to us
        };
        let params = msg.get("params").cloned().unwrap_or(Json::Null);
        Some(match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Json::as_str);
                let version = asked
                    .filter(|v| PROTOCOL_VERSIONS.contains(v))
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": {
                            "name": "nucleus",
                            "title": "nucleus database",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => result(id, json!({ "tools": self.tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Json::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if !self.tools().iter().any(|t| t["name"] == name) {
                    return Some(error(id, -32602, &format!("unknown tool: {name}")));
                }
                match self.call(name, &args) {
                    Ok(v) => result(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": v.to_string() }],
                            "structuredContent": v,
                            "isError": false,
                        }),
                    ),
                    Err(e) => {
                        let text = match e {
                            ToolError::Sql(f) => {
                                let mut t = format!("ERROR: {} ({})", f.message, f.code);
                                if let Some(h) = f.hint {
                                    t.push_str(&format!("\nHINT: {h}"));
                                }
                                t
                            }
                            ToolError::Call(why) => why,
                        };
                        result(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": text }],
                                "isError": true,
                            }),
                        )
                    }
                }
            }
            other => error(id, -32601, &format!("method not found: {other}")),
        })
    }

    /// The tools offered.
    pub fn tools(&self) -> Vec<Json> {
        let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
        let mut tools = vec![
            json!({
                "name": "list_tables",
                "title": "List tables",
                "description": "The tables: name, comment (what it holds), and the row count when analyzed.",
                "inputSchema": { "type": "object", "properties": {} },
                "annotations": read_only,
            }),
            json!({
                "name": "describe_table",
                "title": "Describe a table",
                "description": "A table's columns (type, nullable, default, comment), primary key, indexes, foreign keys and the tables referencing it.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "table": { "type": "string", "description": "The table's name." } },
                    "required": ["table"],
                },
                "annotations": read_only,
            }),
            json!({
                "name": "sample_rows",
                "title": "Sample rows",
                "description": "A few rows of a table, to see what its values look like.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "table": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "description": "How many (default 5)." },
                    },
                    "required": ["table"],
                },
                "annotations": read_only,
            }),
            json!({
                "name": "query",
                "title": "Run a read-only query",
                "description": format!(
                    "Runs SQL in a read-only transaction (nothing it does is kept) and returns the rows, at most {} (more are counted); each statement may take {} ms. Parameters as $1, $2 … with params.",
                    self.options.max_rows, self.options.timeout_ms
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sql": { "type": "string" },
                        "params": {
                            "type": "array",
                            "description": "Values for $1, $2 …: strings, numbers, booleans or null.",
                        },
                    },
                    "required": ["sql"],
                },
                "annotations": read_only,
            }),
            json!({
                "name": "explain",
                "title": "Explain a query",
                "description": "The query's plan; with analyze, run (read-only) with every step's actual rows and time beside the estimates.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sql": { "type": "string" },
                        "analyze": { "type": "boolean" },
                    },
                    "required": ["sql"],
                },
                "annotations": read_only,
            }),
            json!({
                "name": "lint",
                "title": "Lint a statement",
                "description": "What in a query, UPDATE or DELETE is likely slow or wrong (full scans an index would serve, missing foreign key indexes, cartesian products, NOT IN over NULLs …), each with its fix. The statement is not run.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "sql": { "type": "string" } },
                    "required": ["sql"],
                },
                "annotations": read_only,
            }),
        ];
        if self.options.allow_writes {
            tools.push(json!({
                "name": "execute",
                "title": "Run SQL that writes",
                "description": "Runs SQL — INSERT, UPDATE, DELETE, CREATE … — and keeps what it does. Returns each statement's result.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "sql": { "type": "string" } },
                    "required": ["sql"],
                },
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false },
            }));
        }
        tools
    }

    /// Runs `sql`; its answers, or the first failure.
    fn run(&mut self, sql: &str) -> Result<Vec<Answer>, Failure> {
        self.db.run(sql, false).into_iter().collect()
    }

    /// Runs `sql` in a read-only transaction, rolled back after.
    fn read_only(&mut self, sql: &str) -> Result<Vec<Answer>, Failure> {
        if !self.prepared {
            if self.options.timeout_ms > 0 {
                self.run(&format!(
                    "SET statement_timeout = {}",
                    literal(&format!("{}ms", self.options.timeout_ms))
                ))?;
            }
            self.prepared = true;
        }
        self.run("BEGIN READ ONLY")?;
        let r = self.db.run(sql, true).into_iter().collect();
        let _ = self.run("ROLLBACK");
        r
    }

    /// The rows of one query, as JSON: columns, rows (at most `max_rows`),
    /// and how many there were.
    fn rows(&self, answers: Vec<Answer>) -> Json {
        let shown = |columns: Vec<String>, types: Vec<String>, rows: Vec<Vec<Option<String>>>| {
            let total = rows.len();
            let rows: Vec<Json> = rows
                .into_iter()
                .take(self.options.max_rows)
                .map(|r| Json::Array(row(&types, r)))
                .collect();
            json!({
                "columns": columns,
                "rows": rows,
                "row_count": total,
                "truncated": total > self.options.max_rows,
            })
        };
        let mut results: Vec<Json> = answers
            .into_iter()
            .map(|a| match a {
                Answer::Rows {
                    columns,
                    types,
                    rows,
                } => shown(columns, types, rows),
                Answer::Command(tag) => json!({ "command": tag }),
            })
            .collect();
        match results.len() {
            1 => results.remove(0),
            _ => json!({ "results": results }),
        }
    }

    /// One query's rows as objects (column → value).
    fn objects(&mut self, sql: &str) -> Result<Vec<serde_json::Map<String, Json>>, Failure> {
        let mut out = Vec::new();
        for a in self.read_only(sql)? {
            if let Answer::Rows {
                columns,
                types,
                rows,
            } = a
            {
                for r in rows {
                    out.push(columns.iter().cloned().zip(row(&types, r)).collect());
                }
            }
        }
        Ok(out)
    }

    fn call(&mut self, name: &str, args: &Json) -> ToolResult {
        let text = |key: &str| -> Result<String, ToolError> {
            args.get(key)
                .and_then(Json::as_str)
                .map(str::to_string)
                .ok_or_else(|| ToolError::Call(format!("{name} needs \"{key}\" (a string)")))
        };
        match name {
            "list_tables" => Ok(json!({
                "tables": self.objects(
                    "SELECT name, comment, analyzed_rows AS rows FROM nc_catalog.tables ORDER BY name",
                )?,
            })),
            "describe_table" => {
                let table = text("table")?;
                let t = literal(&table);
                let mut found = self.objects(&format!(
                    "SELECT name, comment, analyzed_rows AS rows, primary_key, owner, row_security \
                     FROM nc_catalog.tables WHERE name = {t}"
                ))?;
                if found.is_empty() {
                    // The database's own error, with its hint (the name meant).
                    self.read_only(&format!("SELECT * FROM {} LIMIT 0", ident(&table)))?;
                    return Err(ToolError::Call(format!("no table named {table}")));
                }
                let mut d = found.remove(0);
                d.insert(
                    "columns".into(),
                    Json::from(self.objects(&format!(
                        "SELECT name, type, nullable, default_expr AS \"default\", comment \
                         FROM nc_catalog.columns WHERE table_name = {t} ORDER BY position"
                    ))?),
                );
                d.insert(
                    "indexes".into(),
                    Json::from(self.objects(&format!(
                        "SELECT name, columns, is_unique AS \"unique\", is_primary AS \"primary\" \
                         FROM nc_catalog.indexes WHERE table_name = {t} ORDER BY index_id"
                    ))?),
                );
                d.insert(
                    "foreign_keys".into(),
                    Json::from(self.objects(&format!(
                        "SELECT name, columns, referenced_table, referenced_columns, on_delete, on_update \
                         FROM nc_catalog.foreign_keys WHERE table_name = {t} ORDER BY name"
                    ))?),
                );
                d.insert(
                    "referenced_by".into(),
                    Json::from(self.objects(&format!(
                        "SELECT table_name, name, columns FROM nc_catalog.foreign_keys \
                         WHERE referenced_table = {t} ORDER BY table_name, name"
                    ))?),
                );
                Ok(Json::Object(d))
            }
            "sample_rows" => {
                let table = text("table")?;
                let limit = args.get("limit").and_then(Json::as_u64).unwrap_or(5);
                let limit = limit.clamp(1, self.options.max_rows as u64);
                let answers =
                    self.read_only(&format!("SELECT * FROM {} LIMIT {limit}", ident(&table)))?;
                Ok(self.rows(answers))
            }
            "query" => {
                let sql = text("sql")?;
                let sql = match args.get("params") {
                    None | Some(Json::Null) => sql,
                    Some(Json::Array(ps)) => bind(&sql, ps).map_err(ToolError::Call)?,
                    Some(_) => return Err(ToolError::Call("params is an array".into())),
                };
                let answers = self.read_only(&sql)?;
                Ok(self.rows(answers))
            }
            "explain" => {
                let sql = text("sql")?;
                let analyze = args.get("analyze").and_then(Json::as_bool).unwrap_or(false);
                let answers = self.read_only(&format!(
                    "EXPLAIN {}{sql}",
                    if analyze { "ANALYZE " } else { "" }
                ))?;
                let lines: Vec<Json> = answers
                    .into_iter()
                    .flat_map(|a| match a {
                        Answer::Rows { rows, .. } => rows,
                        Answer::Command(_) => Vec::new(),
                    })
                    .filter_map(|r| r.into_iter().next().flatten())
                    .map(Json::String)
                    .collect();
                Ok(json!({ "plan": lines }))
            }
            "lint" => {
                let sql = text("sql")?;
                Ok(json!({
                    "findings": self.objects(&format!("ANALYZE QUERY {}", sql.trim().trim_end_matches(';')))?,
                }))
            }
            "execute" => {
                let sql = text("sql")?;
                let answers = self.run(&sql)?;
                Ok(self.rows(answers))
            }
            other => Err(ToolError::Call(format!("unknown tool: {other}"))),
        }
    }
}

/// `sql` with `$n` replaced by `params[n-1]` as SQL literals (strings
/// quoted, numbers and booleans as they are, null as NULL) — the
/// statement's text, as the database sees literals.
fn bind(sql: &str, params: &[Json]) -> Result<String, String> {
    let lit = |p: &Json| -> Result<String, String> {
        Ok(match p {
            Json::Null => "NULL".into(),
            Json::Bool(b) => b.to_string(),
            Json::Number(n) => n.to_string(),
            Json::String(s) => literal(s),
            other => {
                return Err(format!(
                    "a parameter is a string, number, boolean or null, not {other}"
                ));
            }
        })
    };
    let mut out = String::new();
    let mut chars = sql.char_indices().peekable();
    // Outside string literals and quoted names only.
    let mut quote: Option<char> = None;
    while let Some((_, c)) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                out.push(c);
            }
            (Some(_), c) => out.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                out.push(c);
            }
            (None, '$') if chars.peek().is_some_and(|(_, d)| d.is_ascii_digit()) => {
                let mut n = String::new();
                while let Some((_, d)) = chars.peek().filter(|(_, d)| d.is_ascii_digit()) {
                    n.push(*d);
                    chars.next();
                }
                let i: usize = n.parse().map_err(|_| format!("bad parameter ${n}"))?;
                let p = params
                    .get(i.wrapping_sub(1))
                    .ok_or_else(|| format!("${i} has no value in params"))?;
                out.push_str(&lit(p)?);
            }
            (None, c) => out.push(c),
        }
    }
    Ok(out)
}

fn result(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Json, code: i64, message: &str) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_become_literals() {
        assert_eq!(
            bind(
                "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = '$1'",
                &[json!("it's"), json!(3)]
            )
            .unwrap(),
            "SELECT * FROM t WHERE a = 'it''s' AND b = 3 AND c = '$1'"
        );
        assert_eq!(
            bind("SELECT $1, $2", &[json!(null), json!(true)]).unwrap(),
            "SELECT NULL, true"
        );
        assert!(bind("SELECT $3", &[json!(1)]).is_err());
        assert!(bind("SELECT $1", &[json!([1])]).is_err());
    }
}
