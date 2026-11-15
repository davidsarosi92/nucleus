//! Typed functions from named queries in `.sql` files (PLAN.md §M14.16
//! M6, as sqlc):
//!
//! ```sql
//! -- name: user_by_email :one
//! -- A user, by the address they log in with.
//! SELECT id, email, name FROM users WHERE email = $1;
//! ```
//!
//! `:one` returns the first row or none, `:many` the rows, `:exec` the
//! count of rows changed.  The database describes each query (`DESCRIBE`):
//! its parameters' types and its columns' names, types and whether they
//! may be `NULL`; a query it cannot describe is an error at its line.  A
//! parameter is named after the column it is compared with or assigned
//! to (`email = $1`, `INSERT … (a, b) VALUES ($1, $2)`, `SET a = $1`,
//! `LIMIT $1`), else `p1`, …; `-- params: email, name?` names them
//! itself, `?` taking `NULL` too.

use super::{
    JAVA_RESERVED, PY_RESERVED, RUST_RESERVED, Result, Ty, field, java_type, pascal, py_type,
    quoted, rust_conv, rust_header, rust_type, ts_type, ty,
};
use crate::dump::rows;
use crate::sql::Backend;
use nucleus_sql::lexer::{Token, tokenize};

/// What a query returns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    /// The first row, or none.
    One,
    /// The rows.
    Many,
    /// The count of rows changed.
    Exec,
}

/// A parameter or a result column.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Item {
    pub name: String,
    pub ty: Ty,
    pub nullable: bool,
}

/// A named query.
#[derive(Debug)]
pub(super) struct Query {
    pub name: String,
    pub kind: Kind,
    pub sql: String,
    pub doc: Vec<String>,
    /// Where it is: the file and the line of its `-- name:`.
    pub at: String,
    /// The line its SQL starts at.
    sql_line: usize,
    /// `-- params:`: names, and whether each takes `NULL`.
    named: Vec<(String, bool)>,
    pub params: Vec<Item>,
    pub columns: Vec<Item>,
}

/// The named queries of a file.
pub(super) fn parse(file: &str, text: &str) -> Result<Vec<Query>> {
    let mut out: Vec<Query> = Vec::new();
    let mut current: Option<Query> = None;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let t = line.trim();
        if let Some(rest) = t
            .strip_prefix("-- name:")
            .or_else(|| t.strip_prefix("--name:"))
        {
            if let Some(q) = current.take() {
                out.push(finish(q)?);
            }
            let mut words = rest.split_whitespace();
            let (Some(name), Some(kind), None) = (words.next(), words.next(), words.next()) else {
                return Err(format!(
                    "{file}:{n}: expected `-- name: <name> :one | :many | :exec`"
                ));
            };
            if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(format!(
                    "{file}:{n}: a query's name is letters, digits and _: {name}"
                ));
            }
            if out.iter().any(|q| q.name == name) {
                return Err(format!("{file}:{n}: a second query named {name}"));
            }
            let kind = match kind {
                ":one" => Kind::One,
                ":many" => Kind::Many,
                ":exec" => Kind::Exec,
                k => return Err(format!("{file}:{n}: {k}: expected :one, :many or :exec")),
            };
            current = Some(Query {
                name: name.to_string(),
                kind,
                sql: String::new(),
                doc: Vec::new(),
                at: format!("{file}:{n}"),
                sql_line: n + 1,
                named: Vec::new(),
                params: Vec::new(),
                columns: Vec::new(),
            });
            continue;
        }
        let Some(q) = current.as_mut() else {
            if t.is_empty() || t.starts_with("--") {
                continue;
            }
            return Err(format!("{file}:{n}: SQL before the first `-- name:`"));
        };
        if q.sql.trim().is_empty() && t.starts_with("--") {
            let c = t.trim_start_matches('-').trim();
            if let Some(list) = c.strip_prefix("params:") {
                for p in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    let (p, null) = match p.strip_suffix('?') {
                        Some(p) => (p.trim(), true),
                        None => (p, false),
                    };
                    q.named.push((p.to_string(), null));
                }
            } else {
                q.doc.push(c.to_string());
            }
            q.sql_line = n + 1;
            continue;
        }
        if q.sql.trim().is_empty() {
            q.sql_line = n;
            q.sql.clear();
        }
        q.sql.push_str(line);
        q.sql.push('\n');
    }
    if let Some(q) = current.take() {
        out.push(finish(q)?);
    }
    Ok(out)
}

