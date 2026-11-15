//! `nucleus codegen ts|rust|python|java` (PLAN.md §M14.16 M5): a type per
//! table, generated from the schema (`nc_catalog`), for each language's
//! nucleus driver — each column in the type that driver returns its
//! values as, a nullable one optional; with the table's `SELECT` of its
//! columns in order and a function that makes the type from such a row.
//!
//! The types are the drivers' own (each driver's value decoding is the
//! source of truth: an `integer` is a JavaScript `number`, a `bigint` a
//! `number | bigint` — a `bigint` past 2^53 —, a `numeric` a `Decimal`,
//! Python's `decimal.Decimal`, Java's `BigDecimal`, Rust's `Value` — the
//! Rust driver has no one payload type for it); an enum's labels are a
//! TypeScript union and a Python `Literal`.  A type named as one the
//! generated code uses (`Value`, `Date`, `List`, …) is given `Row`.
//!
//! scripts/driver-tests.sh's codegen step compiles each language's code
//! strictly (tsc --strict, javac -Werror, clippy -D warnings) and reads a
//! schema of every type with it (scripts/fixtures/codegen).

use crate::dump::rows;
use crate::sql::{Backend, Target};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: nucleus codegen <ts|rust|python|java> [<dir> | :memory:] [connection options of nucleus sql]
  --out <file>             write there (default: standard output)
  --tables <a,b,…>         only these tables (default: all but partitions)
  --import <module>        the driver to import types from: TypeScript's
                           module (default nucleus-db), Python's (nucleus),
                           Rust's crate path (nucleus_client)
  --package <name>         Java: the generated class's package (default: none)
A type per table, its columns in the types the language's driver returns;
the table's SELECT and a function making the type from a row of it.
";

/// A column.
struct Col {
    name: String,
    ty: String,
    nullable: bool,
}

/// A table.
struct Table {
    name: String,
    columns: Vec<Col>,
}

/// What the generators read: the tables, and the enums' labels.
struct Schema {
    tables: Vec<Table>,
    enums: Vec<(String, Vec<String>)>,
}

fn read(b: &mut Backend, only: Option<&[String]>) -> Result<Schema> {
    let names: Vec<String> = rows(
        b,
        "SELECT name FROM nc_catalog.tables WHERE name NOT IN (SELECT partition FROM nc_catalog.partitions) ORDER BY name",
    )?
    .into_iter()
    .filter_map(|r| r[0].clone())
    .collect();
    if let Some(only) = only
        && let Some(missing) = only.iter().find(|t| !names.contains(t))
    {
        return Err(format!("no table {missing}"));
    }
    let cols = rows(
        b,
        "SELECT table_name, name, type, nullable FROM nc_catalog.columns ORDER BY table_name, position",
    )?;
    let mut tables = Vec::new();
    for name in names {
        if only.is_some_and(|o| !o.contains(&name)) {
            continue;
        }
        let columns = cols
            .iter()
            .filter(|r| r[0].as_deref() == Some(name.as_str()))
            .map(|r| Col {
                name: r[1].clone().unwrap_or_default(),
                ty: r[2].clone().unwrap_or_default(),
                nullable: r[3].as_deref() != Some("false") && r[3].as_deref() != Some("f"),
            })
            .collect();
        tables.push(Table { name, columns });
    }
    let mut enums: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows(
        b,
        "SELECT type, label FROM nc_catalog.enums ORDER BY type, position",
    )? {
        let (t, l) = (
            r[0].clone().unwrap_or_default(),
            r[1].clone().unwrap_or_default(),
        );
        match enums.last_mut() {
            Some((name, labels)) if *name == t => labels.push(l),
            _ => enums.push((t, vec![l])),
        }
    }
    Ok(Schema { tables, enums })
}

// ------------------------------------------------------------- types

/// A column's type, as the generators see it.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Bool,
    SmallInt,
    Integer,
    BigInt,
    Real,
    Double,
    Numeric,
    Text,
    Enum(Vec<String>),
    Bytes,
    Date,
    Timestamp,
    TimestampTz,
    Interval,
    Time,
    Inet,
    Cidr,
    Uuid,
    Json,
    Vector,
    SparseVector,
    Other,
    Array(Box<Ty>),
}

