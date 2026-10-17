//! The syntax tree the parser produces.

use crate::types::{DataType, Value};

/// A statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE`.
    CreateTable {
        /// Table name.
        name: String,
        /// Columns.
        columns: Vec<ColumnSpec>,
        /// Table constraints.
        constraints: Vec<TableConstraint>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `DROP TABLE`.
    DropTable {
        /// Tables.
        names: Vec<String>,
        /// `IF EXISTS`.
        if_exists: bool,
        /// `CASCADE`: drop the foreign keys that reference them too.
        cascade: bool,
    },
    /// `CREATE [UNIQUE] INDEX`.
    CreateIndex {
        /// Index name.
        name: String,
        /// Table.
        table: String,
        /// Indexed columns.
        columns: Vec<String>,
        /// `UNIQUE`.
        unique: bool,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `DROP INDEX`.
    DropIndex {
        /// Index name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER TABLE … ADD COLUMN`.
    AddColumn {
        /// Table.
        table: String,
        /// The new column.
        column: ColumnSpec,
    },
    /// `ALTER TABLE … ADD [CONSTRAINT name] …`.
    AddConstraint {
        /// Table.
        table: String,
        /// The constraint.
        constraint: TableConstraint,
    },
    /// `ALTER TABLE … DROP CONSTRAINT [IF EXISTS] name`.
    DropConstraint {
        /// Table.
        table: String,
        /// Constraint name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `INSERT`.
    Insert {
        /// Table.
        table: String,
        /// Target columns, if listed.
        columns: Option<Vec<String>>,
        /// The rows.
        source: Query,
        /// `RETURNING` items.
        returning: Vec<SelectItem>,
    },
    /// `UPDATE`.
    Update {
        /// Table.
        table: String,
        /// `SET column = expr`.
        assignments: Vec<(String, Expr)>,
        /// `WHERE`.
        filter: Option<Expr>,
        /// `RETURNING` items.
        returning: Vec<SelectItem>,
    },
    /// `DELETE`.
    Delete {
        /// Table.
        table: String,
        /// `WHERE`.
        filter: Option<Expr>,
        /// `RETURNING` items.
        returning: Vec<SelectItem>,
    },
    /// A query.
    Query(Query),
    /// `BEGIN` / `START TRANSACTION`.
    Begin(TxnMode),
    /// `COMMIT` / `END`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SET TRANSACTION …`.
    SetTransaction(TxnMode),
    /// `SET name = value`.
    Set {
        /// Variable.
        name: String,
        /// Value, as text.
        value: String,
    },
    /// `SHOW name`.
    Show(String),
    /// `EXPLAIN`.
    Explain(Box<Statement>),
    /// `ANALYZE [table]`.
    Analyze(Option<String>),
}

/// Transaction characteristics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxnMode {
    /// `ISOLATION LEVEL …`, if given.
    pub isolation: Option<IsolationLevel>,
    /// `READ ONLY` (`Some(true)`) / `READ WRITE` (`Some(false)`).
    pub read_only: Option<bool>,
}

/// An isolation level as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED` / `READ COMMITTED` / `REPEATABLE READ` — all run as
    /// snapshot isolation (stronger than asked, as the standard allows).
    Snapshot,
    /// `SERIALIZABLE`.
    Serializable,
}

/// A column definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    /// Name.
    pub name: String,
    /// Type.
    pub ty: DataType,
    /// `VARCHAR(n)`.
    pub max_len: Option<u32>,
    /// `NUMERIC(precision, scale)`.
    pub numeric: Option<(u32, u32)>,
    /// `NOT NULL` (or `PRIMARY KEY`).
    pub not_null: bool,
    /// `PRIMARY KEY` on the column.
    pub primary_key: bool,
    /// `UNIQUE` on the column.
    pub unique: bool,
    /// `DEFAULT expr`, as SQL text.
    pub default: Option<String>,
    /// `CHECK (expr)` on the column, as SQL text.
    pub check: Option<String>,
    /// `[CONSTRAINT name] REFERENCES …` on the column.
    pub references: Option<(Option<String>, References)>,
}