fn finish(mut q: Query) -> Result<Query> {
    let sql = q.sql.trim_end();
    let sql = sql.strip_suffix(';').unwrap_or(sql).trim_end();
    if sql.trim().is_empty() {
        return Err(format!("{}: {} has no SQL", q.at, q.name));
    }
    q.sql = sql.to_string();
    // One statement, parsed here: a describe never runs what follows a `;`.
    if let Err(e) = nucleus_sql::parser::parse_one(&q.sql) {
        return Err(format!("{}: {}", where_in(&q, &e.message, 0), e.message));
    }
    Ok(q)
}

/// What a query is described with.
const DESCRIBE: &str = "DESCRIBE ";

/// The file and line of an error's `(position N)` (in a text `skip`
/// characters before the query's), else of the query.
fn where_in(q: &Query, message: &str, skip: usize) -> String {
    let file = q.at.rsplit_once(':').map_or(q.at.as_str(), |(f, _)| f);
    let at = message
        .split("(position ")
        .nth(1)
        .and_then(|r| r.split(')').next())
        .and_then(|n| n.parse::<usize>().ok());
    match at {
        Some(pos) => {
            let before: String = q.sql.chars().take(pos.saturating_sub(1 + skip)).collect();
            let line = q.sql_line + before.matches('\n').count();
            let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("{file}:{line}:{col}")
        }
        None => format!("{file}:{}", q.sql_line),
    }
}

/// Each query described by the database: its parameters and columns.
pub(super) fn describe(
    b: &mut Backend,
    queries: &mut [Query],
    enums: &[(String, Vec<String>)],
) -> Result<()> {
    for q in queries.iter_mut() {
        let described = rows(b, &format!("{DESCRIBE}{}", q.sql)).map_err(|e| {
            let e = e.strip_prefix("ERROR:  ").unwrap_or(&e);
            format!("{}: {}: {e}", where_in(q, e, DESCRIBE.len()), q.name)
        })?;
        let guessed = param_names(&q.sql);
        for r in described {
            let get = |i: usize| r[i].clone().unwrap_or_default();
            let position: usize = get(1).parse().unwrap_or(0);
            if get(0) == "parameter" {
                let Some(t) = r[3].clone() else {
                    return Err(format!(
                        "{}: {}: the type of ${position} cannot be told from the query: cast it (${position}::text)",
                        q.at, q.name
                    ));
                };
                let (name, nullable) = match q.named.get(position - 1) {
                    Some((n, null)) => (n.clone(), *null),
                    None => (
                        guessed
                            .get(&position)
                            .cloned()
                            .unwrap_or_else(|| format!("p{position}")),
                        false,
                    ),
                };
                q.params.push(Item {
                    name,
                    ty: ty(&t, enums),
                    nullable,
                });
            } else {
                q.columns.push(Item {
                    name: get(2),
                    ty: ty(&get(3), enums),
                    nullable: get(4) != "f" && get(4) != "false",
                });
            }
        }
        if q.named.len() > q.params.len() {
            return Err(format!(
                "{}: {}: `-- params:` names {} parameters, the query has {}",
                q.at,
                q.name,
                q.named.len(),
                q.params.len()
            ));
        }
        if q.kind != Kind::Exec && q.columns.is_empty() {
            return Err(format!(
                "{}: {} returns no rows: make it :exec (or add RETURNING)",
                q.at, q.name
            ));
        }
    }
    Ok(())
}