fn ty(text: &str, enums: &[(String, Vec<String>)]) -> Ty {
    if let Some(inner) = text.strip_suffix("[]") {
        return Ty::Array(Box::new(ty(inner, enums)));
    }
    let base = text.split('(').next().unwrap_or(text).trim();
    match base {
        "boolean" => Ty::Bool,
        "smallint" => Ty::SmallInt,
        "integer" => Ty::Integer,
        "bigint" => Ty::BigInt,
        "real" => Ty::Real,
        "double precision" => Ty::Double,
        "numeric" => Ty::Numeric,
        "text" | "varchar" | "char" => Ty::Text,
        "bytea" => Ty::Bytes,
        "date" => Ty::Date,
        "timestamp" => Ty::Timestamp,
        "timestamptz" => Ty::TimestampTz,
        "interval" => Ty::Interval,
        "time without time zone" | "time" => Ty::Time,
        "inet" => Ty::Inet,
        "cidr" => Ty::Cidr,
        "uuid" => Ty::Uuid,
        "json" => Ty::Json,
        "vector" => Ty::Vector,
        "sparsevec" => Ty::SparseVector,
        other => match enums.iter().find(|(n, _)| n == other) {
            Some((_, labels)) => Ty::Enum(labels.clone()),
            None => Ty::Other,
        },
    }
}

/// `name` as a type's name: `emp_loyees` → `EmpLoyees`.
fn pascal(name: &str) -> String {
    let mut out = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut cs = part.chars();
        if let Some(f) = cs.next() {
            out.push(f.to_ascii_uppercase());
            out.extend(cs);
        }
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out
}

/// The tables' type names: `pascal`, given `Row` when the generated
/// code uses that name (`Value`, `Date`, `List`, …), numbered when two
/// tables would have the same.
fn type_names(s: &Schema, reserved: &[&str]) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    for t in &s.tables {
        let mut name = pascal(&t.name);
        if reserved.contains(&name.as_str()) {
            name.push_str("Row");
        }
        let base = name.clone();
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{base}{n}");
            n += 1;
        }
        taken.push(name);
    }
    taken
}

/// `name` as an identifier of a language: other characters as `_`, a
/// leading digit and a reserved word given a `_`; one already taken in
/// the type, numbered.
fn field(name: &str, reserved: &[&str], taken: &mut Vec<String>) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    if reserved.contains(&s.as_str()) {
        s.push('_');
    }
    let base = s.clone();
    let mut n = 2;
    while taken.contains(&s) {
        s = format!("{base}_{n}");
        n += 1;
    }
    taken.push(s.clone());
    s
}

fn select(t: &Table) -> String {
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| nucleus_sql::definitions::ident(&c.name))
        .collect();
    format!(
        "SELECT {} FROM {}",
        cols.join(", "),
        nucleus_sql::definitions::ident(&t.name)
    )
}

/// A string literal most languages read (JSON's).
fn quoted(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ------------------------------------------------------------ TypeScript

fn ts_type(t: &Ty) -> String {
    match t {
        Ty::Bool => "boolean".into(),
        Ty::SmallInt | Ty::Integer | Ty::Real | Ty::Double => "number".into(),
        Ty::BigInt => "number | bigint".into(),
        Ty::Numeric => "nucleus.Decimal".into(),
        Ty::Text | Ty::Uuid => "string".into(),
        Ty::Enum(labels) => labels
            .iter()
            .map(|l| quoted(l))
            .collect::<Vec<_>>()
            .join(" | "),
        Ty::Bytes => "Uint8Array".into(),
        Ty::Date | Ty::Timestamp | Ty::TimestampTz => "Date".into(),
        Ty::Interval => "nucleus.Interval".into(),
        Ty::Time => "nucleus.Time".into(),
        Ty::Inet => "nucleus.Inet".into(),
        Ty::Cidr => "nucleus.Cidr".into(),
        Ty::Json => "nucleus.JsonValue".into(),
        Ty::Vector => "Float32Array".into(),
        Ty::SparseVector => "nucleus.SparseVector".into(),
        Ty::Other => "nucleus.Ext".into(),
        Ty::Array(inner) => format!("({} | null)[]", ts_type(inner)),
    }
}

const TS_RESERVED: &[&str] = &[
    "Array",
    "Boolean",
    "Date",
    "Error",
    "Float32Array",
    "Map",
    "Number",
    "Object",
    "Promise",
    "Record",
    "Set",
    "String",
    "Uint8Array",
];

fn typescript(s: &Schema, module: &str) -> String {
    let mut out =
        String::from("// Generated by `nucleus codegen ts` from the schema: do not edit.\n");
    out.push_str(&format!(
        "import type * as nucleus from {};\n",
        quoted(module)
    ));
    for (t, ty_name) in s.tables.iter().zip(type_names(s, TS_RESERVED)) {
        out.push_str(&format!(
            "\n/** Table {}. */\nexport interface {ty_name} {{\n",
            t.name
        ));
        let mut fields = Vec::new();
        for c in &t.columns {
            let ty = ts_type(&ty(&c.ty, &s.enums));
            let key = if c.name.chars().enumerate().all(|(i, ch)| {
                ch == '_' || ch == '$' || ch.is_ascii_alphabetic() || (i > 0 && ch.is_ascii_digit())
            }) {
                c.name.clone()
            } else {
                quoted(&c.name)
            };
            let full = if c.nullable {
                format!("{ty} | null")
            } else {
                ty
            };
            out.push_str(&format!("  {key}: {full};\n"));
            fields.push((key, full));
        }
        out.push_str("}\n");
        out.push_str(&format!(
            "export const {ty_name} = {{\n  table: {},\n  select: {},\n  /** A row of `select` as a {ty_name}. */\n  fromRow(row: nucleus.Value[]): {ty_name} {{\n    return {{\n",
            quoted(&t.name),
            quoted(&select(t))
        ));
        for (i, (key, full)) in fields.iter().enumerate() {
            out.push_str(&format!("      {key}: row[{i}] as {full},\n"));
        }
        out.push_str("    };\n  },\n};\n");
    }
    out
}

// --------------------------------------------------------------- Python

const PY_RESERVED: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield", "from_row", "SELECT", "TABLE",
];

