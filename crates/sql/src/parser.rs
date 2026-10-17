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
            "show" => {
                self.pos += 1;
                if self.eat_kws(&["transaction", "isolation", "level"]) {
                    return Ok(Statement::Show("transaction_isolation".into()));
                }
                Ok(Statement::Show(self.ident()?))
            }
            "explain" => {
                self.pos += 1;
                Ok(Statement::Explain(Box::new(self.statement()?)))
            }
            "analyze" => {
                self.pos += 1;
                let table = if self.at_end() || self.peek_op(";") {
                    None
                } else {
                    Some(self.ident()?)
                };
                Ok(Statement::Analyze(table))
            }
            _ => Err(self.error("expected a statement")),
        }
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
        let name = self.ident()?;
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
            let name = self.ident()?;
            self.expect_kw("on")?;
            let table = self.ident()?;
            let columns = self.ident_list()?;
            return Ok(Statement::CreateIndex {
                name,
                table,
                columns,
                unique,
                if_not_exists,
            });
        }
        if unique {
            return Err(self.error("expected INDEX"));
        }
        self.expect_kw("table")?;
        let if_not_exists = self.eat_kws(&["if", "not", "exists"]);
        let name = self.ident()?;
        self.expect_op("(")?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.peek_kw("primary")
                || self.peek_kw("unique")
                || self.peek_kw("check")
                || self.peek_kw("constraint")
            {
                constraints.push(self.table_constraint()?);
            } else if self.peek_kw("foreign") {
                return Err(Error::feature("FOREIGN KEY is not supported yet"));
            } else {
                columns.push(self.column_spec()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
        })
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
        } else if self.peek_kw("foreign") {
            Err(Error::feature("FOREIGN KEY is not supported yet"))
        } else {
            Err(self.error("expected PRIMARY KEY, UNIQUE or CHECK"))
        }
    }

    fn data_type(&mut self) -> Result<(DataType, Option<u32>)> {
        let mut words: Vec<String> = Vec::new();
        while let Some(Token::Word(w)) = self.peek() {
            let candidate: Vec<&str> = words
                .iter()
                .map(String::as_str)
                .chain([w.as_str()])
                .collect();
            let prefix_ok = DataType::parse(&candidate).is_some()
                || matches!(candidate.as_slice(), ["double"] | ["character"]);
            if !prefix_ok {
                break;
            }
            words.push(w.clone());
            self.pos += 1;
        }
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let ty = DataType::parse(&refs).ok_or_else(|| self.error("expected a type"))?;
        let mut max_len = None;
        if self.eat_op("(") {
            let n = match self.next() {
                Some(Token::Number(n)) => n
                    .parse::<u32>()
                    .map_err(|_| Error::syntax("bad type length"))?,
                _ => return Err(self.error("expected a length")),
            };
            if self.eat_op(",") {
                self.next(); // NUMERIC(p, s): scale ignored
            }
            self.expect_op(")")?;
            if ty == DataType::Text {
                max_len = Some(n);
            }
        }
        Ok((ty, max_len))
    }

    fn column_spec(&mut self) -> Result<ColumnSpec> {
        let name = self.ident()?;
        let (ty, max_len) = self.data_type()?;
        let mut c = ColumnSpec {
            name,
            ty,
            max_len,
            not_null: false,
            primary_key: false,
            unique: false,
            default: None,
            check: None,
        };
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
                self.ident()?;
            } else if self.peek_kw("references") {
                return Err(Error::feature(
                    "REFERENCES (foreign keys) is not supported yet",
                ));
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
        self.expect_kw("table")?;
        let if_exists = self.eat_kws(&["if", "exists"]);
        let mut names = vec![self.ident()?];
        while self.eat_op(",") {
            names.push(self.ident()?);
        }
        Ok(Statement::DropTable { names, if_exists })
    }

    fn alter(&mut self) -> Result<Statement> {
        self.expect_kw("alter")?;
        self.expect_kw("table")?;
        let table = self.ident()?;
        if !self.eat_kw("add") {
            return Err(Error::feature("only ALTER TABLE … ADD COLUMN is supported"));
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
        let table = self.ident()?;
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
        let table = self.ident()?;
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
        let table = self.ident()?;
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
        let mut order_by = Vec::new();
        if self.eat_kws(&["order", "by"]) {
            loop {
                let expr = self.expr()?;
                let desc = if self.eat_kw("desc") {
                    true
                } else {
                    self.eat_kw("asc");
                    false
                };
                let nulls_first = if self.eat_kws(&["nulls", "first"]) {
                    Some(true)
                } else if self.eat_kws(&["nulls", "last"]) {
                    Some(false)
                } else {
                    None
                };
                order_by.push(OrderItem {
                    expr,
                    desc,
                    nulls_first,
                });
                if !self.eat_op(",") {
                    break;
                }
            }
        }
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
        let name = self.ident()?;
        let alias = self.alias()?;
        let columns = if alias.is_some() {
            self.column_aliases()?
        } else {
            Vec::new()
        };
        Ok(TableRef::Table {
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
        while self.eat_op("::") {
            let (ty, _) = self.data_type()?;
            e = Expr::Cast {
                expr: Box::new(e),
                ty,
            };
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr> {
        let tok = self
            .next()
            .ok_or_else(|| self.error("expected an expression"))?;
        Ok(match tok {
            Token::Number(n) => {
                if n.contains(['.', 'e', 'E']) {
                    Expr::Literal(Value::Float(
                        n.parse()
                            .map_err(|_| Error::syntax(format!("bad number {n}")))?,
                    ))
                } else {
                    match n.parse::<i64>() {
                        Ok(i) => Expr::Literal(Value::Int(i)),
                        // 9223372036854775808 only makes sense negated.
                        Err(_) if n == "9223372036854775808" => Expr::Literal(Value::Int(i64::MIN)),
                        Err(_) => Expr::Literal(Value::Float(
                            n.parse()
                                .map_err(|_| Error::syntax(format!("bad number {n}")))?,
                        )),
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
                    let (ty, _) = self.data_type()?;
                    self.expect_op(")")?;
                    Expr::Cast {
                        expr: Box::new(e),
                        ty,
                    }
                }
                _ if RESERVED.contains(&w.as_str()) => {
                    self.pos -= 1;
                    return Err(self.error("expected an expression"));
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

    fn name_or_call(&mut self, first: String) -> Result<Expr> {
        if self.eat_op("(") {
            let mut args = Vec::new();
            let mut distinct = false;
            let mut star = false;
            if self.eat_op("*") {
                star = true;
            } else if !self.peek_op(")") {
                distinct = self.eat_kw("distinct");
                args.push(self.expr()?);
                while self.eat_op(",") {
                    args.push(self.expr()?);
                }
            }
            self.expect_op(")")?;
            return Ok(Expr::Function {
                name: first,
                args,
                distinct,
                star,
            });
        }
        if self.eat_op(".") {
            let name = self.ident()?;
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
                let b = match *op {
                    "=" => (PREC_CMP, BinaryOp::Eq),
                    "<>" => (PREC_CMP, BinaryOp::NotEq),
                    "<" => (PREC_CMP, BinaryOp::Lt),
                    "<=" => (PREC_CMP, BinaryOp::LtEq),
                    ">" => (PREC_CMP, BinaryOp::Gt),
                    ">=" => (PREC_CMP, BinaryOp::GtEq),
                    "||" => (PREC_CONCAT, BinaryOp::Concat),
                    "+" => (PREC_ADD, BinaryOp::Add),
                    "-" => (PREC_ADD, BinaryOp::Sub),
                    "*" => (PREC_MUL, BinaryOp::Mul),
                    "/" => (PREC_MUL, BinaryOp::Div),
                    "%" => (PREC_MUL, BinaryOp::Mod),
                    _ => return None,
                };
                (b.0, Infix::Binary(b.1))
            }
            _ => return None,
        })
    }

    fn apply_infix(&mut self, left: Expr, prec: u8, kind: Infix) -> Result<Expr> {
        let boxed = Box::new(left);
        Ok(match kind {
            Infix::Binary(op) => {
                self.pos += 1;
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

#[derive(Clone, Copy)]
enum Infix {
    Binary(BinaryOp),
    Is,
    In(bool),
    Between(bool),
    Like(bool, bool),
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
            parse_one("CREATE TABLE t (a INT REFERENCES u)")
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