/// `REFERENCES table [(columns)] [ON DELETE …] [ON UPDATE …]`.
#[derive(Clone, Debug, PartialEq)]
pub struct References {
    /// The referenced table.
    pub table: String,
    /// The referenced columns (empty: its primary key).
    pub columns: Vec<String>,
    /// `ON DELETE`.
    pub on_delete: FkAction,
    /// `ON UPDATE`.
    pub on_update: FkAction,
}

/// What a foreign key does when the referenced row goes away or changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FkAction {
    /// Fail at the end of the statement (the default).
    #[default]
    NoAction,
    /// Fail.
    Restrict,
    /// Delete the referencing rows / update their columns too.
    Cascade,
    /// Set the referencing columns to NULL.
    SetNull,
    /// Set the referencing columns to their defaults.
    SetDefault,
}

impl FkAction {
    /// The SQL words.
    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }
}

/// A table constraint.
#[derive(Clone, Debug, PartialEq)]
pub enum TableConstraint {
    /// `PRIMARY KEY (cols)`.
    PrimaryKey(Vec<String>),
    /// `UNIQUE (cols)`.
    Unique(Option<String>, Vec<String>),
    /// `CHECK (expr)`.
    Check(Option<String>, String),
    /// `FOREIGN KEY (cols) REFERENCES …`.
    ForeignKey {
        /// Name.
        name: Option<String>,
        /// The referencing columns.
        columns: Vec<String>,
        /// What they reference.
        references: References,
    },
}

/// A query: a body with ordering and limits.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// `WITH …`: common table expressions.
    pub with: Option<With>,
    /// The rows.
    pub body: SetExpr,
    /// `ORDER BY`.
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`.
    pub limit: Option<Expr>,
    /// `OFFSET`.
    pub offset: Option<Expr>,
}

/// A `WITH` clause.
#[derive(Clone, Debug, PartialEq)]
pub struct With {
    /// `WITH RECURSIVE`: a CTE may refer to itself.
    pub recursive: bool,
    /// The CTEs, in order (each sees the ones before it).
    pub ctes: Vec<Cte>,
}

/// A common table expression: `name [(columns)] AS (query)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    /// Name.
    pub name: String,
    /// Column aliases.
    pub columns: Vec<String>,
    /// The query.
    pub query: Box<Query>,
}

/// A query body.
#[derive(Clone, Debug, PartialEq)]
pub enum SetExpr {
    /// `SELECT`.
    Select(Box<Select>),
    /// `VALUES (…), (…)`.
    Values(Vec<Vec<Expr>>),
    /// `left UNION | EXCEPT | INTERSECT [ALL] right`.
    SetOp {
        /// Which.
        op: SetOp,
        /// Left side.
        left: Box<SetExpr>,
        /// Right side.
        right: Box<SetExpr>,
        /// `ALL`: keep duplicates.
        all: bool,
    },
}

/// A set operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`: rows of either side.
    Union,
    /// `EXCEPT`: rows of the left side not on the right.
    Except,
    /// `INTERSECT`: rows on both sides.
    Intersect,
}

impl SetOp {
    /// The SQL keyword.
    pub fn keyword(self) -> &'static str {
        match self {
            SetOp::Union => "UNION",
            SetOp::Except => "EXCEPT",
            SetOp::Intersect => "INTERSECT",
        }
    }
}

/// A `SELECT`.
#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    /// `DISTINCT`.
    pub distinct: bool,
    /// The select list.
    pub items: Vec<SelectItem>,
    /// `FROM` (comma-separated items are cross joined).
    pub from: Vec<TableRef>,
    /// `WHERE`.
    pub filter: Option<Expr>,
    /// `GROUP BY`.
    pub group_by: Vec<Expr>,
    /// `HAVING`.
    pub having: Option<Expr>,
}

/// An item of a select list.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Wildcard,
    /// `t.*`.
    QualifiedWildcard(String),
    /// `expr [AS alias]`.
    Expr {
        /// The expression.
        expr: Expr,
        /// The alias.
        alias: Option<String>,
    },
}