/// Names the generated module uses.
const PY_TYPES: &[&str] = &[
    "Any", "ClassVar", "List", "Literal", "Optional", "Sequence", "Union",
];

fn py_type(t: &Ty, module: &str) -> String {
    match t {
        Ty::Bool => "bool".into(),
        Ty::SmallInt | Ty::Integer | Ty::BigInt => "int".into(),
        Ty::Real | Ty::Double => "float".into(),
        Ty::Numeric => "decimal.Decimal".into(),
        Ty::Text => "str".into(),
        Ty::Enum(labels) => format!(
            "Literal[{}]",
            labels
                .iter()
                .map(|l| quoted(l))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ty::Bytes => "bytes".into(),
        Ty::Date => "datetime.date".into(),
        Ty::Timestamp | Ty::TimestampTz => "datetime.datetime".into(),
        Ty::Interval => format!("{module}.Interval"),
        Ty::Time => "datetime.time".into(),
        Ty::Inet => "Union[ipaddress.IPv4Interface, ipaddress.IPv6Interface]".into(),
        Ty::Cidr => "Union[ipaddress.IPv4Network, ipaddress.IPv6Network]".into(),
        Ty::Uuid => "uuid.UUID".into(),
        Ty::Json => "Any".into(),
        Ty::Vector => "array.array".into(),
        Ty::SparseVector => format!("{module}.SparseVector"),
        Ty::Other => format!("{module}.Ext"),
        Ty::Array(inner) => format!("List[Optional[{}]]", py_type(inner, module)),
    }
}

fn python(s: &Schema, module: &str) -> String {
    let mut out = String::from(
        "# Generated by `nucleus codegen python` from the schema: do not edit.\n\
         from __future__ import annotations\n\n\
         import array\nimport dataclasses\nimport datetime\nimport decimal\nimport ipaddress\nimport uuid\n\
         from typing import Any, ClassVar, List, Literal, Optional, Sequence, Union\n\n",
    );
    out.push_str(&format!("import {module}\n"));
    for (t, name) in s.tables.iter().zip(type_names(s, PY_TYPES)) {
        out.push_str(&format!(
            "\n\n@dataclasses.dataclass\nclass {name}:\n    \"\"\"Table {}.\"\"\"\n\n    TABLE: ClassVar[str] = {}\n    SELECT: ClassVar[str] = {}\n\n",
            t.name.replace('"', "'"),
            quoted(&t.name),
            quoted(&select(t))
        ));
        let mut taken = Vec::new();
        for c in &t.columns {
            let ty = py_type(&ty(&c.ty, &s.enums), module);
            let f = field(&c.name, PY_RESERVED, &mut taken);
            if c.nullable {
                out.push_str(&format!("    {f}: Optional[{ty}]\n"));
            } else {
                out.push_str(&format!("    {f}: {ty}\n"));
            }
        }
        out.push_str(&format!(
            "\n    @classmethod\n    def from_row(cls, row: Sequence[Any]) -> {name}:\n        \"\"\"A row of SELECT as a {name}.\"\"\"\n        return cls(*row)\n"
        ));
    }
    out
}

// ----------------------------------------------------------------- Java

const JAVA_RESERVED: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
    "record",
    "var",
    "yield",
    "sealed",
    "permits",
    "fromRow",
    "SELECT",
    "TABLE",
];

