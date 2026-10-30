//! A hand-written recursive-descent parser (Pratt-style for expressions),
//! for a PostgreSQL-like dialect (PLAN.md D4).

use crate::ast::*;
use crate::error::{Error, Result};
use crate::lexer::{Token, tokenize};
use crate::types::{DataType, Value};

/// Parses a string of `;`-separated statements.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut p = Parser::new(sql)?;
    let mut out = Vec::new();
    loop {
        while p.eat_op(";") {}
        if p.at_end() {
            return Ok(out);
        }
        out.push(p.statement()?);
        if !p.at_end() && !p.eat_op(";") {
            return Err(p.error("expected ; or end of input"));
        }
    }
}

/// The same, each statement with its text (for the audit log).
pub fn parse_with_text(sql: &str) -> Result<Vec<(Statement, String)>> {
    let mut p = Parser::new(sql)?;
    let mut out = Vec::new();
    loop {
        while p.eat_op(";") {}
        if p.at_end() {
            return Ok(out);
        }
        let (stmt, text) = p.text_of(|p| p.statement())?;
        out.push((stmt, text));
        if !p.at_end() && !p.eat_op(";") {
            return Err(p.error("expected ; or end of input"));
        }
    }
}

/// Parses one statement (a trailing `;` is allowed).
pub fn parse_one(sql: &str) -> Result<Statement> {
    let mut all = parse(sql)?;
    match all.len() {
        1 => Ok(all.remove(0)),
        0 => Err(Error::syntax("empty statement")),
        _ => Err(Error::syntax("expected one statement")),
    }
}

/// Parses an expression (for stored `DEFAULT` and `CHECK` text).
pub fn parse_expr(sql: &str) -> Result<Expr> {
    let mut p = Parser::new(sql)?;
    let e = p.expr()?;
    if !p.at_end() {
        return Err(p.error("unexpected input after the expression"));
    }
    Ok(e)
}

/// Words that cannot be a bare alias or column name.
/// Words that open `EXPLAIN`'s option list (`EXPLAIN (` then one of
/// these; else a parenthesised query).  Only `ANALYZE` and `TIMING` are
/// supported; the others are named in the error.
const EXPLAIN_OPTIONS: &[&str] = &[
    "analyze",
    "timing",
    "verbose",
    "costs",
    "buffers",
    "format",
    "summary",
    "settings",
    "wal",
    "generic_plan",
    "serialize",
    "memory",
];

const RESERVED: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "order",
    "by",
    "having",
    "limit",
    "offset",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "on",
    "as",
    "and",
    "or",
    "not",
    "null",
    "true",
    "false",
    "is",
    "in",
    "between",
    "like",
    "ilike",
    "case",
    "when",
    "then",
    "else",
    "end",
    "distinct",
    "all",
    "union",
    "values",
    "into",
    "set",
    "returning",
    "asc",
    "desc",
    "using",
    "except",
    "intersect",
    "collate",
];