/// A `FROM` item.
#[derive(Clone, Debug, PartialEq)]
pub enum TableRef {
    /// A table.
    Table {
        /// Name.
        name: String,
        /// Alias.
        alias: Option<String>,
        /// Column aliases: `AS alias (a, b)` renames the first columns.
        columns: Vec<String>,
    },
    /// `(query) AS alias [(columns)]`.
    Subquery {
        /// The query.
        query: Box<Query>,
        /// Alias.
        alias: String,
        /// Column aliases.
        columns: Vec<String>,
    },
    /// A join.
    Join {
        /// Left side.
        left: Box<TableRef>,
        /// Right side.
        right: Box<TableRef>,
        /// Kind.
        kind: JoinKind,
        /// `ON` condition (none for `CROSS JOIN` and `USING`).
        on: Option<Expr>,
        /// `USING (columns)`: equal columns, resolved by the binder.
        using: Vec<String>,
    },
}

/// A join kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    /// `[INNER] JOIN`, `CROSS JOIN`, `,`.
    Inner,
    /// `LEFT [OUTER] JOIN`.
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
}

/// An `ORDER BY` item.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderItem {
    /// The expression.
    pub expr: Expr,
    /// `DESC`.
    pub desc: bool,
    /// `NULLS FIRST` / `NULLS LAST`, if given.
    pub nulls_first: Option<bool>,
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A literal.
    Literal(Value),
    /// A column, maybe qualified.
    Column {
        /// The table or alias.
        table: Option<String>,
        /// The column.
        name: String,
    },
    /// `$n`.
    Param(usize),
    /// A prefix operator: `-`, `+`, `NOT`.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },
    /// An infix operator.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// The operand.
        expr: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// The operand.
        expr: Box<Expr>,
        /// Low bound.
        low: Box<Expr>,
        /// High bound.
        high: Box<Expr>,
        /// `NOT BETWEEN`.
        negated: bool,
    },
    /// `expr [NOT] IN (list)`.
    InList {
        /// The operand.
        expr: Box<Expr>,
        /// The list.
        list: Vec<Expr>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] IN (subquery)`.
    InSubquery {
        /// The operand.
        expr: Box<Expr>,
        /// The subquery.
        query: Box<Query>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `[NOT] EXISTS (subquery)`.
    Exists {
        /// The subquery.
        query: Box<Query>,
        /// `NOT EXISTS`.
        negated: bool,
    },
    /// `(subquery)` used as a value.
    Subquery(Box<Query>),
    /// `expr [NOT] LIKE / ILIKE pattern`.
    Like {
        /// The operand.
        expr: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// `NOT LIKE`.
        negated: bool,
        /// `ILIKE`.
        case_insensitive: bool,
    },
    /// `CASE [operand] WHEN … THEN … [ELSE …] END`.
    Case {
        /// The operand of a simple `CASE`.
        operand: Option<Box<Expr>>,
        /// `WHEN` / `THEN` pairs.
        whens: Vec<(Expr, Expr)>,
        /// `ELSE`.
        otherwise: Option<Box<Expr>>,
    },
    /// `CAST(expr AS type)` / `expr::type` / `type 'literal'`.
    Cast {
        /// The operand.
        expr: Box<Expr>,
        /// The type.
        ty: DataType,
        /// Its modifier: `VARCHAR(n)`, `NUMERIC(p, s)`.
        typmod: TypeMod,
    },
    /// A function call; `count(*)` has `star`.
    Function {
        /// Name, lowercase.
        name: String,
        /// Arguments.
        args: Vec<Expr>,
        /// `DISTINCT` inside an aggregate.
        distinct: bool,
        /// `count(*)`.
        star: bool,
    },
}

/// A type modifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeMod {
    /// `VARCHAR(n)`: the longest value, in characters.
    pub max_len: Option<u32>,
    /// `NUMERIC(precision, scale)`.
    pub numeric: Option<(u32, u32)>,
}

/// A prefix operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-`.
    Neg,
    /// `+`.
    Plus,
    /// `NOT`.
    Not,
}

/// An infix operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `OR`.
    Or,
    /// `AND`.
    And,
    /// `=`.
    Eq,
    /// `<>`.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// `||`.
    Concat,
}

impl BinaryOp {
    /// The operator as SQL.
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Concat => "||",
        }
    }
}