/// Names the generated class uses.
const JAVA_TYPES: &[&str] = &[
    "Boolean",
    "Double",
    "Float",
    "Integer",
    "List",
    "Long",
    "Object",
    "Schema",
    "Short",
    "String",
    "SuppressWarnings",
];

fn java_type(t: &Ty) -> String {
    match t {
        Ty::Bool => "Boolean".into(),
        Ty::SmallInt => "Short".into(),
        Ty::Integer => "Integer".into(),
        Ty::BigInt => "Long".into(),
        Ty::Real => "Float".into(),
        Ty::Double => "Double".into(),
        Ty::Numeric => "java.math.BigDecimal".into(),
        Ty::Text | Ty::Enum(_) => "String".into(),
        Ty::Bytes => "byte[]".into(),
        Ty::Date => "java.time.LocalDate".into(),
        Ty::Timestamp => "java.time.LocalDateTime".into(),
        Ty::TimestampTz => "java.time.OffsetDateTime".into(),
        Ty::Interval => "io.github.davidsarosi92.nucleus.Interval".into(),
        Ty::Time => "java.time.LocalTime".into(),
        Ty::Inet => "io.github.davidsarosi92.nucleus.Inet".into(),
        Ty::Cidr => "io.github.davidsarosi92.nucleus.Cidr".into(),
        Ty::Uuid => "java.util.UUID".into(),
        Ty::Json => "io.github.davidsarosi92.nucleus.Json".into(),
        Ty::Vector => "float[]".into(),
        Ty::SparseVector => "io.github.davidsarosi92.nucleus.SparseVector".into(),
        Ty::Other => "io.github.davidsarosi92.nucleus.Ext".into(),
        Ty::Array(inner) => format!("java.util.List<{}>", java_type(inner)),
    }
}

fn java(s: &Schema, package: Option<&str>) -> String {
    let mut out =
        String::from("// Generated by `nucleus codegen java` from the schema: do not edit.\n");
    if let Some(p) = package {
        out.push_str(&format!("package {p};\n"));
    }
    out.push_str(
        "\nimport java.util.List;\n\n/** The schema's tables, a record each. */\npublic final class Schema {\n    private Schema() {}\n",
    );
    for (t, name) in s.tables.iter().zip(type_names(s, JAVA_TYPES)) {
        let mut taken = Vec::new();
        let fields: Vec<(String, String)> = t
            .columns
            .iter()
            .map(|c| {
                (
                    field(&c.name, JAVA_RESERVED, &mut taken),
                    java_type(&ty(&c.ty, &s.enums)),
                )
            })
            .collect();
        let params: Vec<String> = fields.iter().map(|(f, t)| format!("{t} {f}")).collect();
        out.push_str(&format!(
            "\n    /** Table {}. */\n    public record {name}({}) {{\n        public static final String TABLE = {};\n        public static final String SELECT = {};\n\n        /** A row of SELECT as a {name}. */\n        @SuppressWarnings(\"unchecked\")\n        public static {name} fromRow(List<Object> row) {{\n            return new {name}(\n",
            t.name.replace("*/", "* /"),
            params.join(", "),
            quoted(&t.name),
            quoted(&select(t))
        ));
        let args: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (_, t))| format!("                ({t}) row.get({i})"))
            .collect();
        out.push_str(&args.join(",\n"));
        out.push_str(");\n        }\n    }\n");
    }
    out.push_str("}\n");
    out
}

// ----------------------------------------------------------------- Rust

const RUST_RESERVED: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Names the generated module uses.
const RUST_TYPES: &[&str] = &[
    "Value", "Result", "Option", "Some", "None", "Ok", "Err", "Vec", "String", "Self",
];