/// Each `$n`'s name from its place: the column it is compared with,
/// assigned to or combined with (`pay + $1`), an `INSERT`'s column,
/// `LIMIT` or `OFFSET`.
fn param_names(sql: &str) -> std::collections::HashMap<usize, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(tokens) = tokenize(sql) else {
        return out;
    };
    let toks: Vec<&Token> = tokens.iter().map(|(t, _)| t).collect();
    let name_of = |t: &Token| match t {
        Token::Word(w) | Token::Quoted(w) => Some(w.clone()),
        _ => None,
    };
    let compares = |t: &Token| {
        matches!(t, Token::Op("=" | "<" | ">" | "<=" | ">=" | "<>" | "!="))
            || t.is("like")
            || t.is("ilike")
    };
    // INSERT INTO t (a, b) VALUES ($1, $2): by position.
    if let Some(cols_at) = toks.iter().position(|t| t.is("insert")).and_then(|i| {
        toks[i..]
            .iter()
            .position(|t| matches!(t, Token::Op("(")))
            .map(|p| i + p)
    }) {
        let mut cols = Vec::new();
        let mut j = cols_at + 1;
        while j < toks.len() && !matches!(toks[j], Token::Op(")")) {
            if let Some(n) = name_of(toks[j]) {
                cols.push(n);
            }
            j += 1;
        }
        if let Some(v) = toks.iter().position(|t| t.is("values")) {
            let mut k = 0;
            let mut depth = 0;
            for t in &toks[v + 1..] {
                match t {
                    Token::Op("(") => depth += 1,
                    Token::Op(")") => {
                        depth -= 1;
                        if depth == 0 {
                            k = 0;
                        }
                    }
                    Token::Op(",") if depth == 1 => k += 1,
                    Token::Param(n) if depth == 1 => {
                        if let Some(c) = cols.get(k) {
                            out.entry(*n).or_insert_with(|| c.clone());
                        }
                    }
                    _ => {}
                }
                if depth == 0 && t.is("returning") {
                    break;
                }
            }
        }
    }
    for (i, t) in toks.iter().enumerate() {
        let Token::Param(n) = t else { continue };
        let before = |k: usize| i.checked_sub(k).map(|j| toks[j]);
        let arith = |t: &Token| matches!(t, Token::Op("+" | "-" | "*" | "/" | "||"));
        let name = if before(1).is_some_and(|t| compares(t) || arith(t)) {
            before(2).and_then(name_of)
        } else if before(1).is_some_and(|t| t.is("limit") || t.is("offset")) {
            before(1).and_then(name_of)
        } else if toks.get(i + 1).is_some_and(|t| compares(t)) {
            toks.get(i + 2).and_then(|t| name_of(t))
        } else if before(1).is_some_and(|t| matches!(t, Token::Op("(")))
            && before(2).is_some_and(|t| t.is("in"))
        {
            // x IN ($n, …)
            before(3).and_then(name_of)
        } else if before(1).is_some_and(|t| matches!(t, Token::Op("(")))
            && before(2).is_some_and(|t| t.is("any"))
            && before(3).is_some_and(compares)
        {
            // x = ANY($n)
            before(4).and_then(name_of)
        } else {
            None
        };
        if let Some(name) = name.filter(|n| !is_keyword(n)) {
            out.entry(*n).or_insert(name);
        }
    }
    out
}

fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "select" | "where" | "and" | "or" | "not" | "set" | "on" | "when" | "then" | "else"
    )
}

// ------------------------------------------------------------ naming

fn camel(name: &str) -> String {
    let p = pascal(name);
    let mut cs = p.chars();
    match cs.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + cs.as_str(),
        None => p,
    }
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `NAME`'s constant name: `USER_BY_EMAIL_SQL`.
fn constant(name: &str) -> String {
    snake(name).to_ascii_uppercase() + "_SQL"
}

/// The parameters' names in a language, unique and not `db`.
fn param_fields(q: &Query, reserved: &[&str], case: fn(&str) -> String) -> Vec<String> {
    let mut taken = vec!["db".to_string(), "conn".to_string()];
    q.params
        .iter()
        .map(|p| field(&case(&p.name), reserved, &mut taken))
        .collect()
}

// --------------------------------------------------------- TypeScript

const JS_RESERVED: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "arguments",
    "eval",
];

/// A parameter's type in TypeScript: what the driver takes.
fn ts_param(t: &Ty) -> String {
    match t {
        Ty::Json => "nucleus.Json | { [key: string]: nucleus.JsonValue }".into(),
        t => ts_type(t),
    }
}

