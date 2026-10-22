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
        /// Index name (`None`: one is made up, `table_column_idx`).
        name: Option<String>,
        /// Table.
        table: String,
        /// Indexed columns.
        columns: Vec<String>,
        /// Per column, `USING ordering`: index it in that ordering.
        orderings: Vec<Option<String>>,
        /// `UNIQUE`.
        unique: bool,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `ALTER TABLE t ALTER [COLUMN] c [SET DATA] TYPE type [USING expr]`.
    AlterColumnType {
        /// Table.
        table: String,
        /// The column.
        column: String,
        /// Its new type.
        ty: SqlType,
        /// The type's modifier.
        typmod: TypeMod,
        /// How each value converts (over the row): else a cast.
        using: Option<Expr>,
    },
    /// `CREATE TYPE name AS ENUM (labels)`.
    CreateEnum {
        /// The type's name.
        name: String,
        /// Its labels, in order.
        labels: Vec<String>,
    },
    /// `DROP TYPE [IF EXISTS] name` (an enum).
    DropType {
        /// The type's name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER TYPE name …`: teaching a type, or making it forget.
    AlterType {
        /// The type's name.
        type_name: String,
        /// What changes.
        action: TypeAction,
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
    pub ty: SqlType,
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
        /// `schema.` before the name.
        schema: Option<String>,
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
    /// A function in `FROM`: `generate_series(1, 3) [WITH ORDINALITY] [AS]
    /// alias [(columns)]`.
    Function {
        /// Name, lowercase.
        name: String,
        /// Arguments (they may refer to enclosing queries).
        args: Vec<Expr>,
        /// `WITH ORDINALITY`: a row-number column after the others.
        ordinality: bool,
        /// Alias.
        alias: Option<String>,
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

/// What `ALTER TYPE` changes.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeAction {
    /// `ADD ORDERING name BY mapping`: an ordering by a mapping of `value`
    /// into an ordered type.
    AddOrdering {
        /// Its name.
        name: String,
        /// The mapping, as SQL over `value`.
        mapping: String,
    },
    /// `ALTER ORDERING name BY mapping`: redefined (what uses it is
    /// rebuilt).
    AlterOrdering {
        /// Its name.
        name: String,
        /// The new mapping.
        mapping: String,
    },
    /// `DROP ORDERING [IF EXISTS] name [CASCADE]`.
    DropOrdering {
        /// Its name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
        /// `CASCADE`: drop what uses it too.
        cascade: bool,
    },
    /// `ADD OPERATOR symbol (left, right) [RETURNS type] AS function`.
    AddOperator {
        /// The symbol (`<->`).
        symbol: String,
        /// The left operand's type.
        left: SqlType,
        /// The right operand's type.
        right: SqlType,
        /// The function it calls.
        function: String,
    },
    /// `DROP OPERATOR [IF EXISTS] symbol (left, right)`.
    DropOperator {
        /// The symbol.
        symbol: String,
        /// The left operand's type.
        left: SqlType,
        /// The right operand's type.
        right: SqlType,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ADD CAST TO type AS function [EXPLICIT | ASSIGNMENT | IMPLICIT]`.
    AddCast {
        /// The target type.
        to: SqlType,
        /// The function converting.
        function: String,
        /// `ASSIGNMENT` 1, `IMPLICIT` 2, else 0.
        context: u8,
    },
    /// `UPGRADE`: every stored value rewritten in the type's current
    /// encoding version.
    Upgrade,
    /// An enum's `ADD VALUE [IF NOT EXISTS] label [BEFORE | AFTER label]`.
    AddValue {
        /// The new label.
        label: String,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// Where it goes.
        place: crate::enums::Place,
    },
    /// An enum's `RENAME VALUE from TO to`.
    RenameValue {
        /// The label now.
        from: String,
        /// Its new text.
        to: String,
    },
    /// `DROP CAST [IF EXISTS] TO type`.
    DropCast {
        /// The target type.
        to: SqlType,
        /// `IF EXISTS`.
        if_exists: bool,
    },
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
    /// `USING ordering`: sort in a taught ordering of the type.
    pub using: Option<String>,
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
        ty: SqlType,
        /// Its modifier: `VARCHAR(n)`, `NUMERIC(p, s)`.
        typmod: TypeMod,
    },
    /// `left op right` with an operator outside the grammar's own (`a <->
    /// b`): a registered or taught one, found by symbol and types.
    Operator {
        /// The symbol.
        op: String,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// `expr COLLATE name`: compared and sorted in text ordering `name`.
    Collate {
        /// The operand.
        expr: Box<Expr>,
        /// The ordering (collation) name.
        collation: String,
    },
    /// `ARRAY[a, b, …]`.
    Array(Vec<Expr>),
    /// `ARRAY(subquery)`: the subquery's single column as an array.
    ArraySubquery(Box<Query>),
    /// `array[index]` (1-based).
    Subscript {
        /// The array.
        expr: Box<Expr>,
        /// The position.
        index: Box<Expr>,
    },
    /// `left op ANY (array)` / `left op ALL (array)` (a subquery stands for
    /// the array of its rows).
    AnyAll {
        /// The left operand.
        left: Box<Expr>,
        /// The comparison.
        op: BinaryOp,
        /// The array.
        right: Box<Expr>,
        /// `ALL` (else `ANY` / `SOME`).
        all: bool,
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
        /// `ORDER BY` inside an aggregate's parentheses.
        order_by: Vec<OrderItem>,
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

/// A type as written: a built-in one, or a name the binder resolves in
/// the session's registry (`uuid`, a module's type).
#[derive(Clone, Debug, PartialEq)]
pub enum SqlType {
    /// A built-in type (the grammar knows their multi-word names).
    Builtin {
        /// `schema.` before it (`pg_catalog.int4`).
        schema: Option<String>,
        /// The type.
        ty: DataType,
    },
    /// A name, maybe schema-qualified.
    Named {
        /// `schema.` before it.
        schema: Option<String>,
        /// The name.
        name: String,
    },
}

impl SqlType {
    /// An unqualified built-in type.
    pub fn builtin(ty: DataType) -> Self {
        SqlType::Builtin { schema: None, ty }
    }
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
    /// `~`: matches a regular expression.
    Match,
    /// `!~`.
    NotMatch,
    /// `~*`: matches, ignoring case.
    IMatch,
    /// `!~*`.
    NotIMatch,
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
            BinaryOp::Match => "~",
            BinaryOp::NotMatch => "!~",
            BinaryOp::IMatch => "~*",
            BinaryOp::NotIMatch => "!~*",
        }
    }
}