fn rust_type(t: &Ty) -> String {
    match t {
        Ty::Bool => "bool".into(),
        Ty::SmallInt => "i16".into(),
        Ty::Integer | Ty::Date => "i32".into(),
        Ty::BigInt | Ty::Timestamp | Ty::TimestampTz | Ty::Time => "i64".into(),
        Ty::Real => "f32".into(),
        Ty::Double => "f64".into(),
        Ty::Text | Ty::Enum(_) | Ty::Json => "String".into(),
        Ty::Bytes => "Vec<u8>".into(),
        Ty::Uuid => "[u8; 16]".into(),
        Ty::Vector => "Vec<f32>".into(),
        Ty::Numeric | Ty::Interval | Ty::Inet | Ty::Cidr | Ty::SparseVector | Ty::Other => {
            "Value".into()
        }
        Ty::Array(inner) => format!("Vec<Option<{}>>", rust_type(inner)),
    }
}

/// A closure `Value -> Result<T, Value>` taking a value of `t` apart.
fn rust_conv(t: &Ty) -> String {
    let variant = match t {
        Ty::Bool => "Bool",
        Ty::SmallInt => "SmallInt",
        Ty::Integer => "Integer",
        Ty::BigInt => "Int",
        Ty::Real => "Real",
        Ty::Double => "Float",
        Ty::Text | Ty::Enum(_) => "Text",
        Ty::Json => "Json",
        Ty::Bytes => "Bytes",
        Ty::Date => "Date",
        Ty::Timestamp => "Timestamp",
        Ty::TimestampTz => "TimestampTz",
        Ty::Time => "Time",
        Ty::Uuid => "Uuid",
        Ty::Vector => "Vector",
        Ty::Array(inner) => {
            return format!(
                "|v: Value| match v {{ Value::Array(xs) => xs.into_iter().map(|x| nullable(x, {})).collect(), v => Err(v) }}",
                rust_conv(inner)
            );
        }
        _ => return "|v: Value| Ok(v)".into(),
    };
    format!("|v: Value| match v {{ Value::{variant}(x) => Ok(x), v => Err(v) }}")
}

fn rust(s: &Schema, krate: &str) -> String {
    let mut out = format!(
        "// Generated by `nucleus codegen rust` from the schema: do not edit.\n\
         // A module of its own (`mod schema;`): a program uses some of it.\n\
         #![allow(dead_code)]\n\n\
         use {krate}::Value;\n\n\
         /// A row's next value, taken apart by `f` (`Err`: another type).\n\
         fn column<T>(\n    row: &mut impl Iterator<Item = Value>,\n    name: &str,\n    f: impl FnOnce(Value) -> Result<T, Value>,\n) -> Result<T, String> {{\n    \
         let v = row.next().ok_or_else(|| format!(\"no value for {{name}}\"))?;\n    \
         f(v).map_err(|v| format!(\"{{name}}: unexpected {{v:?}}\"))\n}}\n\n\
         /// `NULL` as `None`, else `f`'s value.\n\
         fn nullable<T>(v: Value, f: impl FnOnce(Value) -> Result<T, Value>) -> Result<Option<T>, Value> {{\n    \
         match v {{\n        Value::Null => Ok(None),\n        v => f(v).map(Some),\n    }}\n}}\n"
    );
    for (t, name) in s.tables.iter().zip(type_names(s, RUST_TYPES)) {
        let mut taken = Vec::new();
        let fields: Vec<(String, &Col, Ty)> = t
            .columns
            .iter()
            .map(|c| {
                let f = field(&c.name.to_ascii_lowercase(), RUST_RESERVED, &mut taken);
                (f, c, ty(&c.ty, &s.enums))
            })
            .collect();
        out.push_str(&format!(
            "\n/// Table {}.\n#[derive(Clone, Debug, PartialEq)]\npub struct {name} {{\n",
            t.name
        ));
        for (f, c, ty) in &fields {
            let rt = rust_type(ty);
            if c.nullable && rt != "Value" {
                out.push_str(&format!("    pub {f}: Option<{rt}>,\n"));
            } else {
                out.push_str(&format!("    pub {f}: {rt},\n"));
            }
        }
        out.push_str(&format!(
            "}}\n\nimpl {name} {{\n    /// The table.\n    pub const TABLE: &'static str = {};\n    /// Its columns, in the fields' order.\n    pub const SELECT: &'static str = {};\n\n    /// A row of [`Self::SELECT`] as a `{name}`.\n    pub fn from_row(row: Vec<Value>) -> Result<Self, String> {{\n        let mut row = row.into_iter();\n        Ok(Self {{\n",
            quoted(&t.name),
            quoted(&select(t))
        ));
        for (f, c, ty) in &fields {
            let conv = rust_conv(ty);
            let value = rust_type(ty) == "Value";
            let expr = if c.nullable && !value {
                format!("|v: Value| nullable(v, {conv})")
            } else {
                conv
            };
            out.push_str(&format!(
                "            {f}: column(&mut row, {}, {expr})?,\n",
                quoted(&c.name)
            ));
        }
        out.push_str("        })\n    }\n}\n");
    }
    out
}