pub(super) fn typescript(queries: &[Query], module: &str) -> String {
    let mut out = String::from(
        "// Generated by `nucleus codegen ts --queries` from the queries: do not edit.\n",
    );
    out.push_str(&format!(
        "import type * as nucleus from {};\n\n\
         /** What runs a statement: a client, a transaction's, a database in the process. */\n\
         export interface Queryable {{\n  query(sql: string, params?: nucleus.Value[]): Promise<nucleus.Result>;\n}}\n",
        quoted(module)
    ));
    for q in queries {
        let f = field(&camel(&q.name), JS_RESERVED, &mut Vec::new());
        let row = pascal(&q.name) + "Row";
        let sql_const = camel(&q.name) + "Sql";
        out.push_str(&format!(
            "\nexport const {sql_const} = {};\n",
            quoted(&q.sql)
        ));
        let mut keys = Vec::new();
        if q.kind != Kind::Exec {
            out.push_str(&format!("\nexport interface {row} {{\n"));
            let mut taken = Vec::new();
            for c in &q.columns {
                let mut key = if c.name.chars().enumerate().all(|(i, ch)| {
                    ch == '_'
                        || ch == '$'
                        || ch.is_ascii_alphabetic()
                        || (i > 0 && ch.is_ascii_digit())
                }) && !c.name.is_empty()
                {
                    c.name.clone()
                } else {
                    quoted(&c.name)
                };
                let base = key.clone();
                let mut n = 2;
                while taken.contains(&key) {
                    key = format!("{}_{n}", base.trim_matches('"'));
                    n += 1;
                }
                taken.push(key.clone());
                let t = ts_type(&c.ty);
                let full = if c.nullable { format!("{t} | null") } else { t };
                out.push_str(&format!("  {key}: {full};\n"));
                keys.push((key, full));
            }
            out.push_str("}\n");
        }
        let names = param_fields(q, JS_RESERVED, |n| n.to_string());
        let params: Vec<String> = q
            .params
            .iter()
            .zip(&names)
            .map(|(p, n)| {
                let t = ts_param(&p.ty);
                if p.nullable {
                    format!(", {n}: {t} | null")
                } else {
                    format!(", {n}: {t}")
                }
            })
            .collect();
        let doc: String = q.doc.iter().map(|d| format!("\n * {d}")).collect();
        let ret = match q.kind {
            Kind::One => format!("{row} | null"),
            Kind::Many => format!("{row}[]"),
            Kind::Exec => "number".into(),
        };
        out.push_str(&format!(
            "\n/** {} ({}).{doc} */\nexport async function {f}(db: Queryable{}): Promise<{ret}> {{\n  const r = await db.query({sql_const}, [{}] as nucleus.Value[]);\n",
            q.name,
            q.at,
            params.concat(),
            names.join(", ")
        ));
        let make = |indent: &str| {
            let fields: String = keys
                .iter()
                .enumerate()
                .map(|(i, (k, t))| format!("{indent}  {k}: row[{i}] as {t},\n"))
                .collect();
            format!("{{\n{fields}{indent}}}")
        };
        match q.kind {
            Kind::One => out.push_str(&format!(
                "  const row = r.rows[0];\n  return row === undefined ? null : {};\n}}\n",
                make("  ")
            )),
            Kind::Many => out.push_str(&format!(
                "  return r.rows.map((row) => ({}));\n}}\n",
                make("  ")
            )),
            Kind::Exec => out.push_str("  return r.count;\n}\n"),
        }
    }
    out
}

// -------------------------------------------------------------- Python