struct Parser<'a> {
    sql: &'a str,
    toks: Vec<(Token, usize)>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str) -> Result<Self> {
        Ok(Self {
            sql,
            toks: tokenize(sql)?,
            pos: 0,
        })
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.toks.get(self.pos + n).map(|(t, _)| t)
    }

    fn offset(&self) -> usize {
        self.toks.get(self.pos).map_or(self.sql.len(), |(_, o)| *o)
    }

    fn error(&self, msg: &str) -> Error {
        match self.peek() {
            None => Error::syntax(format!("syntax error at end of input: {msg}")),
            Some(t) => Error::syntax(format!(
                "syntax error at or near {} (position {}): {msg}",
                show(t),
                self.offset() + 1
            )),
        }
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        self.pos += 1;
        t
    }

    fn peek_kw(&self, kw: &str) -> bool {
        self.peek().is_some_and(|t| t.is(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.peek_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_kws(&mut self, kws: &[&str]) -> bool {
        if kws
            .iter()
            .enumerate()
            .all(|(i, k)| self.peek_at(i).is_some_and(|t| t.is(k)))
        {
            self.pos += kws.len();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {}", kw.to_uppercase())))
        }
    }

    fn peek_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(o)) if *o == op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.peek_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {op}")))
        }
    }

    /// An identifier (a non-reserved word or a quoted name).
    /// `[WITH] option …` of `CREATE ROLE` / `ALTER ROLE`.
    fn role_options(&mut self) -> Result<Vec<RoleOption>> {
        self.eat_kw("with");
        let mut out = Vec::new();
        while let Some(Token::Word(w)) = self.peek() {
            let w = w.clone();
            self.pos += 1;
            out.push(match w.as_str() {
                "superuser" => RoleOption::Superuser(true),
                "nosuperuser" => RoleOption::Superuser(false),
                "login" => RoleOption::Login(true),
                "nologin" => RoleOption::Login(false),
                "inherit" => RoleOption::Inherit(true),
                "noinherit" => RoleOption::Inherit(false),
                "createrole" => RoleOption::CreateRole(true),
                "nocreaterole" => RoleOption::CreateRole(false),
                "connection" => {
                    self.expect_kw("limit")?;
                    let neg = self.eat_op("-");
                    let n = match self.next() {
                        Some(Token::Number(n)) => n
                            .parse::<u32>()
                            .map_err(|_| Error::syntax("bad connection limit"))?,
                        _ => return Err(self.error("expected a number")),
                    };
                    RoleOption::ConnectionLimit(if neg { None } else { Some(n) })
                }
                "password" | "encrypted" => {
                    if w == "encrypted" {
                        self.expect_kw("password")?;
                    }
                    if self.eat_kw("null") {
                        RoleOption::Password(None)
                    } else {
                        RoleOption::Password(Some(self.string()?))
                    }
                }
                "valid" => {
                    self.expect_kw("until")?;
                    RoleOption::ValidUntil(self.string()?)
                }
                "in" => {
                    let _ = self.eat_kw("role") || self.eat_kw("group");
                    let mut roles = vec![self.ident()?];
                    while self.eat_op(",") {
                        roles.push(self.ident()?);
                    }
                    RoleOption::InRole(roles)
                }
                _ => {
                    self.pos -= 1;
                    break;
                }
            });
        }
        Ok(out)
    }

    /// Whether `ON` comes before `TO` / `FROM` at depth 0: a `GRANT` or
    /// `REVOKE` of privileges on objects (else of roles).
    fn names_privileges(&self) -> bool {
        let mut depth = 0i32;
        for (t, _) in &self.toks[self.pos..] {
            match t {
                Token::Op("(") => depth += 1,
                Token::Op(")") => depth -= 1,
                Token::Word(w) if depth == 0 && (w == "to" || w == "from") => return false,
                Token::Word(w) if depth == 0 && w == "on" => return true,
                _ => {}
            }
        }
        false
    }

    /// `privilege [(columns)], …` or `ALL [PRIVILEGES]`.
    fn privilege_items(&mut self) -> Result<Vec<crate::privileges::Item>> {
        let mut items = Vec::new();
        loop {
            // Privilege names are often reserved words (SELECT, ALL, …).
            let word = match self.next() {
                Some(Token::Word(w)) => w,
                _ => return Err(self.error("expected a privilege")),
            };
            let privilege = if word == "all" {
                self.eat_kw("privileges");
                None
            } else {
                Some(crate::privileges::by_name(&word).ok_or_else(|| {
                    Error::syntax(format!("unrecognized privilege type \"{word}\""))
                })?)
            };
            let columns = if self.peek_op("(") {
                self.ident_list()?
            } else {
                Vec::new()
            };
            items.push(crate::privileges::Item { privilege, columns });
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(items)
    }

    /// Privileges, `ON`, and the objects.
    fn privileges_on(
        &mut self,
    ) -> Result<(Vec<crate::privileges::Item>, crate::privileges::Target)> {
        use crate::privileges::{Kind, Target};
        let items = self.privilege_items()?;
        self.expect_kw("on")?;
        let target = if self.eat_kw("all") {
            self.expect_kw("tables")?;
            self.expect_kw("in")?;
            self.expect_kw("schema")?;
            Target {
                kind: Kind::Table,
                objects: Vec::new(),
                all_in_schema: Some(self.ident()?),
            }
        } else {
            let kind = if self.eat_kw("schema") {
                Kind::Schema
            } else if self.eat_kw("function") || self.eat_kw("routine") {
                Kind::Function
            } else if self.eat_kw("type") || self.eat_kw("domain") {
                Kind::Type
            } else if self.eat_kw("database") {
                Kind::Database
            } else {
                self.eat_kw("table");
                Kind::Table
            };
            let mut objects = Vec::new();
            loop {
                objects.push(match kind {
                    Kind::Table => self.relation()?,
                    _ => self.ident()?,
                });
                // A function's argument types: optional (by name).
                if kind == Kind::Function && self.peek_op("(") {
                    let mut depth = 0;
                    while let Some(t) = self.next() {
                        match t {
                            Token::Op("(") => depth += 1,
                            Token::Op(")") => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            Target {
                kind,
                objects,
                all_in_schema: None,
            }
        };
        Ok((items, target))
    }

    /// Roles, or `PUBLIC` (as `public`).
    fn grantees(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        loop {
            self.eat_kw("group");
            out.push(self.ident()?);
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(out)
    }

    /// The privileges part of `GRANT` / `REVOKE` (after the keyword).
    fn grant_privileges(&mut self, revoke: bool, option_for: bool) -> Result<Statement> {
        let (items, target) = self.privileges_on()?;
        self.expect_kw(if revoke { "from" } else { "to" })?;
        let grantees = self.grantees()?;
        let grant_option = if revoke {
            let _ = self.eat_kw("cascade") || self.eat_kw("restrict");
            option_for
        } else {
            self.eat_kws(&["with", "grant", "option"])
        };
        Ok(Statement::Grant {
            items,
            target,
            grantees,
            grant_option,
            revoke,
        })
    }

    /// `GRANT role, … TO role, … [WITH ADMIN OPTION]`.
    fn grant(&mut self) -> Result<Statement> {
        self.expect_kw("grant")?;
        if self.names_privileges() {
            return self.grant_privileges(false, false);
        }
        let roles = self.name_list()?;
        self.expect_kw("to")?;
        let members = self.name_list()?;
        let admin_option = self.eat_kws(&["with", "admin", "option"]);
        let condition = if self.eat_kw("when") {
            self.expect_op("(")?;
            let (_, text) = self.text_of(|p| p.expr())?;
            self.expect_op(")")?;
            Some(text)
        } else {
            None
        };
        Ok(Statement::GrantRole {
            roles,
            members,
            admin_option,
            condition,
        })
    }

    /// `REVOKE [ADMIN OPTION FOR] role, … FROM role, …`.
    fn revoke(&mut self) -> Result<Statement> {
        self.expect_kw("revoke")?;
        if self.eat_kws(&["grant", "option", "for"]) {
            return self.grant_privileges(true, true);
        }
        if self.names_privileges() {
            return self.grant_privileges(true, false);
        }
        let admin_option_only = self.eat_kws(&["admin", "option", "for"]);
        let roles = self.name_list()?;
        self.expect_kw("from")?;
        let members = self.name_list()?;
        let _ = self.eat_kw("cascade") || self.eat_kw("restrict");
        Ok(Statement::RevokeRole {
            roles,
            members,
            admin_option_only,
        })
    }

    /// `CREATE POLICY name ON table [AS PERMISSIVE | RESTRICTIVE] [FOR
    /// command] [TO roles] [USING (expr)] [WITH CHECK (expr)]`.
    fn create_policy(&mut self) -> Result<Statement> {
        use crate::policies::{Command, Policy};
        let name = self.ident()?;
        self.expect_kw("on")?;
        let table = self.relation()?;
        let permissive = if self.eat_kw("as") {
            if self.eat_kw("restrictive") {
                false
            } else {
                self.expect_kw("permissive")?;
                true
            }
        } else {
            true
        };
        let command = if self.eat_kw("for") {
            match self.next() {
                Some(Token::Word(w)) => match w.as_str() {
                    "all" => Command::All,
                    "select" => Command::Select,
                    "insert" => Command::Insert,
                    "update" => Command::Update,
                    "delete" => Command::Delete,
                    _ => return Err(self.error("expected ALL, SELECT, INSERT, UPDATE or DELETE")),
                },
                _ => return Err(self.error("expected a command")),
            }
        } else {
            Command::All
        };
        let roles = if self.eat_kw("to") {
            self.grantees()?
        } else {
            vec![crate::privileges::PUBLIC.into()]
        };
        let condition = |p: &mut Self| -> Result<String> {
            p.expect_op("(")?;
            let (_, text) = p.text_of(|p| p.expr())?;
            p.expect_op(")")?;
            Ok(text)
        };
        let using = if self.eat_kw("using") {
            Some(condition(self)?)
        } else {
            None
        };
        let check = if self.eat_kws(&["with", "check"]) {
            Some(condition(self)?)
        } else {
            None
        };
        Ok(Statement::CreatePolicy(Policy {
            name,
            table,
            permissive,
            command,
            roles,
            using,
            check,
        }))
    }

    /// `CREATE ACCESS RULE name …` after its first three words.
    fn create_access_rule(&mut self) -> Result<Statement> {
        use crate::rules::{Action, Place, Rule};
        let name = self.ident()?;
        let mut rule = Rule {
            name,
            position: 0,
            roles: Vec::new(),
            from: Vec::new(),
            via: Vec::new(),
            auth: Vec::new(),
            tls: None,
            action: Action::Reject,
        };
        loop {
            if self.eat_kw("for") {
                if !self.eat_kw("all") {
                    rule.roles = self.name_list()?;
                }
            } else if self.eat_kw("from") {
                rule.from.push(self.string()?);
                while self.eat_op(",") {
                    rule.from.push(self.string()?);
                }
            } else if self.eat_kw("via") {
                rule.via = self.name_list()?;
            } else if self.eat_kw("auth") {
                rule.auth = self.name_list()?;
            } else if self.eat_kws(&["with", "tls"]) {
                rule.tls = Some(true);
            } else if self.eat_kws(&["without", "tls"]) {
                rule.tls = Some(false);
            } else {
                break;
            }
        }
        if self.eat_kw("reject") {
            rule.action = Action::Reject;
        } else {
            self.expect_kw("allow")?;
            let read_only = self.eat_kws(&["read", "only"]);
            let as_role = if self.eat_kws(&["as", "role"]) {
                Some(self.ident()?)
            } else {
                None
            };
            rule.action = Action::Allow { read_only, as_role };
        }
        let place = if self.eat_kw("before") {
            Place::Before(self.ident()?)
        } else if self.eat_kw("after") {
            Place::After(self.ident()?)
        } else {
            Place::Last
        };
        Ok(Statement::CreateAccessRule(rule, place))
    }

    /// `name, name, …` (no parentheses).
    fn name_list(&mut self) -> Result<Vec<String>> {
        let mut out = vec![self.ident()?];
        while self.eat_op(",") {
            out.push(self.ident()?);
        }
        Ok(out)
    }

    /// A string literal's text.
    fn string(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.error("expected a string")),
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Word(w)) if !RESERVED.contains(&w.as_str()) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            Some(Token::Quoted(q)) => {
                let q = q.clone();
                self.pos += 1;
                Ok(q)
            }
            _ => Err(self.error("expected a name")),
        }
    }

    /// A table to write or define: `name` or `public.name`.
    fn relation(&mut self) -> Result<String> {
        let first = self.ident()?;
        if !self.eat_op(".") {
            return Ok(first);
        }
        let name = self.ident()?;
        match first.as_str() {
            "public" => Ok(name),
            "pg_catalog" | "information_schema" | "nc_catalog" => {
                Err(Error::insufficient_privilege(format!(
                    "permission denied: \"{first}.{name}\" is a system catalog"
                )))
            }
            _ => Err(Error::invalid_schema(format!(
                "schema \"{first}\" does not exist"
            ))),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect_op("(")?;
        let mut out = vec![self.ident()?];
        while self.eat_op(",") {
            out.push(self.ident()?);
        }
        self.expect_op(")")?;
        Ok(out)
    }

    /// The source text of the tokens consumed by `f`.
    fn text_of<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<(T, String)> {
        let start = self.offset();
        let v = f(self)?;
        let end = self.offset();
        Ok((v, self.sql[start..end].trim().to_string()))
    }

    // ------------------------------------------------------------ statements

    /// `COPY t [(columns)] FROM STDIN [[WITH] (option [value], …)]`, or
    /// the older `[WITH] [CSV] [HEADER] [DELIMITER [AS] 'c'] [NULL [AS]
    /// 'n'] [QUOTE [AS] 'q'] [ESCAPE [AS] 'e']`.
    fn copy(&mut self) -> Result<Statement> {
        self.pos += 1;
        let table = self.relation()?;
        let columns = if self.peek_op("(") {
            Some(self.ident_list()?)
        } else {
            None
        };
        if self.eat_kw("to") {
            return Err(Error::feature(
                "COPY TO is not supported: read the result with a query (a cursor, or columnar batches)",
            ));
        }
        self.expect_kw("from")?;
        if !self.eat_kw("stdin") {
            return Err(Error::feature(
                "COPY FROM supports STDIN: the client sends the data",
            ));
        }
        let mut options = crate::ast::CopyOptions::default();
        let (mut delimiter, mut null) = (None, None);
        let byte = |p: &mut Self, what: &str| -> Result<u8> {
            let s = p.string()?;
            match s.as_bytes() {
                [b] if b.is_ascii() => Ok(*b),
                _ => Err(Error::feature(format!(
                    "COPY {what} must be a single one-byte character"
                ))),
            }
        };
        let with = self.eat_kw("with");
        if self.eat_op("(") {
            loop {
                let name = self.ident()?;
                match name.as_str() {
                    "format" => {
                        let f = self.ident()?;
                        options.csv = match f.as_str() {
                            "csv" => true,
                            "text" => false,
                            _ => {
                                return Err(Error::feature(format!(
                                    "COPY format \"{f}\" not recognized (text, csv)"
                                )));
                            }
                        };
                    }
                    "header" => {
                        options.header = !(self.eat_kw("false") || self.eat_kw("off"));
                        let _ = self.eat_kw("true") || self.eat_kw("on");
                    }
                    "delimiter" => delimiter = Some(byte(self, "delimiter")?),
                    "null" => null = Some(self.string()?),
                    "quote" => options.quote = byte(self, "quote")?,
                    "escape" => options.escape = byte(self, "escape")?,
                    _ => {
                        return Err(Error::feature(format!(
                            "COPY option \"{name}\" not recognized"
                        )));
                    }
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        } else {
            let _ = with;
            loop {
                if self.eat_kw("csv") {
                    options.csv = true;
                } else if self.eat_kw("header") {
                    options.header = true;
                } else if self.eat_kw("delimiter") {
                    self.eat_kw("as");
                    delimiter = Some(byte(self, "delimiter")?);
                } else if self.eat_kw("null") {
                    self.eat_kw("as");
                    null = Some(self.string()?);
                } else if self.eat_kw("quote") {
                    self.eat_kw("as");
                    options.quote = byte(self, "quote")?;
                } else if self.eat_kw("escape") {
                    self.eat_kw("as");
                    options.escape = byte(self, "escape")?;
                } else {
                    break;
                }
            }
        }
        if options.csv {
            options.delimiter = b',';
            options.null = String::new();
        }
        if let Some(d) = delimiter {
            options.delimiter = d;
        }
        if let Some(n) = null {
            options.null = n;
        }
        Ok(Statement::Copy {
            table,
            columns,
            options,
        })
    }

    fn statement(&mut self) -> Result<Statement> {
        let Some(Token::Word(w)) = self.peek() else {
            if self.peek_op("(") {
                return Ok(Statement::Query(self.query()?));
            }
            return Err(self.error("expected a statement"));
        };
        match w.as_str() {
            "select" | "values" | "with" => Ok(Statement::Query(self.query()?)),
            "create" => self.create(),
            "drop" => self.drop(),
            "alter" => self.alter(),
            "insert" => self.insert(),
            "update" => self.update(),
            "delete" => self.delete(),
            "begin" | "start" => {
                self.pos += 1;
                if !self.eat_kw("transaction") {
                    self.eat_kw("work");
                }
                Ok(Statement::Begin(self.txn_mode()?))
            }
            "commit" | "end" => {
                self.pos += 1;
                let _ = self.eat_kw("transaction") || self.eat_kw("work");
                Ok(Statement::Commit)
            }
            "rollback" | "abort" => {
                self.pos += 1;
                let _ = self.eat_kw("transaction") || self.eat_kw("work");
                Ok(Statement::Rollback)
            }
            "set" => self.set(),
            "grant" => self.grant(),
            "revoke" => self.revoke(),
            "reset" if matches!(self.peek_at(1), Some(Token::Word(w)) if w == "role") => {
                self.pos += 2;
                Ok(Statement::SetRole(None))
            }
            "reset" => {
                self.pos += 1;
                if self.eat_kw("all") {
                    return Ok(Statement::Reset(None));
                }
                let mut name = self.ident()?;
                while self.eat_op(".") {
                    name.push('.');
                    name.push_str(&self.ident()?);
                }
                Ok(Statement::Reset(Some(name)))
            }
            "show" => {
                self.pos += 1;
                if self.eat_kws(&["transaction", "isolation", "level"]) {
                    return Ok(Statement::Show("transaction_isolation".into()));
                }
                Ok(Statement::Show(self.ident()?))
            }
            "copy" => self.copy(),
            "comment" => {
                self.pos += 1;
                self.expect_kw("on")?;
                let (table, column) = if self.eat_kw("table") {
                    (self.relation()?, None)
                } else if self.eat_kw("column") {
                    // t.c or public.t.c
                    let mut parts = vec![self.ident()?];
                    while self.eat_op(".") {
                        parts.push(self.ident()?);
                    }
                    let column = parts.pop().filter(|_| !parts.is_empty());
                    let Some(column) = column else {
                        return Err(self.error("expected table.column"));
                    };
                    let table = match parts.as_slice() {
                        [t] => t.clone(),
                        [s, t] if s == "public" => t.clone(),
                        [s, _] => {
                            return Err(Error::invalid_schema(format!(
                                "schema \"{s}\" does not exist"
                            )));
                        }
                        _ => return Err(self.error("expected table.column")),
                    };
                    (table, Some(column))
                } else {
                    return Err(Error::feature("COMMENT ON supports TABLE and COLUMN"));
                };
                self.expect_kw("is")?;
                let text = if self.eat_kw("null") {
                    None
                } else {
                    Some(self.string()?)
                };
                Ok(Statement::Comment {
                    table,
                    column,
                    text,
                })
            }
            "explain" => {
                self.pos += 1;
                let options = self.explain_options()?;
                Ok(Statement::Explain(Box::new(self.statement()?), options))
            }
            "analyze" => {
                self.pos += 1;
                // `ANALYZE QUERY <statement>` (a table named `query` stays
                // a table).
                if self.peek_kw("query")
                    && matches!(self.peek_at(1), Some(Token::Word(w))
                        if ["select", "with", "values", "table", "update", "delete", "insert"]
                            .contains(&w.as_str()))
                {
                    self.pos += 1;
                    return Ok(Statement::Lint(Box::new(self.statement()?)));
                }
                let table = if self.at_end() || self.peek_op(";") {
                    None
                } else {
                    Some(self.relation()?)
                };
                Ok(Statement::Analyze(table))
            }
            _ => Err(self.error("expected a statement")),
        }
    }

    /// `ANALYZE`, or `( option [value], … )` with `ANALYZE` and `TIMING`
    /// (booleans: `true`, `false`, `on`, `off`, `1`, `0`; none is true).
    fn explain_options(&mut self) -> Result<ExplainOptions> {
        let mut o = ExplainOptions::default();
        if self.eat_kw("analyze") {
            o.analyze = true;
            return Ok(o);
        }
        let options = self.peek_op("(")
            && matches!(self.peek_at(1), Some(Token::Word(w)) if EXPLAIN_OPTIONS.contains(&w.as_str()));
        if !options {
            return Ok(o);
        }
        self.pos += 1;
        loop {
            let name = self.ident()?;
            let value = match self.peek() {
                Some(Token::Word(w)) if ["true", "on"].contains(&w.as_str()) => Some(true),
                Some(Token::Word(w)) if ["false", "off"].contains(&w.as_str()) => Some(false),
                Some(Token::Number(n)) if n == "1" => Some(true),
                Some(Token::Number(n)) if n == "0" => Some(false),
                _ => None,
            };
            if value.is_some() {
                self.pos += 1;
            }
            let value = value.unwrap_or(true);
            match name.as_str() {
                "analyze" => o.analyze = value,
                "timing" => o.timing = value,
                other => {
                    return Err(Error::feature(format!(
                        "EXPLAIN option {} is not supported",
                        other.to_uppercase()
                    )));
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(o)
    }

    fn txn_mode(&mut self) -> Result<TxnMode> {
        let mut mode = TxnMode::default();
        loop {
            if self.eat_kws(&["isolation", "level"]) {
                mode.isolation = Some(if self.eat_kw("serializable") {
                    IsolationLevel::Serializable
                } else if self.eat_kws(&["repeatable", "read"])
                    || self.eat_kws(&["read", "committed"])
                    || self.eat_kws(&["read", "uncommitted"])
                    || self.eat_kw("snapshot")
                {
                    IsolationLevel::Snapshot
                } else {
                    return Err(self.error("expected an isolation level"));
                });
            } else if self.eat_kws(&["read", "only"]) {
                mode.read_only = Some(true);
            } else if self.eat_kws(&["read", "write"]) {
                mode.read_only = Some(false);
            } else {
                return Ok(mode);
            }
            self.eat_op(",");
        }
    }

    fn set(&mut self) -> Result<Statement> {
        self.expect_kw("set")?;
        if self.eat_kw("transaction") {
            return Ok(Statement::SetTransaction(self.txn_mode()?));
        }
        if self.eat_kws(&["session", "characteristics", "as", "transaction"]) {
            let mode = self.txn_mode()?;
            return Ok(Statement::Set {
                name: "default_transaction_isolation".into(),
                value: match mode.isolation {
                    Some(IsolationLevel::Serializable) => "serializable".into(),
                    _ => "repeatable read".into(),
                },
            });
        }
        let _ = self.eat_kw("session") || self.eat_kw("local");
        if self.eat_kw("role") {
            let _ = self.eat_op("=") || self.eat_kw("to");
            let role = match self.next() {
                Some(Token::Word(w)) if w == "none" => None,
                Some(Token::Word(w) | Token::Quoted(w) | Token::Str(w)) => Some(w),
                _ => return Err(self.error("expected a role")),
            };
            return Ok(Statement::SetRole(role));
        }
        // A dotted name (`nucleus.tenant`): a custom setting.
        let mut name = self.ident()?;
        while self.eat_op(".") {
            name.push('.');
            name.push_str(&self.ident()?);
        }
        if !self.eat_op("=") {
            self.expect_kw("to")?;
        }
        let mut words = Vec::new();
        while !self.at_end() && !self.peek_op(";") {
            words.push(match self.next() {
                Some(Token::Word(w) | Token::Quoted(w) | Token::Str(w) | Token::Number(w)) => w,
                Some(Token::Op(",")) => ",".into(),
                Some(Token::Op("-")) => "-".into(),
                _ => return Err(self.error("unexpected SET value")),
            });
        }
        Ok(Statement::Set {
            name,
            value: words.join(" "),
        })
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("create")?;
        let unique = self.eat_kw("unique");
        if self.eat_kw("index") {
            let if_not_exists = self.eat_kws(&["if", "not", "exists"]);
            let name = if self.peek_kw("on") {
                None
            } else {
                Some(self.ident()?)
            };
            self.expect_kw("on")?;
            let table = self.relation()?;
            // (column [USING ordering], …)
            self.expect_op("(")?;
            let mut columns = Vec::new();
            let mut orderings = Vec::new();
            loop {
                columns.push(self.ident()?);
                orderings.push(if self.eat_kw("using") {
                    Some(self.ident()?)
                } else {
                    None
                });
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            return Ok(Statement::CreateIndex {
                name,
                table,
                columns,
                orderings,
                unique,
                if_not_exists,
            });
        }
        if unique {
            return Err(self.error("expected INDEX"));
        }
        if self.peek_kw("role") || self.peek_kw("user") {
            let user = self.eat_kw("user");
            if !user {
                self.expect_kw("role")?;
            }
            let name = self.ident()?;
            let mut options = if user {
                vec![RoleOption::Login(true)]
            } else {
                Vec::new()
            };
            options.extend(self.role_options()?);
            return Ok(Statement::CreateRole { name, options });
        }
        if self.eat_kw("policy") {
            return self.create_policy();
        }
        if self.eat_kws(&["access", "rule"]) {
            return self.create_access_rule();
        }
        if self.eat_kw("token") {
            let name = self.ident()?;
            let (mut role, mut read_only, mut tables, mut valid_until) = (None, false, None, None);
            let mut budget = crate::tokens::Budgets::default();
            let number = |p: &mut Self| -> Result<u64> {
                match p.next() {
                    Some(Token::Number(n)) => {
                        n.parse().map_err(|_| Error::syntax("a whole number"))
                    }
                    _ => Err(p.error("expected a number")),
                }
            };
            loop {
                if self.eat_kw("for") {
                    role = Some(self.ident()?);
                } else if self.eat_kws(&["read", "only"]) {
                    read_only = true;
                } else if self.eat_kw("on") {
                    self.eat_kw("table");
                    let mut t = vec![self.relation()?];
                    while self.eat_op(",") {
                        t.push(self.relation()?);
                    }
                    tables = Some(t);
                } else if self.eat_kws(&["valid", "until"]) {
                    valid_until = Some(self.string()?);
                } else if self.eat_kw("rows") {
                    budget.rows = Some(number(self)?);
                } else if self.eat_kw("scan") {
                    budget.scan = Some(number(self)?);
                } else if self.eat_kw("timeout") {
                    budget.timeout = Some(crate::dbsettings::duration(&self.string()?)?);
                } else if self.eat_kw("rate") {
                    budget.rate = Some(number(self)? as u32);
                    self.expect_kw("per")?;
                    self.expect_kw("minute")?;
                } else {
                    break;
                }
            }
            return Ok(Statement::CreateToken {
                name,
                role,
                read_only,
                tables,
                valid_until,
                budget,
            });
        }
        if self.eat_kw("type") {
            let name = self.ident()?;
            self.expect_kw("as")?;
            self.expect_kw("enum")?;
            self.expect_op("(")?;
            let mut labels = Vec::new();
            if !self.peek_op(")") {
                loop {
                    labels.push(self.string()?);
                    if !self.eat_op(",") {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            return Ok(Statement::CreateEnum { name, labels });
        }
        self.expect_kw("table")?;
        let if_not_exists = self.eat_kws(&["if", "not", "exists"]);
        let name = self.relation()?;
        if self.eat_kws(&["partition", "of"]) {
            let parent = self.relation()?;
            let bound = self.partition_bound()?;
            let options = self.table_options()?;
            return Ok(Statement::CreatePartition {
                name,
                parent,
                bound,
                options,
                if_not_exists,
            });
        }
        self.expect_op("(")?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.peek_kw("primary")
                || self.peek_kw("unique")
                || self.peek_kw("check")
                || self.peek_kw("constraint")
                || self.peek_kw("foreign")
            {
                constraints.push(self.table_constraint()?);
            } else {
                columns.push(self.column_spec()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        let partition_by = if self.eat_kws(&["partition", "by"]) {
            let kind = self.ident()?;
            self.expect_op("(")?;
            let cols = self.name_list()?;
            self.expect_op(")")?;
            Some((kind, cols))
        } else {
            None
        };
        let options = self.table_options()?;
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
            options,
            partition_by,
        })
    }

    /// `[WITH (name = value, …)]`.
    fn table_options(&mut self) -> Result<Vec<(String, String)>> {
        let mut options = Vec::new();
        if self.eat_kw("with") {
            self.expect_op("(")?;
            loop {
                let key = self.ident()?;
                self.expect_op("=")?;
                let value = match self.peek() {
                    Some(Token::Str(_)) => self.string()?,
                    Some(Token::Number(n)) => {
                        let n = n.clone();
                        self.pos += 1;
                        n
                    }
                    _ => self.ident()?,
                };
                options.push((key, value));
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        Ok(options)
    }

    /// `FOR VALUES FROM (…) TO (…) | IN (…) | WITH (MODULUS m, REMAINDER r)`
    /// or `DEFAULT`.
    fn partition_bound(&mut self) -> Result<crate::ast::PartitionBoundSpec> {
        use crate::ast::PartitionBoundSpec as B;
        if self.eat_kw("default") {
            return Ok(B::Default);
        }
        self.expect_kw("for")?;
        self.expect_kw("values")?;
        let open_list = |p: &mut Self| -> Result<Vec<Option<Expr>>> {
            p.expect_op("(")?;
            let mut out = Vec::new();
            loop {
                if p.eat_kw("minvalue") || p.eat_kw("maxvalue") {
                    out.push(None);
                } else {
                    out.push(Some(p.expr()?));
                }
                if !p.eat_op(",") {
                    break;
                }
            }
            p.expect_op(")")?;
            Ok(out)
        };
        if self.eat_kw("from") {
            let from = open_list(self)?;
            self.expect_kw("to")?;
            let to = open_list(self)?;
            return Ok(B::Range { from, to });
        }
        if self.eat_kw("in") {
            self.expect_op("(")?;
            let mut vals = vec![self.expr()?];
            while self.eat_op(",") {
                vals.push(self.expr()?);
            }
            self.expect_op(")")?;
            return Ok(B::List(vals));
        }
        self.expect_kw("with")?;
        self.expect_op("(")?;
        let mut modulus = None;
        let mut remainder = None;
        loop {
            let key = self.ident()?;
            let n: u32 = match self.next() {
                Some(Token::Number(n)) => n
                    .parse()
                    .map_err(|_| self.error("expected a whole number"))?,
                _ => return Err(self.error("expected a whole number")),
            };
            match key.as_str() {
                "modulus" => modulus = Some(n),
                "remainder" => remainder = Some(n),
                _ => return Err(self.error("expected MODULUS or REMAINDER")),
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        match (modulus, remainder) {
            (Some(modulus), Some(remainder)) => Ok(B::Hash { modulus, remainder }),
            _ => Err(self.error("a hash partition needs MODULUS and REMAINDER")),
        }
    }

    fn table_constraint(&mut self) -> Result<TableConstraint> {
        let name = if self.eat_kw("constraint") {
            Some(self.ident()?)
        } else {
            None
        };
        if self.eat_kws(&["primary", "key"]) {
            Ok(TableConstraint::PrimaryKey(self.ident_list()?))
        } else if self.eat_kw("unique") {
            Ok(TableConstraint::Unique(name, self.ident_list()?))
        } else if self.eat_kw("check") {
            self.expect_op("(")?;
            let (_, text) = self.text_of(|p| p.expr())?;
            self.expect_op(")")?;
            Ok(TableConstraint::Check(name, text))
        } else if self.eat_kws(&["foreign", "key"]) {
            let columns = self.ident_list()?;
            self.expect_kw("references")?;
            Ok(TableConstraint::ForeignKey {
                name,
                columns,
                references: self.references()?,
            })
        } else {
            Err(self.error("expected PRIMARY KEY, UNIQUE, CHECK or FOREIGN KEY"))
        }
    }

    /// After `REFERENCES`: `table [(columns)] [MATCH SIMPLE] [ON DELETE a]
    /// [ON UPDATE a] [NOT DEFERRABLE] [INITIALLY IMMEDIATE]`.
    fn references(&mut self) -> Result<References> {
        let table = self.relation()?;
        let columns = if self.peek_op("(") {
            self.ident_list()?
        } else {
            Vec::new()
        };
        let mut r = References {
            table,
            columns,
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
        };
        loop {
            if self.eat_kws(&["match", "simple"])
                || self.eat_kws(&["not", "deferrable"])
                || self.eat_kws(&["initially", "immediate"])
            {
            } else if self.peek_kw("match") {
                return Err(Error::feature(
                    "only MATCH SIMPLE foreign keys are supported",
                ));
            } else if self.peek_kw("deferrable") || self.peek_kws(&["initially", "deferred"]) {
                return Err(Error::feature("deferrable constraints are not supported"));
            } else if self.eat_kws(&["on", "delete"]) {
                r.on_delete = self.fk_action()?;
            } else if self.eat_kws(&["on", "update"]) {
                r.on_update = self.fk_action()?;
            } else {
                return Ok(r);
            }
        }
    }

    fn fk_action(&mut self) -> Result<FkAction> {
        Ok(if self.eat_kws(&["no", "action"]) {
            FkAction::NoAction
        } else if self.eat_kw("restrict") {
            FkAction::Restrict
        } else if self.eat_kw("cascade") {
            FkAction::Cascade
        } else if self.eat_kws(&["set", "null"]) {
            FkAction::SetNull
        } else if self.eat_kws(&["set", "default"]) {
            FkAction::SetDefault
        } else {
            return Err(
                self.error("expected NO ACTION, RESTRICT, CASCADE, SET NULL or SET DEFAULT")
            );
        })
    }

    fn peek_kws(&self, kws: &[&str]) -> bool {
        kws.iter()
            .enumerate()
            .all(|(i, k)| self.peek_at(i).is_some_and(|t| t.is(k)))
    }

    fn data_type(&mut self) -> Result<(SqlType, TypeMod)> {
        // `schema.` before a type name (the binder checks the schema).
        let mut schema = None;
        if let Some(Token::Word(w)) = self.peek()
            && self.peek_at(1) == Some(&Token::Op("."))
        {
            schema = Some(w.clone());
            self.pos += 2;
        }
        let mut words: Vec<String> = Vec::new();
        while let Some(Token::Word(w)) = self.peek() {
            let candidate: Vec<&str> = words
                .iter()
                .map(String::as_str)
                .chain([w.as_str()])
                .collect();
            if !DataType::is_prefix(&candidate) {
                break;
            }
            words.push(w.clone());
            self.pos += 1;
        }
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        if words.is_empty()
            && let Some(Token::Word(w) | Token::Quoted(w)) = self.peek()
        {
            // Not a built-in name: one the registry may know.
            let name = w.clone();
            self.pos += 1;
            if self.peek_op("(") || self.peek_op("[") {
                return Err(Error::feature(format!(
                    "modifiers and arrays of type {name} are not supported yet"
                )));
            }
            return Ok((SqlType::Named { schema, name }, TypeMod::default()));
        }
        let ty = DataType::parse(&refs).ok_or_else(|| self.error("expected a type"))?;
        let (ty, typmod) = self.builtin_rest(ty)?;
        Ok((SqlType::Builtin { schema, ty }, typmod))
    }

    /// The rest of a built-in type after its name: modifiers, `WITH TIME
    /// ZONE`, `[]`.
    fn builtin_rest(&mut self, ty: DataType) -> Result<(DataType, TypeMod)> {
        let mut typmod = TypeMod::default();
        if self.eat_op("(") {
            let number = |p: &mut Self| match p.next() {
                Some(Token::Number(n)) => n
                    .parse::<u32>()
                    .map_err(|_| Error::syntax("bad type modifier")),
                _ => Err(p.error("expected a number")),
            };
            let n = number(self)?;
            let m = if self.eat_op(",") {
                Some(number(self)?)
            } else {
                None
            };
            self.expect_op(")")?;
            match ty {
                DataType::Text | DataType::Char => typmod.max_len = Some(n),
                DataType::Numeric => {
                    let s = m.unwrap_or(0);
                    if !(1..=crate::decimal::MAX_DIGITS).contains(&n) || s > n {
                        return Err(Error::invalid_parameter(format!(
                            "NUMERIC precision {n} must be between 1 and {}, and scale {s} at most the precision",
                            crate::decimal::MAX_DIGITS
                        )));
                    }
                    typmod.numeric = Some((n, s));
                }
                // Fractional-second precision of timestamps: accepted,
                // microseconds are kept.
                _ => {}
            }
        }
        // `CHAR` alone is `CHAR(1)`.
        if ty == DataType::Char && typmod.max_len.is_none() {
            typmod.max_len = Some(1);
        }
        // `TIMESTAMP(p) WITH TIME ZONE`.
        let mut ty = ty;
        if ty == DataType::Timestamp && self.eat_kws(&["with", "time", "zone"]) {
            ty = DataType::TimestampTz;
        } else if ty == DataType::Timestamp {
            self.eat_kws(&["without", "time", "zone"]);
        } else if ty == DataType::Time && self.eat_kws(&["with", "time", "zone"]) {
            return Err(Error::feature(
                "type time with time zone is not supported; use time or timestamptz",
            ));
        } else if ty == DataType::Time {
            self.eat_kws(&["without", "time", "zone"]);
        }
        // `type[]` (any number of brackets, sizes ignored: one dimension).
        let mut array = false;
        while self.peek_op("[") {
            self.pos += 1;
            if let Some(Token::Number(_)) = self.peek() {
                self.pos += 1;
            }
            self.expect_op("]")?;
            array = true;
        }
        if array {
            ty = ty
                .array_of()
                .ok_or_else(|| Error::feature("arrays of arrays are not supported"))?;
        }
        Ok((ty, typmod))
    }

    fn column_spec(&mut self) -> Result<ColumnSpec> {
        let name = self.ident()?;
        let (ty, typmod) = self.data_type()?;
        let mut c = ColumnSpec {
            name,
            ty,
            max_len: typmod.max_len,
            numeric: typmod.numeric,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
            check: None,
            references: None,
        };
        let mut constraint_name = None;
        loop {
            if self.eat_kws(&["not", "null"]) {
                c.not_null = true;
            } else if self.eat_kw("null") {
            } else if self.eat_kws(&["primary", "key"]) {
                c.primary_key = true;
                c.not_null = true;
            } else if self.eat_kw("unique") {
                c.unique = true;
            } else if self.eat_kw("default") {
                let (_, text) = self.text_of(|p| p.expr_prec(PREC_ADD))?;
                c.default = Some(text);
            } else if self.eat_kw("check") {
                self.expect_op("(")?;
                let (_, text) = self.text_of(|p| p.expr())?;
                self.expect_op(")")?;
                c.check = Some(text);
            } else if self.eat_kw("constraint") {
                constraint_name = Some(self.ident()?);
            } else if self.eat_kw("references") {
                c.references = Some((constraint_name.take(), self.references()?));
            } else {
                return Ok(c);
            }
        }
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_kw("drop")?;
        if self.eat_kw("index") {
            let if_exists = self.eat_kws(&["if", "exists"]);
            return Ok(Statement::DropIndex {
                name: self.ident()?,
                if_exists,
            });
        }
        if self.eat_kw("policy") {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let name = self.ident()?;
            self.expect_kw("on")?;
            let table = self.relation()?;
            return Ok(Statement::DropPolicy {
                name,
                table,
                if_exists,
            });
        }
        if self.eat_kws(&["access", "rule"]) {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let name = self.ident()?;
            return Ok(Statement::DropAccessRule { name, if_exists });
        }
        if self.eat_kw("token") {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let name = self.ident()?;
            return Ok(Statement::DropToken { name, if_exists });
        }
        if self.eat_kw("role") || self.eat_kw("user") {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let mut names = vec![self.ident()?];
            while self.eat_op(",") {
                names.push(self.ident()?);
            }
            return Ok(Statement::DropRole { names, if_exists });
        }
        if self.eat_kw("type") {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let name = self.ident()?;
            if self.eat_kw("cascade") {
                return Err(Error::feature("DROP TYPE … CASCADE is not supported"));
            }
            self.eat_kw("restrict");
            return Ok(Statement::DropType { name, if_exists });
        }
        self.expect_kw("table")?;
        let if_exists = self.eat_kws(&["if", "exists"]);
        let mut names = vec![self.relation()?];
        while self.eat_op(",") {
            names.push(self.relation()?);
        }
        let cascade = self.eat_kw("cascade");
        if !cascade {
            self.eat_kw("restrict");
        }
        Ok(Statement::DropTable {
            names,
            if_exists,
            cascade,
        })
    }

    /// `ALTER TYPE name ADD | ALTER ORDERING o BY mapping`, `… DROP
    /// ORDERING [IF EXISTS] o [CASCADE | RESTRICT]`.
    fn alter_type(&mut self) -> Result<Statement> {
        let type_name = self.ident()?;
        let verb = self.ident()?;
        // An enum's labels.
        if verb == "add" && self.eat_kw("value") {
            let if_not_exists = self.eat_kws(&["if", "not", "exists"]);
            let label = self.string()?;
            let place = if self.eat_kw("before") {
                crate::enums::Place::Before(self.string()?)
            } else if self.eat_kw("after") {
                crate::enums::Place::After(self.string()?)
            } else {
                crate::enums::Place::Last
            };
            return Ok(Statement::AlterType {
                type_name,
                action: TypeAction::AddValue {
                    label,
                    if_not_exists,
                    place,
                },
            });
        }
        if verb == "rename" && self.eat_kw("value") {
            let from = self.string()?;
            self.expect_kw("to")?;
            let to = self.string()?;
            return Ok(Statement::AlterType {
                type_name,
                action: TypeAction::RenameValue { from, to },
            });
        }
        if verb == "upgrade" {
            return Ok(Statement::AlterType {
                type_name,
                action: TypeAction::Upgrade,
            });
        }
        let unsupported = || {
            Error::feature(
                "ALTER TYPE supports ADD, ALTER and DROP ORDERING, ADD and DROP OPERATOR and CAST",
            )
        };
        if self.eat_kw("operator") {
            let if_exists = verb == "drop" && self.eat_kws(&["if", "exists"]);
            let symbol = match self.next() {
                Some(Token::UserOp(s)) => s,
                Some(Token::Op(o)) => {
                    return Err(Error::invalid_parameter(format!(
                        "{o} is the grammar's own operator; a type can be taught others (<->, @>, …)"
                    )));
                }
                _ => return Err(self.error("expected an operator")),
            };
            self.expect_op("(")?;
            let (left, _) = self.data_type()?;
            self.expect_op(",")?;
            let (right, _) = self.data_type()?;
            self.expect_op(")")?;
            let action = match verb.as_str() {
                "add" => {
                    if self.eat_kw("returns") {
                        self.data_type()?;
                    }
                    self.expect_kw("as")?;
                    TypeAction::AddOperator {
                        symbol,
                        left,
                        right,
                        function: self.ident()?,
                    }
                }
                "drop" => TypeAction::DropOperator {
                    symbol,
                    left,
                    right,
                    if_exists,
                },
                _ => return Err(unsupported()),
            };
            return Ok(Statement::AlterType { type_name, action });
        }
        if self.eat_kw("cast") {
            let if_exists = verb == "drop" && self.eat_kws(&["if", "exists"]);
            self.expect_kw("to")?;
            let (to, _) = self.data_type()?;
            let action = match verb.as_str() {
                "add" => {
                    self.expect_kw("as")?;
                    let function = self.ident()?;
                    let context = if self.eat_kw("implicit") {
                        2
                    } else if self.eat_kw("assignment") {
                        1
                    } else {
                        self.eat_kw("explicit");
                        0
                    };
                    TypeAction::AddCast {
                        to,
                        function,
                        context,
                    }
                }
                "drop" => TypeAction::DropCast { to, if_exists },
                _ => return Err(unsupported()),
            };
            return Ok(Statement::AlterType { type_name, action });
        }
        if !self.eat_kw("ordering") {
            return Err(unsupported());
        }
        let action = match verb.as_str() {
            "add" | "alter" => {
                let name = self.ident()?;
                self.expect_kw("by")?;
                let (_, mapping) = self.text_of(|p| p.expr())?;
                if verb == "add" {
                    TypeAction::AddOrdering { name, mapping }
                } else {
                    TypeAction::AlterOrdering { name, mapping }
                }
            }
            "drop" => {
                let if_exists = self.eat_kws(&["if", "exists"]);
                let name = self.ident()?;
                let cascade = self.eat_kw("cascade");
                if !cascade {
                    self.eat_kw("restrict");
                }
                TypeAction::DropOrdering {
                    name,
                    if_exists,
                    cascade,
                }
            }
            _ => {
                return Err(Error::feature(
                    "ALTER TYPE supports ADD, ALTER and DROP ORDERING",
                ));
            }
        };
        Ok(Statement::AlterType { type_name, action })
    }

    fn alter(&mut self) -> Result<Statement> {
        self.expect_kw("alter")?;
        if self.eat_kw("type") {
            return self.alter_type();
        }
        if self.eat_kw("database") {
            let database = self.ident()?;
            if self.eat_kw("reset") {
                let name = self.ident()?;
                return Ok(Statement::AlterDatabaseSet {
                    database,
                    name,
                    value: None,
                });
            }
            self.expect_kw("set")?;
            let name = self.ident()?;
            if !self.eat_op("=") {
                self.expect_kw("to")?;
            }
            let value = match self.next() {
                Some(Token::Word(w) | Token::Str(w) | Token::Number(w)) => w,
                _ => return Err(self.error("expected a value")),
            };
            return Ok(Statement::AlterDatabaseSet {
                database,
                name,
                value: Some(value),
            });
        }
        if self.eat_kws(&["default", "privileges"]) {
            let for_role = if self.eat_kw("for") {
                let _ = self.eat_kw("role") || self.eat_kw("user");
                Some(self.ident()?)
            } else {
                None
            };
            if self.eat_kws(&["in", "schema"]) {
                let s = self.ident()?;
                if s != "public" {
                    return Err(Error::invalid_schema(format!(
                        "schema \"{s}\" does not exist"
                    )));
                }
            }
            let revoke = self.eat_kw("revoke");
            if !revoke {
                self.expect_kw("grant")?;
            }
            let option_for = revoke && self.eat_kws(&["grant", "option", "for"]);
            let items = self.privilege_items()?;
            self.expect_kw("on")?;
            if !self.eat_kw("tables") {
                return Err(Error::feature("default privileges are for TABLES"));
            }
            let privileges = items
                .iter()
                .try_fold(0u32, |acc, i| i.privilege.map(|p| acc | p));
            self.expect_kw(if revoke { "from" } else { "to" })?;
            let grantees = self.grantees()?;
            let grant_option = if revoke {
                option_for
            } else {
                self.eat_kws(&["with", "grant", "option"])
            };
            return Ok(Statement::AlterDefaultPrivileges {
                for_role,
                privileges,
                grantees,
                grant_option,
                revoke,
            });
        }
        if self.eat_kw("role") || self.eat_kw("user") {
            let name = self.ident()?;
            if self.eat_kws(&["rename", "to"]) {
                return Ok(Statement::RenameRole {
                    name,
                    to: self.ident()?,
                });
            }
            return Ok(Statement::AlterRole {
                name,
                options: self.role_options()?,
            });
        }
        self.expect_kw("table")?;
        let table = self.relation()?;
        for (words, enable, force) in [
            (
                &["enable", "row", "level", "security"][..],
                Some(true),
                None,
            ),
            (
                &["disable", "row", "level", "security"][..],
                Some(false),
                None,
            ),
            (&["force", "row", "level", "security"][..], None, Some(true)),
            (
                &["no", "force", "row", "level", "security"][..],
                None,
                Some(false),
            ),
        ] {
            if self.eat_kws(words) {
                return Ok(Statement::SetRowSecurity {
                    table,
                    enable,
                    force,
                });
            }
        }
        if self.eat_kws(&["attach", "partition"]) {
            let partition = self.relation()?;
            let bound = self.partition_bound()?;
            return Ok(Statement::AttachPartition {
                table,
                partition,
                bound,
            });
        }
        for (words, drop) in [
            (&["detach", "partition"][..], false),
            (&["drop", "partition"][..], true),
        ] {
            if self.eat_kws(words) {
                let partition = self.relation()?;
                return Ok(Statement::DetachPartition {
                    table,
                    partition,
                    drop,
                });
            }
        }
        if self.eat_kws(&["set", "storage", "policy"]) {
            if self.eat_kw("none") {
                return Ok(Statement::SetStoragePolicy {
                    table,
                    policy: None,
                });
            }
            self.expect_op("(")?;
            let mut kinds = vec![self.ident()?];
            while self.eat_op(",") {
                kinds.push(self.ident()?);
            }
            self.expect_kw("after")?;
            let after = self.expr()?;
            self.expect_op(")")?;
            return Ok(Statement::SetStoragePolicy {
                table,
                policy: Some((kinds, after)),
            });
        }
        if self.eat_kws(&["set", "storage"]) {
            let mut kinds = vec![self.ident()?];
            while self.eat_op(",") {
                kinds.push(self.ident()?);
            }
            let columns = if self.eat_op("(") {
                let cols = self.name_list()?;
                self.expect_op(")")?;
                Some(cols)
            } else {
                None
            };
            return Ok(Statement::SetStorage {
                table,
                kinds,
                columns,
            });
        }
        if self.eat_kws(&["drop", "constraint"]) {
            let if_exists = self.eat_kws(&["if", "exists"]);
            let name = self.ident()?;
            let _ = self.eat_kw("restrict") || self.eat_kw("cascade");
            return Ok(Statement::DropConstraint {
                table,
                name,
                if_exists,
            });
        }
        if self.eat_kw("alter") {
            // ALTER [COLUMN] c [SET DATA] TYPE t [USING expr]
            self.eat_kw("column");
            let column = self.ident()?;
            let _ = self.eat_kws(&["set", "data"]);
            self.expect_kw("type")?;
            let (ty, typmod) = self.data_type()?;
            let using = if self.eat_kw("using") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Statement::AlterColumnType {
                table,
                column,
                ty,
                typmod,
                using,
            });
        }
        if !self.eat_kw("add") {
            return Err(Error::feature(
                "ALTER TABLE supports ADD COLUMN, ADD CONSTRAINT, DROP CONSTRAINT and ALTER COLUMN … TYPE",
            ));
        }
        if ["constraint", "primary", "unique", "check", "foreign"]
            .iter()
            .any(|k| self.peek_kw(k))
        {
            return Ok(Statement::AddConstraint {
                table,
                constraint: self.table_constraint()?,
            });
        }
        self.eat_kw("column");
        Ok(Statement::AddColumn {
            table,
            column: self.column_spec()?,
        })
    }

    fn returning(&mut self) -> Result<Vec<SelectItem>> {
        if self.eat_kw("returning") {
            self.select_items()
        } else {
            Ok(Vec::new())
        }
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_kw("insert")?;
        self.expect_kw("into")?;
        let table = self.relation()?;
        let columns = if self.peek_op("(") && !matches!(self.peek_at(1), Some(t) if t.is("select"))
        {
            Some(self.ident_list()?)
        } else {
            None
        };
        let source = if self.eat_kws(&["default", "values"]) {
            Query {
                with: None,
                body: SetExpr::Values(vec![vec![]]),
                order_by: vec![],
                limit: None,
                offset: None,
            }
        } else {
            self.query()?
        };
        Ok(Statement::Insert {
            table,
            columns,
            source,
            returning: self.returning()?,
        })
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_kw("update")?;
        let table = self.relation()?;
        self.expect_kw("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            self.expect_op("=")?;
            assignments.push((col, self.expr()?));
            if !self.eat_op(",") {
                break;
            }
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::Update {
            table,
            assignments,
            filter,
            returning: self.returning()?,
        })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let table = self.relation()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::Delete {
            table,
            filter,
            returning: self.returning()?,
        })
    }

    // --------------------------------------------------------------- queries

    /// `ALL` / `DISTINCT` after a set operator: whether `ALL`.
    fn set_quantifier(&mut self) -> bool {
        let all = self.eat_kw("all");
        if !all {
            self.eat_kw("distinct");
        }
        all
    }

    fn with_clause(&mut self) -> Result<Option<With>> {
        if !self.eat_kw("with") {
            return Ok(None);
        }
        let recursive = self.eat_kw("recursive");
        let mut ctes = Vec::new();
        loop {
            let name = self.ident()?;
            let columns = self.column_aliases()?;
            self.expect_kw("as")?;
            // Planner hints; every CTE is materialized here.
            let _ = self.eat_kw("materialized") || self.eat_kws(&["not", "materialized"]);
            self.expect_op("(")?;
            let query = self.query()?;
            self.expect_op(")")?;
            ctes.push(Cte {
                name,
                columns,
                query: Box::new(query),
            });
            if !self.eat_op(",") {
                return Ok(Some(With { recursive, ctes }));
            }
        }
    }

    /// Whether a query starts here (inside parentheses).
    fn at_query(&self) -> bool {
        self.peek_kw("select") || self.peek_kw("values") || self.peek_kw("with")
    }

    fn query(&mut self) -> Result<Query> {
        let with = self.with_clause()?;
        // INTERSECT binds tighter than UNION and EXCEPT (as in PostgreSQL).
        let mut body = self.intersect_term()?;
        loop {
            let op = if self.eat_kw("union") {
                SetOp::Union
            } else if self.eat_kw("except") {
                SetOp::Except
            } else {
                break;
            };
            let all = self.set_quantifier();
            let right = self.intersect_term()?;
            body = SetExpr::SetOp {
                op,
                left: Box::new(body),
                right: Box::new(right),
                all,
            };
        }
        let order_by = if self.eat_kws(&["order", "by"]) {
            self.order_items()?
        } else {
            Vec::new()
        };
        let (mut limit, mut offset) = (None, None);
        for _ in 0..2 {
            if self.eat_kw("limit") {
                limit = if self.eat_kw("all") {
                    None
                } else {
                    Some(self.expr()?)
                };
            } else if self.eat_kw("offset") {
                offset = Some(self.expr()?);
                let _ = self.eat_kw("rows") || self.eat_kw("row");
            }
        }
        Ok(Query {
            with,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn intersect_term(&mut self) -> Result<SetExpr> {
        let mut body = self.set_term()?;
        while self.eat_kw("intersect") {
            let all = self.set_quantifier();
            let right = self.set_term()?;
            body = SetExpr::SetOp {
                op: SetOp::Intersect,
                left: Box::new(body),
                right: Box::new(right),
                all,
            };
        }
        Ok(body)
    }

    fn set_term(&mut self) -> Result<SetExpr> {
        if self.eat_op("(") {
            let q = self.query()?;
            self.expect_op(")")?;
            if q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none() {
                return Ok(q.body);
            }
            return Err(Error::feature(
                "ORDER BY / LIMIT inside a parenthesised set operation member",
            ));
        }
        if self.eat_kw("values") {
            let mut rows = Vec::new();
            loop {
                self.expect_op("(")?;
                let mut row = vec![self.expr()?];
                while self.eat_op(",") {
                    row.push(self.expr()?);
                }
                self.expect_op(")")?;
                rows.push(row);
                if !self.eat_op(",") {
                    break;
                }
            }
            return Ok(SetExpr::Values(rows));
        }
        self.expect_kw("select")?;
        let distinct = self.eat_kw("distinct");
        if !distinct {
            self.eat_kw("all");
        }
        let items = self.select_items()?;
        let mut from = Vec::new();
        if self.eat_kw("from") {
            from.push(self.table_ref()?);
            while self.eat_op(",") {
                from.push(self.table_ref()?);
            }
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.eat_kws(&["group", "by"]) {
            group_by.push(self.expr()?);
            while self.eat_op(",") {
                group_by.push(self.expr()?);
            }
        }
        let having = if self.eat_kw("having") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(SetExpr::Select(Box::new(Select {
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
        })))
    }

    fn select_items(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            if self.eat_op("*") {
                items.push(SelectItem::Wildcard);
            } else if matches!(self.peek(), Some(Token::Word(_) | Token::Quoted(_)))
                && matches!(self.peek_at(1), Some(Token::Op(".")))
                && matches!(self.peek_at(2), Some(Token::Op("*")))
            {
                let t = self.ident()?;
                self.pos += 2;
                items.push(SelectItem::QualifiedWildcard(t));
            } else {
                let expr = self.expr()?;
                let alias = self.alias()?;
                items.push(SelectItem::Expr { expr, alias });
            }
            if !self.eat_op(",") {
                return Ok(items);
            }
        }
    }

    fn alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("as") {
            return Ok(Some(self.ident()?));
        }
        match self.peek() {
            Some(Token::Word(w)) if !RESERVED.contains(&w.as_str()) => Ok(Some(self.ident()?)),
            Some(Token::Quoted(_)) => Ok(Some(self.ident()?)),
            _ => Ok(None),
        }
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        let mut left = self.table_primary()?;
        loop {
            let kind = if self.eat_kw("join") || self.eat_kws(&["inner", "join"]) {
                JoinKind::Inner
            } else if self.eat_kws(&["left", "join"]) || self.eat_kws(&["left", "outer", "join"]) {
                JoinKind::Left
            } else if self.eat_kws(&["right", "join"]) || self.eat_kws(&["right", "outer", "join"])
            {
                JoinKind::Right
            } else if self.eat_kws(&["cross", "join"]) {
                let right = self.table_primary()?;
                left = TableRef::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind: JoinKind::Inner,
                    on: None,
                    using: vec![],
                };
                continue;
            } else if self.eat_kws(&["full", "join"]) || self.eat_kws(&["full", "outer", "join"]) {
                JoinKind::Full
            } else {
                return Ok(left);
            };
            let right = self.table_primary()?;
            let (on, using) = if self.eat_kw("on") {
                (Some(self.expr()?), vec![])
            } else if self.eat_kw("using") {
                (None, self.ident_list()?)
            } else {
                return Err(self.error("expected ON or USING"));
            };
            left = TableRef::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
                using,
            };
        }
    }

    fn table_primary(&mut self) -> Result<TableRef> {
        if self.eat_op("(") {
            if self.at_query() {
                let query = self.query()?;
                self.expect_op(")")?;
                let alias = self
                    .alias()?
                    .ok_or_else(|| Error::syntax("subquery in FROM must have an alias"))?;
                let columns = self.column_aliases()?;
                return Ok(TableRef::Subquery {
                    query: Box::new(query),
                    alias,
                    columns,
                });
            }
            let t = self.table_ref()?;
            self.expect_op(")")?;
            return Ok(t);
        }
        let mut schema = None;
        let mut name = self.ident()?;
        if self.eat_op(".") {
            schema = Some(name);
            name = self.ident()?;
        }
        if self.eat_op("(") {
            let mut args = Vec::new();
            if !self.peek_op(")") {
                args.push(self.expr()?);
                while self.eat_op(",") {
                    args.push(self.expr()?);
                }
            }
            self.expect_op(")")?;
            let ordinality = self.eat_kws(&["with", "ordinality"]);
            let alias = self.alias()?;
            let columns = if alias.is_some() {
                self.column_aliases()?
            } else {
                Vec::new()
            };
            return Ok(TableRef::Function {
                name,
                args,
                ordinality,
                alias,
                columns,
            });
        }
        let alias = self.alias()?;
        let columns = if alias.is_some() {
            self.column_aliases()?
        } else {
            Vec::new()
        };
        Ok(TableRef::Table {
            schema,
            name,
            alias,
            columns,
        })
    }

    /// `(a, b, …)` after a table alias, if present.
    fn column_aliases(&mut self) -> Result<Vec<String>> {
        if self.peek_op("(") {
            self.ident_list()
        } else {
            Ok(Vec::new())
        }
    }

    // ----------------------------------------------------------- expressions

    fn expr(&mut self) -> Result<Expr> {
        self.expr_prec(0)
    }

    fn expr_prec(&mut self, min: u8) -> Result<Expr> {
        let mut left = self.prefix()?;
        loop {
            let Some((prec, kind)) = self.infix() else {
                return Ok(left);
            };
            if prec < min {
                return Ok(left);
            }
            left = self.apply_infix(left, prec, kind)?;
        }
    }

    fn prefix(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(self.expr_prec(PREC_NOT)?),
            });
        }
        if self.eat_op("-") {
            let e = self.expr_prec(PREC_UNARY)?;
            // Fold negative literals so i64::MIN parses.
            return Ok(match e {
                Expr::Literal(Value::Int(i)) => Expr::Literal(Value::Int(i.wrapping_neg())),
                Expr::Literal(Value::Float(f)) => Expr::Literal(Value::Float(-f)),
                e => Expr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(e),
                },
            });
        }
        if self.eat_op("+") {
            return Ok(Expr::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(self.expr_prec(PREC_UNARY)?),
            });
        }
        let e = self.primary()?;
        self.postfix(e)
    }

    fn postfix(&mut self, mut e: Expr) -> Result<Expr> {
        loop {
            if self.eat_op("::") {
                e = self.cast_to(e)?;
            } else if self.eat_op("[") {
                let index = self.expr()?;
                self.expect_op("]")?;
                e = Expr::Subscript {
                    expr: Box::new(e),
                    index: Box::new(index),
                };
            } else if self.eat_kw("collate") {
                // A text ordering, maybe schema-qualified (the schema is
                // the binder's to check; the name is the ordering's).
                let mut collation = match self.next() {
                    Some(Token::Word(w) | Token::Quoted(w)) => w,
                    _ => return Err(self.error("expected a collation")),
                };
                if self.eat_op(".") {
                    collation = self.ident()?;
                }
                e = Expr::Collate {
                    expr: Box::new(e),
                    collation,
                };
            } else {
                return Ok(e);
            }
        }
    }

    /// The type of a cast (after `::` or `AS`), applied to `e`.  (A cast to
    /// a name that is no type but a cast function — `::regclass` with
    /// pg-compat — the binder makes a call.)
    fn cast_to(&mut self, e: Expr) -> Result<Expr> {
        let (ty, typmod) = self.data_type()?;
        Ok(Expr::Cast {
            expr: Box::new(e),
            ty,
            typmod,
        })
    }

    fn primary(&mut self) -> Result<Expr> {
        let tok = self
            .next()
            .ok_or_else(|| self.error("expected an expression"))?;
        Ok(match tok {
            Token::Number(n) => {
                // As in PostgreSQL: integers are bigint, anything else (or
                // too big) numeric; double precision only beyond numeric's
                // range.
                let float = || -> Result<Expr> {
                    Ok(Expr::Literal(Value::Float(
                        n.parse()
                            .map_err(|_| Error::syntax(format!("bad number {n}")))?,
                    )))
                };
                let numeric = || match crate::decimal::Decimal::parse(&n) {
                    Ok(d) => Ok(Expr::Literal(Value::Numeric(d))),
                    Err(_) => float(),
                };
                if n.contains(['.', 'e', 'E']) {
                    numeric()?
                } else {
                    match n.parse::<i64>() {
                        Ok(i) => Expr::Literal(Value::Int(i)),
                        // 9223372036854775808 only makes sense negated.
                        Err(_) if n == "9223372036854775808" => Expr::Literal(Value::Int(i64::MIN)),
                        Err(_) => numeric()?,
                    }
                }
            }
            Token::Str(s) => Expr::Literal(Value::Text(s)),
            Token::Param(n) => {
                if n == 0 {
                    return Err(Error::syntax("parameters are numbered from $1"));
                }
                Expr::Param(n)
            }
            Token::Op("(") => {
                if self.at_query() {
                    let q = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Subquery(Box::new(q)));
                }
                let e = self.expr()?;
                self.expect_op(")")?;
                e
            }
            Token::Word(w) => match w.as_str() {
                "null" => Expr::Literal(Value::Null),
                "true" => Expr::Literal(Value::Bool(true)),
                "false" => Expr::Literal(Value::Bool(false)),
                "case" => self.case()?,
                "exists" => {
                    self.expect_op("(")?;
                    let q = self.query()?;
                    self.expect_op(")")?;
                    Expr::Exists {
                        query: Box::new(q),
                        negated: false,
                    }
                }
                "cast" => {
                    self.expect_op("(")?;
                    let e = self.expr()?;
                    self.expect_kw("as")?;
                    let e = self.cast_to(e)?;
                    self.expect_op(")")?;
                    e
                }
                "array" if self.peek_op("[") => {
                    self.pos += 1;
                    let mut items = Vec::new();
                    if !self.peek_op("]") {
                        items.push(self.expr()?);
                        while self.eat_op(",") {
                            items.push(self.expr()?);
                        }
                    }
                    self.expect_op("]")?;
                    Expr::Array(items)
                }
                "array" if self.peek_op("(") => {
                    self.pos += 1;
                    let q = self.query()?;
                    self.expect_op(")")?;
                    Expr::ArraySubquery(Box::new(q))
                }
                _ if RESERVED.contains(&w.as_str()) => {
                    self.pos -= 1;
                    return Err(self.error("expected an expression"));
                }
                "current_date" | "current_timestamp" | "localtimestamp" | "localtime"
                | "current_user" | "session_user" | "current_catalog" | "current_role" | "user"
                    if !self.peek_op("(") =>
                {
                    Expr::Function {
                        name: w,
                        args: vec![],
                        distinct: false,
                        star: false,
                        order_by: vec![],
                    }
                }
                _ if DataType::is_prefix(&[w.as_str()]) && self.typed_literal_ahead() => {
                    self.pos -= 1;
                    let (ty, typmod) = self.data_type()?;
                    let Some(Token::Str(s)) = self.next() else {
                        return Err(self.error("expected a string"));
                    };
                    Expr::Cast {
                        expr: Box::new(Expr::Literal(Value::Text(s))),
                        ty,
                        typmod,
                    }
                }
                // `uuid '…'`: a registered type's literal.
                _ if matches!(self.peek(), Some(Token::Str(_))) => {
                    let Some(Token::Str(s)) = self.next() else {
                        unreachable!("peeked")
                    };
                    Expr::Cast {
                        expr: Box::new(Expr::Literal(Value::Text(s))),
                        ty: SqlType::Named {
                            schema: None,
                            name: w,
                        },
                        typmod: TypeMod::default(),
                    }
                }
                _ => self.name_or_call(w)?,
            },
            Token::Quoted(q) => self.name_or_call(q)?,
            t => {
                self.pos -= 1;
                let _ = t;
                return Err(self.error("expected an expression"));
            }
        })
    }

    /// After a type's first word (just consumed): whether the rest of a
    /// type name and then a string follow (`DATE '2024-01-01'`).
    fn typed_literal_ahead(&mut self) -> bool {
        let save = self.pos;
        self.pos -= 1;
        let ok = self.data_type().is_ok() && matches!(self.peek(), Some(Token::Str(_)));
        self.pos = save;
        ok
    }

    /// `ORDER BY` items (after the keywords).
    fn order_items(&mut self) -> Result<Vec<OrderItem>> {
        let mut out = Vec::new();
        loop {
            let expr = self.expr()?;
            let desc = if self.eat_kw("desc") {
                true
            } else {
                self.eat_kw("asc");
                false
            };
            let using = if self.eat_kw("using") {
                Some(self.ident()?)
            } else {
                None
            };
            let desc = desc || (using.is_some() && self.eat_kw("desc"));
            if using.is_some() {
                self.eat_kw("asc");
            }
            let nulls_first = if self.eat_kws(&["nulls", "first"]) {
                Some(true)
            } else if self.eat_kws(&["nulls", "last"]) {
                Some(false)
            } else {
                None
            };
            out.push(OrderItem {
                expr,
                desc,
                nulls_first,
                using,
            });
            if !self.eat_op(",") {
                return Ok(out);
            }
        }
    }

    fn name_or_call(&mut self, first: String) -> Result<Expr> {
        // extract(field FROM expr)
        if first == "extract" && self.peek_op("(") {
            let save = self.pos;
            self.pos += 1;
            let field = match self.next() {
                Some(Token::Word(w) | Token::Str(w) | Token::Quoted(w)) => Some(w),
                _ => None,
            };
            if let Some(field) = field
                && self.eat_kw("from")
            {
                let e = self.expr()?;
                self.expect_op(")")?;
                return Ok(Expr::Function {
                    name: "extract".into(),
                    args: vec![Expr::Literal(Value::Text(field)), e],
                    distinct: false,
                    star: false,
                    order_by: vec![],
                });
            }
            self.pos = save;
        }
        let call = |name: &str, args: Vec<Expr>| Expr::Function {
            name: name.into(),
            args,
            distinct: false,
            star: false,
            order_by: vec![],
        };
        // position(sub IN s): strpos(s, sub).
        if first == "position" && self.peek_op("(") {
            let save = self.pos;
            self.pos += 1;
            let sub = self.expr_prec(PREC_IN + 1)?;
            if self.eat_kw("in") {
                let s = self.expr()?;
                self.expect_op(")")?;
                return Ok(call("strpos", vec![s, sub]));
            }
            self.pos = save;
        }
        // substring(s FROM start [FOR count]), substring(s FOR count).
        if first == "substring" && self.peek_op("(") {
            let save = self.pos;
            self.pos += 1;
            let s = self.expr()?;
            let from = if self.eat_kw("from") {
                Some(self.expr()?)
            } else {
                None
            };
            let count = if self.eat_kw("for") {
                Some(self.expr()?)
            } else {
                None
            };
            if from.is_some() || count.is_some() {
                self.expect_op(")")?;
                let start = from.unwrap_or(Expr::Literal(Value::Int(1)));
                let mut args = vec![s, start];
                args.extend(count);
                return Ok(call("substr", args));
            }
            self.pos = save;
        }
        if self.eat_op("(") {
            let mut args = Vec::new();
            let mut distinct = false;
            let mut star = false;
            let mut order_by = Vec::new();
            if self.eat_op("*") {
                star = true;
            } else if !self.peek_op(")") {
                distinct = self.eat_kw("distinct");
                args.push(self.expr()?);
                while self.eat_op(",") {
                    args.push(self.expr()?);
                }
                if self.eat_kws(&["order", "by"]) {
                    order_by = self.order_items()?;
                }
            }
            self.expect_op(")")?;
            return Ok(Expr::Function {
                name: first,
                args,
                distinct,
                star,
                order_by,
            });
        }
        if self.eat_op(".") {
            let name = self.ident()?;
            // schema.function(…)
            if self.peek_op("(") {
                return self.name_or_call(name);
            }
            // schema.table.column
            if self.eat_op(".") {
                let column = self.ident()?;
                return Ok(Expr::Column {
                    table: Some(name),
                    name: column,
                });
            }
            return Ok(Expr::Column {
                table: Some(first),
                name,
            });
        }
        Ok(Expr::Column {
            table: None,
            name: first,
        })
    }

    fn case(&mut self) -> Result<Expr> {
        let operand = if self.peek_kw("when") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("when") {
            let cond = self.expr()?;
            self.expect_kw("then")?;
            whens.push((cond, self.expr()?));
        }
        if whens.is_empty() {
            return Err(self.error("CASE needs at least one WHEN"));
        }
        let otherwise = if self.eat_kw("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            whens,
            otherwise,
        })
    }

    /// The infix operator at the cursor: its precedence and kind.
    fn infix(&self) -> Option<(u8, Infix)> {
        let t = self.peek()?;
        Some(match t {
            Token::Word(w) => match w.as_str() {
                "or" => (PREC_OR, Infix::Binary(BinaryOp::Or)),
                "and" => (PREC_AND, Infix::Binary(BinaryOp::And)),
                "is" => (PREC_IS, Infix::Is),
                "in" => (PREC_IN, Infix::In(false)),
                "between" => (PREC_IN, Infix::Between(false)),
                "like" => (PREC_IN, Infix::Like(false, false)),
                "ilike" => (PREC_IN, Infix::Like(false, true)),
                "operator" if self.peek_at(1) == Some(&Token::Op("(")) => {
                    // OPERATOR([schema.]op)
                    let (op_at, len) = if matches!(self.peek_at(3), Some(Token::Op("."))) {
                        (4, 6)
                    } else {
                        (2, 4)
                    };
                    let Some(Token::Op(op)) = self.peek_at(op_at) else {
                        return None;
                    };
                    let (prec, op) = binary_operator(op)?;
                    (prec, Infix::Operator(op, len))
                }
                "not" => match self.peek_at(1)? {
                    t if t.is("in") => (PREC_IN, Infix::In(true)),
                    t if t.is("between") => (PREC_IN, Infix::Between(true)),
                    t if t.is("like") => (PREC_IN, Infix::Like(true, false)),
                    t if t.is("ilike") => (PREC_IN, Infix::Like(true, true)),
                    _ => return None,
                },
                _ => return None,
            },
            Token::Op(op) => {
                let (prec, op) = binary_operator(op)?;
                (prec, Infix::Binary(op))
            }
            // Any other operator: PostgreSQL's precedence for them.
            Token::UserOp(_) => (PREC_CONCAT, Infix::User),
            _ => return None,
        })
    }

    fn apply_infix(&mut self, left: Expr, prec: u8, kind: Infix) -> Result<Expr> {
        let boxed = Box::new(left);
        Ok(match kind {
            Infix::User => {
                let Some(Token::UserOp(op)) = self.next() else {
                    unreachable!("peeked")
                };
                Expr::Operator {
                    op,
                    left: boxed,
                    right: Box::new(self.expr_prec(prec + 1)?),
                }
            }
            Infix::Binary(op) | Infix::Operator(op, _) => {
                self.pos += match kind {
                    Infix::Operator(_, n) => n,
                    _ => 1,
                };
                // op ANY / SOME / ALL (array or subquery)
                let quantified = [("any", false), ("some", false), ("all", true)]
                    .into_iter()
                    .find(|(k, _)| self.peek_kw(k) && self.peek_at(1) == Some(&Token::Op("(")));
                if let Some((_, all)) = quantified {
                    self.pos += 2;
                    let right = if self.at_query() {
                        Expr::ArraySubquery(Box::new(self.query()?))
                    } else {
                        self.expr()?
                    };
                    self.expect_op(")")?;
                    return Ok(Expr::AnyAll {
                        left: boxed,
                        op,
                        right: Box::new(right),
                        all,
                    });
                }
                Expr::Binary {
                    op,
                    left: boxed,
                    right: Box::new(self.expr_prec(prec + 1)?),
                }
            }
            Infix::Is => {
                self.pos += 1;
                let negated = self.eat_kw("not");
                if self.eat_kw("null") {
                    Expr::IsNull {
                        expr: boxed,
                        negated,
                    }
                } else if self.eat_kw("true") || self.eat_kw("false") {
                    let v = self.toks[self.pos - 1].0.is("true");
                    // x IS TRUE: false for NULL, unlike x = TRUE.
                    let test = Expr::Function {
                        name: "coalesce".into(),
                        args: vec![
                            Expr::Binary {
                                op: BinaryOp::Eq,
                                left: boxed,
                                right: Box::new(Expr::Literal(Value::Bool(v))),
                            },
                            Expr::Literal(Value::Bool(false)),
                        ],
                        distinct: false,
                        star: false,
                        order_by: vec![],
                    };
                    if negated {
                        Expr::Unary {
                            op: UnaryOp::Not,
                            expr: Box::new(test),
                        }
                    } else {
                        test
                    }
                } else if self.eat_kw("distinct") {
                    self.expect_kw("from")?;
                    let right = self.expr_prec(PREC_IS + 1)?;
                    let f = Expr::Function {
                        name: "__is_distinct_from".into(),
                        args: vec![*boxed, right],
                        distinct: false,
                        star: false,
                        order_by: vec![],
                    };
                    if negated {
                        Expr::Unary {
                            op: UnaryOp::Not,
                            expr: Box::new(f),
                        }
                    } else {
                        f
                    }
                } else {
                    return Err(self.error("expected NULL, TRUE, FALSE or DISTINCT FROM after IS"));
                }
            }
            Infix::In(negated) => {
                self.pos += if negated { 2 } else { 1 };
                self.expect_op("(")?;
                if self.at_query() {
                    let q = self.query()?;
                    self.expect_op(")")?;
                    return Ok(Expr::InSubquery {
                        expr: boxed,
                        query: Box::new(q),
                        negated,
                    });
                }
                let mut list = vec![self.expr()?];
                while self.eat_op(",") {
                    list.push(self.expr()?);
                }
                self.expect_op(")")?;
                Expr::InList {
                    expr: boxed,
                    list,
                    negated,
                }
            }
            Infix::Between(negated) => {
                self.pos += if negated { 2 } else { 1 };
                let low = self.expr_prec(PREC_IN + 1)?;
                self.expect_kw("and")?;
                let high = self.expr_prec(PREC_IN + 1)?;
                Expr::Between {
                    expr: boxed,
                    low: Box::new(low),
                    high: Box::new(high),
                    negated,
                }
            }
            Infix::Like(negated, case_insensitive) => {
                self.pos += if negated { 2 } else { 1 };
                let pattern = self.expr_prec(PREC_IN + 1)?;
                Expr::Like {
                    expr: boxed,
                    pattern: Box::new(pattern),
                    negated,
                    case_insensitive,
                }
            }
        })
    }
}

/// A binary operator token: its precedence and operator.
fn binary_operator(op: &str) -> Option<(u8, BinaryOp)> {
    Some(match op {
        "=" => (PREC_CMP, BinaryOp::Eq),
        "<>" => (PREC_CMP, BinaryOp::NotEq),
        "<" => (PREC_CMP, BinaryOp::Lt),
        "<=" => (PREC_CMP, BinaryOp::LtEq),
        ">" => (PREC_CMP, BinaryOp::Gt),
        ">=" => (PREC_CMP, BinaryOp::GtEq),
        "||" => (PREC_CONCAT, BinaryOp::Concat),
        "~" => (PREC_CONCAT, BinaryOp::Match),
        "!~" => (PREC_CONCAT, BinaryOp::NotMatch),
        "~*" => (PREC_CONCAT, BinaryOp::IMatch),
        "!~*" => (PREC_CONCAT, BinaryOp::NotIMatch),
        "+" => (PREC_ADD, BinaryOp::Add),
        "-" => (PREC_ADD, BinaryOp::Sub),
        "*" => (PREC_MUL, BinaryOp::Mul),
        "/" => (PREC_MUL, BinaryOp::Div),
        "%" => (PREC_MUL, BinaryOp::Mod),
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Infix {
    Binary(BinaryOp),
    /// `OPERATOR(…)`: the operator and how many tokens it takes.
    Operator(BinaryOp, usize),
    Is,
    In(bool),
    Between(bool),
    Like(bool, bool),
    /// A [`Token::UserOp`].
    User,
}

const PREC_OR: u8 = 1;
const PREC_AND: u8 = 2;
const PREC_NOT: u8 = 3;
const PREC_IS: u8 = 4;
const PREC_CMP: u8 = 5;
const PREC_IN: u8 = 6;
const PREC_CONCAT: u8 = 7;
const PREC_ADD: u8 = 8;
const PREC_MUL: u8 = 9;
const PREC_UNARY: u8 = 10;

fn show(t: &Token) -> String {
    match t {
        Token::Word(w) => format!("\"{w}\""),
        Token::Quoted(q) => format!("\"{q}\""),
        Token::Str(s) => format!("'{s}'"),
        Token::Number(n) => n.clone(),
        Token::Param(n) => format!("${n}"),
        Token::Op(o) => format!("\"{o}\""),
        Token::UserOp(o) => format!("\"{o}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> Statement {
        parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[test]
    fn statements_parse() {
        for sql in [
            "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(20) NOT NULL DEFAULT 'x', score DOUBLE PRECISION CHECK (score >= 0))",
            "create table if not exists t2 (a int, b text, primary key (a, b), unique (b), constraint c check (a > 0))",
            "CREATE UNIQUE INDEX i ON t (name)",
            "DROP TABLE IF EXISTS a, b",
            "DROP INDEX i",
            "ALTER TABLE t ADD COLUMN extra BOOLEAN DEFAULT true",
            "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b') RETURNING id",
            "INSERT INTO t SELECT * FROM u",
            "INSERT INTO t DEFAULT VALUES",
            "UPDATE t SET name = name || '!', score = score * 2 WHERE id IN (1, 2)",
            "DELETE FROM t WHERE NOT (id BETWEEN 1 AND 3)",
            "SELECT DISTINCT a.x, count(*) AS n FROM a JOIN b ON a.id = b.aid LEFT JOIN c USING (id) \
             WHERE a.x LIKE 'f%' GROUP BY a.x HAVING count(*) > 1 ORDER BY n DESC NULLS LAST LIMIT 10 OFFSET 2",
            "SELECT 1 UNION ALL SELECT 2 UNION SELECT 3 ORDER BY 1",
            "SELECT * FROM (SELECT 1 AS one) s, t CROSS JOIN u",
            "SELECT CASE WHEN x IS NULL THEN 0 ELSE x END, CAST(y AS TEXT), z::bigint FROM t",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.id) AND id NOT IN (SELECT id FROM v)",
            "SELECT $1 + $2",
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "START TRANSACTION READ ONLY",
            "COMMIT",
            "ROLLBACK WORK",
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "SET client_encoding TO 'UTF8'",
            "SHOW transaction isolation level",
            "EXPLAIN SELECT * FROM t",
            "ANALYZE t",
            "VALUES (1, 'a'), (2, 'b')",
        ] {
            ok(sql);
        }
        assert_eq!(parse("SELECT 1; SELECT 2;;").unwrap().len(), 2);
    }

    #[test]
    fn precedence() {
        let Statement::Query(q) = ok("SELECT 1 + 2 * 3 = 7 AND NOT false OR x IS NOT NULL") else {
            panic!()
        };
        let SetExpr::Select(s) = q.body else { panic!() };
        let SelectItem::Expr { expr, .. } = &s.items[0] else {
            panic!()
        };
        // ((1 + (2*3)) = 7 AND NOT false) OR (x IS NOT NULL)
        let Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } = expr
        else {
            panic!("{expr:?}")
        };
        assert!(matches!(**right, Expr::IsNull { negated: true, .. }));
        let Expr::Binary {
            op: BinaryOp::And,
            left: eq,
            ..
        } = &**left
        else {
            panic!()
        };
        let Expr::Binary {
            op: BinaryOp::Eq,
            left: sum,
            ..
        } = &**eq
        else {
            panic!()
        };
        let Expr::Binary {
            op: BinaryOp::Add,
            right: prod,
            ..
        } = &**sum
        else {
            panic!()
        };
        assert!(matches!(
            **prod,
            Expr::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn defaults_and_checks_keep_their_text() {
        let Statement::CreateTable {
            columns,
            constraints,
            ..
        } = ok(
            "CREATE TABLE t (a INT DEFAULT 1 + 2 NOT NULL, b TEXT CHECK (length(b) > 0), CHECK (a < 100))",
        )
        else {
            panic!()
        };
        assert_eq!(columns[0].default.as_deref(), Some("1 + 2"));
        assert!(columns[0].not_null);
        assert_eq!(columns[1].check.as_deref(), Some("length(b) > 0"));
        assert_eq!(
            constraints,
            vec![TableConstraint::Check(None, "a < 100".into())]
        );
    }

    #[test]
    fn errors_say_where() {
        let e = parse_one("SELECT FROM WHERE").unwrap_err();
        assert_eq!(e.code, "42601");
        assert!(e.message.contains("position"), "{}", e.message);
        assert!(parse_one("SELECT 1 2 3").is_err());
        assert_eq!(
            parse_one("CREATE TABLE t (a INT REFERENCES u DEFERRABLE)")
                .unwrap_err()
                .code,
            "0A000"
        );
        assert_eq!(
            parse_one("SELECT -9223372036854775808").unwrap(),
            Statement::Query(Query {
                with: None,
                body: SetExpr::Select(Box::new(Select {
                    distinct: false,
                    items: vec![SelectItem::Expr {
                        expr: Expr::Literal(Value::Int(i64::MIN)),
                        alias: None
                    }],
                    from: vec![],
                    filter: None,
                    group_by: vec![],
                    having: None,
                })),
                order_by: vec![],
                limit: None,
                offset: None,
            })
        );
    }
}