// ------------------------------------------------------------------ run

pub fn run(args: &[String]) -> Result<()> {
    let mut target = Target::new("nucleus codegen");
    let mut language: Option<String> = None;
    let mut out: Option<String> = None;
    let mut only: Option<Vec<String>> = None;
    let mut import: Option<String> = None;
    let mut package: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--out" => out = Some(value("a file")?),
            "--tables" => {
                only = Some(
                    value("table names")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--import" => import = Some(value("a module")?),
            "--package" => package = Some(value("a package")?),
            s if language.is_none() && !s.starts_with('-') => language = Some(s.to_string()),
            _ => {
                if !target.option(a, &mut value)? {
                    return Err(format!("unknown option {a}\n{USAGE}"));
                }
            }
        }
    }
    let language = language.ok_or(USAGE)?;
    if !matches!(
        language.as_str(),
        "ts" | "typescript" | "rust" | "python" | "py" | "java"
    ) {
        return Err(format!("unknown language {language}\n{USAGE}"));
    }
    let mut b = target.open()?;
    let schema = read(&mut b, only.as_deref());
    b.close()?;
    let schema = schema?;
    let text = match language.as_str() {
        "ts" | "typescript" => typescript(&schema, import.as_deref().unwrap_or("nucleus-db")),
        "python" | "py" => python(&schema, import.as_deref().unwrap_or("nucleus")),
        "java" => java(&schema, package.as_deref()),
        _ => rust(&schema, import.as_deref().unwrap_or("nucleus_client")),
    };
    match out {
        Some(path) => std::fs::write(&path, text).map_err(|e| format!("{path}: {e}")),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_identifiers() {
        assert_eq!(pascal("emp_loyees"), "EmpLoyees");
        assert_eq!(pascal("Emp Loyees"), "EmpLoyees");
        assert_eq!(pascal("2024_data"), "T2024Data");
        let mut taken = Vec::new();
        assert_eq!(field("select", &["select"], &mut taken), "select_");
        assert_eq!(field("a b", &[], &mut taken), "a_b");
        assert_eq!(field("a_b", &[], &mut taken), "a_b_2");
        assert_eq!(field("1st", &[], &mut taken), "_1st");
        let tables = ["value", "a_b", "aB", "date"]
            .iter()
            .map(|n| Table {
                name: n.to_string(),
                columns: Vec::new(),
            })
            .collect();
        let s = Schema {
            tables,
            enums: Vec::new(),
        };
        assert_eq!(
            type_names(&s, RUST_TYPES),
            ["ValueRow", "AB", "AB2", "Date"]
        );
        assert_eq!(
            type_names(&s, TS_RESERVED),
            ["Value", "AB", "AB2", "DateRow"]
        );
    }

    #[test]
    fn types_are_the_drivers() {
        let enums = vec![(
            "mood".to_string(),
            vec!["sad".to_string(), "ok".to_string()],
        )];
        assert_eq!(ts_type(&ty("bigint", &enums)), "number | bigint");
        assert_eq!(ts_type(&ty("numeric", &enums)), "nucleus.Decimal");
        assert_eq!(ts_type(&ty("mood", &enums)), "\"sad\" | \"ok\"");
        assert_eq!(ts_type(&ty("integer[]", &enums)), "(number | null)[]");
        assert_eq!(
            py_type(&ty("numeric(10,2)", &enums), "nucleus"),
            "decimal.Decimal"
        );
        assert_eq!(
            java_type(&ty("timestamptz", &enums)),
            "java.time.OffsetDateTime"
        );
        assert_eq!(rust_type(&ty("text[]", &enums)), "Vec<Option<String>>");
        assert_eq!(rust_type(&ty("time without time zone", &enums)), "i64");
    }
}