pub(super) fn python(queries: &[Query], module: &str) -> String {
    let mut out = String::from(
        "# Generated by `nucleus codegen python --queries` from the queries: do not edit.\n\
         from __future__ import annotations\n\n\
         import array\nimport dataclasses\nimport datetime\nimport decimal\nimport ipaddress\nimport uuid\n\
         from typing import Any, List, Literal, Optional, Sequence, Union\n\n",
    );
    out.push_str(&format!("import {module}\n"));
    for q in queries {
        let f = field(&snake(&q.name), PY_RESERVED, &mut Vec::new());
        let row = pascal(&q.name) + "Row";
        let sql_const = constant(&q.name);
        out.push_str(&format!("\n{sql_const} = {}\n", quoted(&q.sql)));
        if q.kind != Kind::Exec {
            out.push_str(&format!(
                "\n\n@dataclasses.dataclass\nclass {row}:\n    \"\"\"A row of {}.\"\"\"\n\n",
                q.name
            ));
            let mut taken = Vec::new();
            for c in &q.columns {
                let t = py_type(&c.ty, module);
                let n = field(&c.name, PY_RESERVED, &mut taken);
                if c.nullable {
                    out.push_str(&format!("    {n}: Optional[{t}]\n"));
                } else {
                    out.push_str(&format!("    {n}: {t}\n"));
                }
            }
        }
        let names = param_fields(q, PY_RESERVED, |n| n.to_string());
        let params: String = q
            .params
            .iter()
            .zip(&names)
            .map(|(p, n)| {
                let t = py_type(&p.ty, module);
                if p.nullable {
                    format!(", {n}: Optional[{t}]")
                } else {
                    format!(", {n}: {t}")
                }
            })
            .collect();
        let ret = match q.kind {
            Kind::One => format!("Optional[{row}]"),
            Kind::Many => format!("List[{row}]"),
            Kind::Exec => "int".into(),
        };
        let doc: String = q.doc.iter().map(|d| format!("\n    {d}")).collect();
        let tuple = match names.len() {
            0 => "()".to_string(),
            1 => format!("({},)", names[0]),
            _ => format!("({})", names.join(", ")),
        };
        out.push_str(&format!(
            "\n\ndef {f}(conn: Any{params}) -> {ret}:\n    \"\"\"{} ({}).{doc}\"\"\"\n    r = conn.execute({sql_const}, {tuple})\n",
            q.name, q.at
        ));
        match q.kind {
            Kind::One => out.push_str(&format!(
                "    return {row}(*r.rows[0]) if r.rows else None\n"
            )),
            Kind::Many => out.push_str(&format!("    return [{row}(*row) for row in r.rows]\n")),
            Kind::Exec => out.push_str("    return r.count\n"),
        }
    }
    out
}

// ---------------------------------------------------------------- Java

pub(super) fn java(queries: &[Query], package: Option<&str>, class: &str) -> String {
    let mut out = String::from(
        "// Generated by `nucleus codegen java --queries` from the queries: do not edit.\n",
    );
    if let Some(p) = package {
        out.push_str(&format!("package {p};\n"));
    }
    out.push_str(&format!(
        "\nimport io.github.davidsarosi92.nucleus.NucleusClient;\nimport java.util.ArrayList;\nimport java.util.List;\nimport java.util.Optional;\n\n/** The queries, a function each. */\npublic final class {class} {{\n    private {class}() {{}}\n"
    ));
    for q in queries {
        let f = field(&camel(&q.name), JAVA_RESERVED, &mut Vec::new());
        let row = pascal(&q.name) + "Row";
        let sql_const = constant(&q.name);
        out.push_str(&format!(
            "\n    public static final String {sql_const} = {};\n",
            quoted(&q.sql)
        ));
        let mut taken = Vec::new();
        let fields: Vec<(String, String)> = q
            .columns
            .iter()
            .map(|c| (field(&c.name, JAVA_RESERVED, &mut taken), java_type(&c.ty)))
            .collect();
        if q.kind != Kind::Exec {
            let comps: Vec<String> = fields.iter().map(|(n, t)| format!("{t} {n}")).collect();
            out.push_str(&format!(
                "\n    /** A row of {}. */\n    public record {row}({}) {{\n        @SuppressWarnings(\"unchecked\")\n        static {row} of(List<Object> row) {{\n            return new {row}(\n{});\n        }}\n    }}\n",
                q.name,
                comps.join(", "),
                fields
                    .iter()
                    .enumerate()
                    .map(|(i, (_, t))| format!("                ({t}) row.get({i})"))
                    .collect::<Vec<_>>()
                    .join(",\n")
            ));
        }
        let names = param_fields(q, JAVA_RESERVED, camel);
        let params: String = q
            .params
            .iter()
            .zip(&names)
            .map(|(p, n)| format!(", {} {n}", java_type(&p.ty)))
            .collect();
        let args: String = names.iter().map(|n| format!(", {n}")).collect();
        let doc: String = q.doc.iter().map(|d| format!("\n     * {d}")).collect();
        let ret = match q.kind {
            Kind::One => format!("Optional<{row}>"),
            Kind::Many => format!("List<{row}>"),
            Kind::Exec => "long".into(),
        };
        out.push_str(&format!(
            "\n    /** {} ({}).{doc} */\n    public static {ret} {f}(NucleusClient db{params}) {{\n        var r = db.query({sql_const}{args});\n",
            q.name,
            q.at.replace("*/", "* /")
        ));
        match q.kind {
            Kind::One => out.push_str(&format!(
                "        return r.rows().isEmpty() ? Optional.empty() : Optional.of({row}.of(r.rows().get(0)));\n    }}\n"
            )),
            Kind::Many => out.push_str(&format!(
                "        List<{row}> out = new ArrayList<>();\n        for (List<Object> row : r.rows()) out.add({row}.of(row));\n        return out;\n    }}\n"
            )),
            Kind::Exec => out.push_str("        return r.count();\n    }\n"),
        }
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------- Rust

/// Names the generated helpers take.
const RUST_QUERY_RESERVED: &[&str] = &["column", "nullable", "decode"];

/// A parameter's type in Rust and its value from the name.
fn rust_param(t: &Ty, n: &str) -> (String, String) {
    let (ty, variant, conv) = match t {
        Ty::Bool => ("bool", "Bool", ""),
        Ty::SmallInt => ("i16", "SmallInt", ""),
        Ty::Integer => ("i32", "Integer", ""),
        Ty::BigInt => ("i64", "Int", ""),
        Ty::Real => ("f32", "Real", ""),
        Ty::Double => ("f64", "Float", ""),
        Ty::Text | Ty::Enum(_) => ("&str", "Text", ".to_string()"),
        Ty::Json => ("&str", "Json", ".to_string()"),
        Ty::Bytes => ("&[u8]", "Bytes", ".to_vec()"),
        Ty::Date => ("i32", "Date", ""),
        Ty::Timestamp => ("i64", "Timestamp", ""),
        Ty::TimestampTz => ("i64", "TimestampTz", ""),
        Ty::Time => ("i64", "Time", ""),
        Ty::Uuid => ("[u8; 16]", "Uuid", ""),
        Ty::Vector => ("&[f32]", "Vector", ".to_vec()"),
        _ => return ("Value".into(), n.to_string()),
    };
    (ty.into(), format!("Value::{variant}({n}{conv})"))
}

pub(super) fn rust(queries: &[Query], krate: &str) -> String {
    let mut out = rust_header("rust --queries` from the queries", krate);
    out.push_str(&format!(
        "\nuse {krate}::{{Client, Error}};\n\n\
         /// A row's values taken apart, a wrong type a protocol error.\n\
         fn decode<T>(r: Result<T, String>) -> Result<T, Error> {{\n    r.map_err(Error::Protocol)\n}}\n"
    ));
    let mut reserved: Vec<&str> = RUST_RESERVED.to_vec();
    reserved.extend(RUST_QUERY_RESERVED);
    for q in queries {
        let f = field(
            &snake(&q.name).to_ascii_lowercase(),
            &reserved,
            &mut Vec::new(),
        );
        let row = pascal(&q.name) + "Row";
        let sql_const = constant(&q.name);
        out.push_str(&format!(
            "\n/// The SQL of {}.\npub const {sql_const}: &str = {};\n",
            q.name,
            quoted(&q.sql)
        ));
        if q.kind != Kind::Exec {
            let mut taken = Vec::new();
            let fields: Vec<(String, &Item)> = q
                .columns
                .iter()
                .map(|c| {
                    (
                        field(&c.name.to_ascii_lowercase(), RUST_RESERVED, &mut taken),
                        c,
                    )
                })
                .collect();
            out.push_str(&format!(
                "\n/// A row of {}.\n#[derive(Clone, Debug, PartialEq)]\npub struct {row} {{\n",
                q.name
            ));
            for (n, c) in &fields {
                let rt = rust_type(&c.ty);
                if c.nullable && rt != "Value" {
                    out.push_str(&format!("    pub {n}: Option<{rt}>,\n"));
                } else {
                    out.push_str(&format!("    pub {n}: {rt},\n"));
                }
            }
            out.push_str(&format!(
                "}}\n\nimpl {row} {{\n    fn of(row: Vec<Value>) -> Result<Self, String> {{\n        let mut row = row.into_iter();\n        Ok(Self {{\n"
            ));
            for (n, c) in &fields {
                let conv = rust_conv(&c.ty);
                let expr = if c.nullable && rust_type(&c.ty) != "Value" {
                    format!("|v: Value| nullable(v, {conv})")
                } else {
                    conv
                };
                out.push_str(&format!(
                    "            {n}: column(&mut row, {}, {expr})?,\n",
                    quoted(&c.name)
                ));
            }
            out.push_str("        })\n    }\n}\n");
        }
        let names = param_fields(q, &reserved, |n| n.to_ascii_lowercase());
        let mut params = String::new();
        let mut values = Vec::new();
        for (p, n) in q.params.iter().zip(&names) {
            let (t, v) = rust_param(&p.ty, n);
            if p.nullable && t != "Value" {
                params.push_str(&format!(", {n}: Option<{t}>"));
                values.push(format!("{n}.map_or(Value::Null, |{n}| {v})"));
            } else {
                params.push_str(&format!(", {n}: {t}"));
                values.push(v);
            }
        }
        let doc: String = q.doc.iter().map(|d| format!("\n/// {d}")).collect();
        let ret = match q.kind {
            Kind::One => format!("Option<{row}>"),
            Kind::Many => format!("Vec<{row}>"),
            Kind::Exec => "u64".into(),
        };
        out.push_str(&format!(
            "\n/// {} ({}).{doc}\npub fn {f}(db: &mut Client{params}) -> Result<{ret}, Error> {{\n    let r = db.execute({sql_const}, &[{}])?;\n",
            q.name,
            q.at,
            values.join(", ")
        ));
        match q.kind {
            Kind::One => out.push_str(&format!(
                "    r.rows.into_iter().next().map(|row| decode({row}::of(row))).transpose()\n}}\n"
            )),
            Kind::Many => out.push_str(&format!(
                "    r.rows.into_iter().map(|row| decode({row}::of(row))).collect()\n}}\n"
            )),
            Kind::Exec => out.push_str("    Ok(r.count)\n}\n"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_read() {
        let text = "-- the file's own comment\n\n\
                    -- name: user_by_email :one\n-- A user.\n-- params: email\nSELECT id\n  FROM users WHERE email = $1;\n\n\
                    -- name: touch :exec\nUPDATE users SET seen = now()\n";
        let qs = parse("q.sql", text).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].sql, "SELECT id\n  FROM users WHERE email = $1");
        assert_eq!(
            (qs[0].kind, qs[0].doc.as_slice()),
            (Kind::One, &["A user.".to_string()][..])
        );
        assert_eq!(qs[0].named, [("email".to_string(), false)]);
        assert_eq!((qs[0].sql_line, qs[0].at.as_str()), (6, "q.sql:3"));
        assert_eq!(qs[1].kind, Kind::Exec);
        let e = parse("q.sql", "-- name: x :one\nSELECT 1;\nSELECT 2").unwrap_err();
        assert!(e.starts_with("q.sql:"), "{e}");
        let e = parse("q.sql", "-- name: x :one\nSELECT\n  1 +* FROM t").unwrap_err();
        assert!(e.starts_with("q.sql:3:"), "{e}");
        assert!(
            parse("q.sql", "SELECT 1")
                .unwrap_err()
                .contains("before the first")
        );
        assert!(
            parse("q.sql", "-- name: x :all\nSELECT 1")
                .unwrap_err()
                .contains(":all")
        );
    }

    #[test]
    fn parameters_are_named_by_their_place() {
        let n = param_names(
            "SELECT * FROM t WHERE u.email = $1 AND $2 < age AND id IN ($3) AND k = ANY($4) LIMIT $5 OFFSET $6",
        );
        let get = |i: usize| n.get(&i).map(String::as_str);
        assert_eq!(
            [get(1), get(2), get(3), get(4), get(5), get(6)],
            [
                Some("email"),
                Some("age"),
                Some("id"),
                Some("k"),
                Some("limit"),
                Some("offset")
            ]
        );
        let n = param_names("INSERT INTO t (a, \"B c\") VALUES ($2, $1) RETURNING id");
        assert_eq!(
            (n.get(&2).map(String::as_str), n.get(&1).map(String::as_str)),
            (Some("a"), Some("B c"))
        );
        let n = param_names("UPDATE t SET name = $1, pay = pay * $3 WHERE id = $2");
        assert_eq!(
            (n[&1].as_str(), n[&2].as_str(), n[&3].as_str()),
            ("name", "id", "pay")
        );
        assert!(param_names("SELECT $1::int + 1").is_empty());
    }

    #[test]
    fn names_change_case() {
        assert_eq!(camel("user_by_email"), "userByEmail");
        assert_eq!(snake("userByEmail"), "user_by_email");
        assert_eq!(constant("user_by_email"), "USER_BY_EMAIL_SQL");
    }
}
