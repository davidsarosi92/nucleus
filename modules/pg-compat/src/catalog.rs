//! The SQL side of PostgreSQL compatibility, the `pg-compat` module
//! ([`PgCompat`]): the
//! `pg_catalog` and `information_schema` tables PostgreSQL clients query,
//! as views of the nucleus catalog (PLAN.md §M14.5) — enough for `psql`'s
//! `\d` family and for tools that read `information_schema` — and the
//! `pg_*` functions they call.  nucleus's own catalog is `nc_catalog`
//! (`nucleus_sql`'s `ncatalog`); these exist because clients hard-code
//! PostgreSQL's names, and only sessions whose registry has the module
//! see them (the PostgreSQL front end's).
//!
//! The tables are virtual: their rows are made from the catalog when they
//! are scanned, and they cannot be written.  Object ids follow
//! PostgreSQL's where they are fixed (types, namespaces, access methods);
//! a table's oid is `16384 + 1000 × id`, its indexes and constraints are
//! numbered after it:
//!
//! | object | oid |
//! |--------|-----|
//! | table | `t = 16384 + 1000 × table id` |
//! | primary key index | `t + 1` |
//! | index | `t + 1 + index id` |
//! | primary key constraint | `t + 500` |
//! | unique constraint | `t + 500 + index id` |
//! | check constraint `j` | `t + 700 + j` |
//! | foreign key `j` | `t + 800 + j` |
//! | catalog table `k` | `12000 + k` |
//!
//! Unique indexes named `…_key` (the names UNIQUE constraints get) are shown
//! as constraints.  Everything belongs to one role, `nucleus` (oid 10).

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use nucleus_kv::KvTxn;
use nucleus_sql::ext::catalog::{self, Column, ConstraintKind, FkAction, Table};
use nucleus_sql::ext::{
    Arg, ArgInfo, CallContext, CatalogView, DataType, Error, Module, Registry, Result, Returns,
    Row, ScalarFunction, Signature, Value, Volatility,
};

/// The `pg_catalog` namespace.
pub const NS_CATALOG: i64 = 11;
/// The `public` namespace.
pub const NS_PUBLIC: i64 = 2200;
/// The `information_schema` namespace.
pub const NS_INFO: i64 = 13_000;
/// The `nc_catalog` namespace (nucleus's own catalog).
pub const NS_NC: i64 = 13_100;
/// The one role.
pub const OWNER: i64 = 10;
/// PostgreSQL's `uuid`.
pub const UUID: i64 = 2950;
const ROLE: &str = "nucleus";
const DATABASE_OID: i64 = 16_383;

const B: DataType = DataType::Bool;
const I: DataType = DataType::BigInt;
const F: DataType = DataType::Float;
const T: DataType = DataType::Text;
const IA: DataType = DataType::Array(&DataType::BigInt);
const TA: DataType = DataType::Array(&DataType::Text);

/// A catalog table: schema, name, columns.
struct Def {
    ns: i64,
    name: &'static str,
    cols: &'static [(&'static str, DataType)],
}

const DEFS: &[Def] = &[
    Def {
        ns: NS_CATALOG,
        name: "pg_namespace",
        cols: &[("oid", I), ("nspname", T), ("nspowner", I), ("nspacl", TA)],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_class",
        cols: &[
            ("oid", I),
            ("relname", T),
            ("relnamespace", I),
            ("reltype", I),
            ("reloftype", I),
            ("relowner", I),
            ("relam", I),
            ("relfilenode", I),
            ("reltablespace", I),
            ("relpages", I),
            ("reltuples", F),
            ("relallvisible", I),
            ("reltoastrelid", I),
            ("relhasindex", B),
            ("relisshared", B),
            ("relpersistence", T),
            ("relkind", T),
            ("relnatts", I),
            ("relchecks", I),
            ("relhasrules", B),
            ("relhastriggers", B),
            ("relhassubclass", B),
            ("relrowsecurity", B),
            ("relforcerowsecurity", B),
            ("relispopulated", B),
            ("relreplident", T),
            ("relispartition", B),
            ("relrewrite", I),
            ("relfrozenxid", I),
            ("relminmxid", I),
            ("relacl", TA),
            ("reloptions", TA),
            ("relpartbound", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_am",
        cols: &[("oid", I), ("amname", T), ("amhandler", T), ("amtype", T)],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_attribute",
        cols: &[
            ("attrelid", I),
            ("attname", T),
            ("atttypid", I),
            ("attlen", I),
            ("attnum", I),
            ("attcacheoff", I),
            ("atttypmod", I),
            ("attndims", I),
            ("attbyval", B),
            ("attalign", T),
            ("attstorage", T),
            ("attcompression", T),
            ("attnotnull", B),
            ("atthasdef", B),
            ("atthasmissing", B),
            ("attidentity", T),
            ("attgenerated", T),
            ("attisdropped", B),
            ("attislocal", B),
            ("attinhcount", I),
            ("attstattarget", I),
            ("attcollation", I),
            ("attacl", TA),
            ("attoptions", TA),
            ("attfdwoptions", TA),
            ("attmissingval", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_attrdef",
        cols: &[("oid", I), ("adrelid", I), ("adnum", I), ("adbin", T)],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_type",
        cols: &[
            ("oid", I),
            ("typname", T),
            ("typnamespace", I),
            ("typowner", I),
            ("typlen", I),
            ("typbyval", B),
            ("typtype", T),
            ("typcategory", T),
            ("typispreferred", B),
            ("typisdefined", B),
            ("typdelim", T),
            ("typrelid", I),
            ("typsubscript", T),
            ("typelem", I),
            ("typarray", I),
            ("typinput", T),
            ("typoutput", T),
            ("typreceive", T),
            ("typsend", T),
            ("typnotnull", B),
            ("typbasetype", I),
            ("typtypmod", I),
            ("typndims", I),
            ("typcollation", I),
            ("typdefault", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_collation",
        cols: &[
            ("oid", I),
            ("collname", T),
            ("collnamespace", I),
            ("collowner", I),
            ("collprovider", T),
            ("collisdeterministic", B),
            ("collencoding", I),
            ("collcollate", T),
            ("collctype", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_index",
        cols: &[
            ("indexrelid", I),
            ("indrelid", I),
            ("indnatts", I),
            ("indnkeyatts", I),
            ("indisunique", B),
            ("indnullsnotdistinct", B),
            ("indisprimary", B),
            ("indisexclusion", B),
            ("indimmediate", B),
            ("indisclustered", B),
            ("indisvalid", B),
            ("indcheckxmin", B),
            ("indisready", B),
            ("indislive", B),
            ("indisreplident", B),
            ("indkey", IA),
            ("indcollation", IA),
            ("indclass", IA),
            ("indoption", IA),
            ("indexprs", T),
            ("indpred", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_constraint",
        cols: &[
            ("oid", I),
            ("conname", T),
            ("connamespace", I),
            ("contype", T),
            ("condeferrable", B),
            ("condeferred", B),
            ("convalidated", B),
            ("conrelid", I),
            ("contypid", I),
            ("conindid", I),
            ("conparentid", I),
            ("confrelid", I),
            ("confupdtype", T),
            ("confdeltype", T),
            ("confmatchtype", T),
            ("conislocal", B),
            ("coninhcount", I),
            ("connoinherit", B),
            ("conkey", IA),
            ("confkey", IA),
            ("conbin", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_policy",
        cols: &[
            ("oid", I),
            ("polname", T),
            ("polrelid", I),
            ("polcmd", T),
            ("polpermissive", B),
            ("polroles", IA),
            ("polqual", T),
            ("polwithcheck", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_roles",
        cols: &[
            ("oid", I),
            ("rolname", T),
            ("rolsuper", B),
            ("rolinherit", B),
            ("rolcreaterole", B),
            ("rolcreatedb", B),
            ("rolcanlogin", B),
            ("rolreplication", B),
            ("rolconnlimit", I),
            ("rolpassword", T),
            ("rolvaliduntil", DataType::TimestampTz),
            ("rolbypassrls", B),
            ("rolconfig", TA),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_auth_members",
        cols: &[
            ("oid", I),
            ("roleid", I),
            ("member", I),
            ("grantor", I),
            ("admin_option", B),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_statistic_ext",
        cols: &[
            ("oid", I),
            ("stxrelid", I),
            ("stxname", T),
            ("stxnamespace", I),
            ("stxowner", I),
            ("stxstattarget", I),
            ("stxkeys", IA),
            ("stxkind", TA),
            ("stxexprs", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_publication",
        cols: &[
            ("oid", I),
            ("pubname", T),
            ("pubowner", I),
            ("puballtables", B),
            ("pubinsert", B),
            ("pubupdate", B),
            ("pubdelete", B),
            ("pubtruncate", B),
            ("pubviaroot", B),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_publication_namespace",
        cols: &[("oid", I), ("pnpubid", I), ("pnnspid", I)],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_publication_rel",
        cols: &[
            ("oid", I),
            ("prpubid", I),
            ("prrelid", I),
            ("prqual", T),
            ("prattrs", IA),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_trigger",
        cols: &[
            ("oid", I),
            ("tgrelid", I),
            ("tgparentid", I),
            ("tgname", T),
            ("tgfoid", I),
            ("tgtype", I),
            ("tgenabled", T),
            ("tgisinternal", B),
            ("tgconstrrelid", I),
            ("tgconstrindid", I),
            ("tgconstraint", I),
            ("tgdeferrable", B),
            ("tginitdeferred", B),
            ("tgnargs", I),
            ("tgattr", IA),
            ("tgqual", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_inherits",
        cols: &[
            ("inhrelid", I),
            ("inhparent", I),
            ("inhseqno", I),
            ("inhdetachpending", B),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_description",
        cols: &[
            ("objoid", I),
            ("classoid", I),
            ("objsubid", I),
            ("description", T),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_database",
        cols: &[
            ("oid", I),
            ("datname", T),
            ("datdba", I),
            ("encoding", I),
            ("datlocprovider", T),
            ("datistemplate", B),
            ("datallowconn", B),
            ("datconnlimit", I),
            ("datfrozenxid", I),
            ("datminmxid", I),
            ("dattablespace", I),
            ("datcollate", T),
            ("datctype", T),
            ("daticulocale", T),
            ("daticurules", T),
            ("datcollversion", T),
            ("datacl", TA),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_tablespace",
        cols: &[
            ("oid", I),
            ("spcname", T),
            ("spcowner", I),
            ("spcacl", TA),
            ("spcoptions", TA),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_tables",
        cols: &[
            ("schemaname", T),
            ("tablename", T),
            ("tableowner", T),
            ("tablespace", T),
            ("hasindexes", B),
            ("hasrules", B),
            ("hastriggers", B),
            ("rowsecurity", B),
        ],
    },
    Def {
        ns: NS_CATALOG,
        name: "pg_indexes",
        cols: &[
            ("schemaname", T),
            ("tablename", T),
            ("indexname", T),
            ("tablespace", T),
            ("indexdef", T),
        ],
    },
    Def {
        ns: NS_INFO,
        name: "schemata",
        cols: &[("catalog_name", T), ("schema_name", T), ("schema_owner", T)],
    },
    Def {
        ns: NS_INFO,
        name: "tables",
        cols: &[
            ("table_catalog", T),
            ("table_schema", T),
            ("table_name", T),
            ("table_type", T),
            ("is_insertable_into", T),
            ("is_typed", T),
        ],
    },
    Def {
        ns: NS_INFO,
        name: "columns",
        cols: &[
            ("table_catalog", T),
            ("table_schema", T),
            ("table_name", T),
            ("column_name", T),
            ("ordinal_position", I),
            ("column_default", T),
            ("is_nullable", T),
            ("data_type", T),
            ("character_maximum_length", I),
            ("numeric_precision", I),
            ("numeric_scale", I),
            ("datetime_precision", I),
            ("udt_name", T),
            ("is_identity", T),
            ("is_generated", T),
        ],
    },
    Def {
        ns: NS_INFO,
        name: "table_constraints",
        cols: &[
            ("constraint_catalog", T),
            ("constraint_schema", T),
            ("constraint_name", T),
            ("table_catalog", T),
            ("table_schema", T),
            ("table_name", T),
            ("constraint_type", T),
            ("is_deferrable", T),
            ("initially_deferred", T),
        ],
    },
    Def {
        ns: NS_INFO,
        name: "key_column_usage",
        cols: &[
            ("constraint_catalog", T),
            ("constraint_schema", T),
            ("constraint_name", T),
            ("table_catalog", T),
            ("table_schema", T),
            ("table_name", T),
            ("column_name", T),
            ("ordinal_position", I),
        ],
    },
];

fn ns_name(ns: i64) -> Option<&'static str> {
    Some(match ns {
        NS_CATALOG => "pg_catalog",
        NS_PUBLIC => "public",
        NS_INFO => "information_schema",
        NS_NC => "nc_catalog",
        _ => return None,
    })
}

fn def_table(k: usize) -> Table {
    let d = &DEFS[k];
    Table {
        id: 0,
        name: format!("{}.{}", ns_name(d.ns).expect("known"), d.name),
        columns: d
            .cols
            .iter()
            .map(|(n, ty)| Column {
                name: (*n).to_string(),
                ty: *ty,
                nullable: true,
                default: None,
                max_len: None,
                numeric: None,
                comment: None,
            })
            .collect(),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
        next_index_id: 1,
        foreign_keys: Vec::new(),
        referenced_by: Vec::new(),
        owner: None,
        row_security: false,
        force_row_security: false,
        comment: None,
        storage: "row".into(),
        columnar: Vec::new(),
        copy: Default::default(),
        partition_by: None,
        partition_of: None,
        storage_policy: None,
        statistics: Vec::new(),
    }
}

/// The `pg-compat` module.
pub struct PgCompat;

impl Module for PgCompat {
    fn name(&self) -> &str {
        "pg-compat"
    }

    fn register(&self, r: &mut Registry) -> Result<()> {
        r.add_schema("pg_catalog", "PostgreSQL-compatible catalog views", true)?;
        r.add_schema("information_schema", "SQL-standard catalog views", false)?;
        // PostgreSQL's names for types nucleus has under others.
        for (alias, ty) in [
            ("oid", DataType::BigInt),
            ("xid", DataType::BigInt),
            ("regproc", DataType::BigInt),
            ("name", DataType::Text),
            ("bpchar", DataType::Char),
        ] {
            r.add_alias(alias, ty)?;
        }
        for k in 0..DEFS.len() {
            r.add_view(Arc::new(PgView(k)))?;
        }
        for (f, names, arity) in FUNCTIONS {
            r.add_function(
                names,
                Arc::new(PgFunction {
                    f: *f,
                    arity: arity.clone(),
                }),
            )?;
        }
        Ok(())
    }
}

/// `DEFS[k]` as a view.
struct PgView(usize);

impl CatalogView for PgView {
    fn schema(&self) -> &str {
        ns_name(DEFS[self.0].ns).expect("known")
    }

    fn name(&self) -> &str {
        DEFS[self.0].name
    }

    fn columns(&self) -> Vec<(String, DataType)> {
        DEFS[self.0]
            .cols
            .iter()
            .map(|(n, t)| ((*n).to_string(), *t))
            .collect()
    }

    fn rows(&self, cx: &CallContext<'_>) -> Result<Vec<Row>> {
        rows(self.0, cx.txn(), cx.registry())
    }
}

// ------------------------------------------------------------------ oids

/// A table's oid.
pub fn rel_oid(t: &Table) -> i64 {
    16_384 + 1000 * i64::from(t.id)
}

fn index_oid(t: &Table, index: Option<usize>) -> i64 {
    rel_oid(t) + 1 + index.map_or(0, |i| i64::from(t.indexes[i].id))
}

/// A column's type oid and type modifier, as PostgreSQL reports them.
fn type_of(c: &Column) -> (i64, i64) {
    let typmod = match (c.max_len, c.numeric) {
        (Some(n), _) => i64::from(n) + 4,
        (_, Some((p, s))) => ((i64::from(p) << 16) | i64::from(s)) + 4,
        _ => -1,
    };
    let oid = if c.ty == DataType::Text && c.max_len.is_some() {
        1043
    } else if c.ty == DataType::Char {
        1042
    } else {
        type_oid(c.ty)
    };
    (oid, typmod)
}

/// A role's oid: stable, from its name (the bootstrap administrator's is
/// PostgreSQL's 10).
pub fn role_oid(name: &str) -> i64 {
    if name == ROLE {
        return OWNER;
    }
    let h = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    16_384 + (h % (1 << 30)) as i64
}

/// The oid of a type.
pub fn type_oid(t: DataType) -> i64 {
    match t {
        DataType::Bool => 16,
        DataType::SmallInt => 21,
        DataType::Integer => 23,
        DataType::BigInt => 20,
        DataType::Real => 700,
        DataType::Float => 701,
        DataType::Text => 25,
        DataType::Bytea => 17,
        DataType::Char => 1042,
        DataType::Numeric => 1700,
        DataType::Date => 1082,
        DataType::Time => 1083,
        DataType::Timestamp => 1114,
        DataType::TimestampTz => 1184,
        DataType::Interval => 1186,
        DataType::Inet => 869,
        DataType::Cidr => 650,
        DataType::Ext(t) if t.name() == "uuid" => UUID,
        // Registered types PostgreSQL has no oid for are shown as text.
        // Enums too: PostgreSQL gives each its own oid, nucleus sends labels.
        DataType::Ext(_) | DataType::Enum(_) => 25,
        DataType::Array(e) => match e {
            DataType::Bool => 1000,
            DataType::SmallInt => 1005,
            DataType::Integer => 1007,
            DataType::BigInt => 1016,
            DataType::Real => 1021,
            DataType::Float => 1022,
            DataType::Text => 1009,
            DataType::Bytea => 1001,
            DataType::Char => 1014,
            DataType::Numeric => 1231,
            DataType::Date => 1182,
            DataType::Time => 1183,
            DataType::Timestamp => 1115,
            DataType::TimestampTz => 1185,
            DataType::Interval => 1187,
            DataType::Inet => 1041,
            DataType::Cidr => 651,
            DataType::Array(_) | DataType::Ext(_) | DataType::Enum(_) => 0,
        },
    }
}

/// `(oid, name, length, category, element, array, collation)` of the types
/// `pg_type` lists.
const TYPES: &[(i64, &str, i64, &str, i64, i64, i64)] = &[
    (16, "bool", 1, "B", 0, 1000, 0),
    (17, "bytea", -1, "U", 0, 1001, 0),
    (18, "char", 1, "Z", 0, 1002, 0),
    (19, "name", 64, "S", 0, 1003, 950),
    (20, "int8", 8, "N", 0, 1016, 0),
    (21, "int2", 2, "N", 0, 1005, 0),
    (23, "int4", 4, "N", 0, 1007, 0),
    (25, "text", -1, "S", 0, 1009, 100),
    (26, "oid", 4, "N", 0, 1028, 0),
    (700, "float4", 4, "N", 0, 1021, 0),
    (701, "float8", 8, "N", 0, 1022, 0),
    (1042, "bpchar", -1, "S", 0, 1014, 100),
    (1043, "varchar", -1, "S", 0, 1015, 100),
    (1082, "date", 4, "D", 0, 1182, 0),
    (1083, "time", 8, "D", 0, 1183, 0),
    (1114, "timestamp", 8, "D", 0, 1115, 0),
    (1184, "timestamptz", 8, "D", 0, 1185, 0),
    (1186, "interval", 16, "T", 0, 1187, 0),
    (869, "inet", -1, "I", 0, 1041, 0),
    (650, "cidr", -1, "I", 0, 651, 0),
    (1700, "numeric", -1, "N", 0, 1231, 0),
    (2205, "regclass", 4, "N", 0, 2210, 0),
    (2950, "uuid", 16, "U", 0, 2951, 0),
    (2951, "_uuid", -1, "A", 2950, 0, 0),
    (1000, "_bool", -1, "A", 16, 0, 0),
    (1002, "_char", -1, "A", 18, 0, 0),
    (1003, "_name", -1, "A", 19, 0, 950),
    (1005, "_int2", -1, "A", 21, 0, 0),
    (1007, "_int4", -1, "A", 23, 0, 0),
    (1009, "_text", -1, "A", 25, 0, 100),
    (1014, "_bpchar", -1, "A", 1042, 0, 100),
    (1015, "_varchar", -1, "A", 1043, 0, 100),
    (1016, "_int8", -1, "A", 20, 0, 0),
    (1021, "_float4", -1, "A", 700, 0, 0),
    (1022, "_float8", -1, "A", 701, 0, 0),
    (1028, "_oid", -1, "A", 26, 0, 0),
    (1115, "_timestamp", -1, "A", 1114, 0, 0),
    (1182, "_date", -1, "A", 1082, 0, 0),
    (1185, "_timestamptz", -1, "A", 1184, 0, 0),
    (1187, "_interval", -1, "A", 1186, 0, 0),
    (1231, "_numeric", -1, "A", 1700, 0, 0),
    (2210, "_regclass", -1, "A", 2205, 0, 0),
];

/// `format_type(oid, typmod)`: a type's SQL name.
pub fn format_type(oid: i64, typmod: Option<i64>) -> Option<String> {
    let m = typmod.filter(|m| *m >= 4).map(|m| m - 4);
    let base = match oid {
        16 => "boolean".into(),
        18 => "\"char\"".into(),
        19 => "name".into(),
        20 => "bigint".into(),
        21 => "smallint".into(),
        23 => "integer".into(),
        25 => "text".into(),
        26 => "oid".into(),
        700 => "real".into(),
        701 => "double precision".into(),
        1042 => match m {
            Some(n) => format!("character({n})"),
            None => "character".into(),
        },
        1043 => match m {
            Some(n) => format!("character varying({n})"),
            None => "character varying".into(),
        },
        1082 => "date".into(),
        1114 => "timestamp without time zone".into(),
        1184 => "timestamp with time zone".into(),
        1186 => "interval".into(),
        1700 => match m {
            Some(m) => format!("numeric({},{})", m >> 16, m & 0xffff),
            None => "numeric".into(),
        },
        2205 => "regclass".into(),
        2950 => "uuid".into(),
        _ => {
            let (_, _, _, _, elem, _, _) = TYPES.iter().find(|t| t.0 == oid)?;
            return format_type(*elem, typmod).map(|e| format!("{e}[]"));
        }
    };
    Some(base)
}

// ------------------------------------------------------------------ rows

/// A row for `table` from named values (unnamed columns are NULL).
fn row(table: &Table, vals: &[(&str, Value)]) -> Row {
    let mut by: HashMap<&str, &Value> = HashMap::new();
    for (k, v) in vals {
        by.insert(k, v);
    }
    table
        .columns
        .iter()
        .map(|c| {
            by.get(c.name.as_str())
                .map_or(Value::Null, |v| (*v).clone())
        })
        .collect()
}

fn int(i: impl Into<i64>) -> Value {
    Value::Int(i.into())
}

fn text(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

fn ints(v: impl IntoIterator<Item = i64>) -> Value {
    Value::Array(v.into_iter().map(Value::Int).collect())
}

fn attnums(cols: &[usize]) -> Value {
    ints(cols.iter().map(|&c| c as i64 + 1))
}

fn fk_code(a: FkAction) -> &'static str {
    match a {
        FkAction::NoAction => "a",
        FkAction::Restrict => "r",
        FkAction::Cascade => "c",
        FkAction::SetNull => "n",
        FkAction::SetDefault => "d",
    }
}

/// A relation in `pg_class`: a user table or index, or a catalog table.
struct Rel {
    oid: i64,
    name: String,
    ns: i64,
    kind: &'static str,
    am: i64,
    /// Columns: name, type oid, typmod, not null, has default, collation.
    cols: Vec<(String, i64, i64, bool, bool, i64)>,
    checks: i64,
    has_index: bool,
    has_triggers: bool,
    /// The user table it is (or indexes).
    table: Option<(Table, Option<Option<usize>>)>,
}

fn column_attrs(t: &Table, c: usize) -> (String, i64, i64, bool, bool, i64) {
    let col = &t.columns[c];
    let (oid, typmod) = type_of(col);
    (
        col.name.clone(),
        oid,
        typmod,
        !col.nullable || t.primary_key.contains(&c),
        col.default.is_some(),
        if col.ty == DataType::Text { 100 } else { 0 },
    )
}

fn relations(tables: &[Table], registry: &Registry) -> Vec<Rel> {
    let mut out = Vec::new();
    for t in tables {
        out.push(Rel {
            oid: rel_oid(t),
            name: t.name.clone(),
            ns: NS_PUBLIC,
            kind: "r",
            am: 2,
            cols: (0..t.columns.len()).map(|c| column_attrs(t, c)).collect(),
            checks: t.checks.len() as i64,
            has_index: !t.primary_key.is_empty() || !t.indexes.is_empty(),
            has_triggers: !t.foreign_keys.is_empty() || !t.referenced_by.is_empty(),
            table: Some((t.clone(), None)),
        });
        let mut indexes: Vec<(Option<usize>, String, Vec<usize>)> = Vec::new();
        if !t.primary_key.is_empty() {
            indexes.push((None, format!("{}_pkey", t.name), t.primary_key.clone()));
        }
        for (i, ix) in t.indexes.iter().enumerate() {
            indexes.push((Some(i), ix.name.clone(), ix.columns.clone()));
        }
        for (i, name, cols) in indexes {
            out.push(Rel {
                oid: index_oid(t, i),
                name,
                ns: NS_PUBLIC,
                kind: "i",
                am: 403,
                cols: cols.iter().map(|&c| column_attrs(t, c)).collect(),
                checks: 0,
                has_index: false,
                has_triggers: false,
                table: Some((t.clone(), Some(i))),
            });
        }
    }
    for (k, d) in DEFS.iter().enumerate() {
        out.push(Rel {
            oid: 12_000 + k as i64,
            name: d.name.to_string(),
            ns: d.ns,
            kind: if d.ns == NS_INFO { "v" } else { "r" },
            am: if d.ns == NS_INFO { 0 } else { 2 },
            cols: d
                .cols
                .iter()
                .map(|(n, ty)| {
                    (
                        (*n).to_string(),
                        type_oid(*ty),
                        -1,
                        false,
                        false,
                        if *ty == T { 100 } else { 0 },
                    )
                })
                .collect(),
            checks: 0,
            has_index: false,
            has_triggers: false,
            table: None,
        });
    }
    let nc = registry
        .views()
        .iter()
        .filter(|v| v.schema() == "nc_catalog");
    for (k, v) in nc.enumerate() {
        out.push(Rel {
            oid: 12_500 + k as i64,
            name: v.name().to_string(),
            ns: NS_NC,
            kind: "v",
            am: 0,
            cols: v
                .columns()
                .into_iter()
                .map(|(n, ty)| (n, type_oid(ty), -1, false, false, 0))
                .collect(),
            checks: 0,
            has_index: false,
            has_triggers: false,
            table: None,
        });
    }
    out
}

/// A constraint in `pg_constraint`.
pub(crate) struct Con {
    oid: i64,
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) table: Table,
    index: i64,
    /// The referenced table, for a foreign key.
    references: Option<Table>,
    cols: Vec<usize>,
    ref_cols: Vec<usize>,
    on_update: &'static str,
    on_delete: &'static str,
    pub(crate) def: String,
}

fn quoted_list(t: &Table, cols: &[usize]) -> String {
    cols.iter()
        .map(|&c| t.columns[c].name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn constraints(tables: &[Table]) -> Vec<Con> {
    catalog::constraints(tables)
        .into_iter()
        .map(|c| {
            let t = c.table;
            let base = rel_oid(t);
            let mut con = Con {
                oid: base + 500,
                name: c.name,
                kind: "p",
                table: t.clone(),
                index: index_oid(t, None),
                references: None,
                cols: t.primary_key.clone(),
                ref_cols: Vec::new(),
                on_update: " ",
                on_delete: " ",
                def: c.definition,
            };
            match c.kind {
                ConstraintKind::PrimaryKey => {}
                ConstraintKind::Unique(i) => {
                    con.oid = base + 500 + i64::from(t.indexes[i].id);
                    con.kind = "u";
                    con.index = index_oid(t, Some(i));
                    con.cols = t.indexes[i].columns.clone();
                }
                ConstraintKind::Check(j) => {
                    con.oid = base + 700 + j as i64;
                    con.kind = "c";
                    con.index = 0;
                    con.cols = Vec::new();
                }
                ConstraintKind::ForeignKey(j) => {
                    let fk = &t.foreign_keys[j];
                    let parent = tables.iter().find(|p| p.name == fk.table).expect("listed");
                    con.oid = base + 800 + j as i64;
                    con.kind = "f";
                    con.index = catalog::referenced_key(parent, &fk.ref_columns)
                        .map_or(0, |(i, _)| index_oid(parent, i));
                    con.references = Some(parent.clone());
                    con.cols = fk.columns.clone();
                    con.ref_cols = fk.ref_columns.clone();
                    con.on_update = fk_code(fk.on_update);
                    con.on_delete = fk_code(fk.on_delete);
                }
            }
            con
        })
        .collect()
}

fn index_def(t: &Table, i: Option<usize>) -> String {
    let (name, cols, unique) = match i {
        None => (format!("{}_pkey", t.name), t.primary_key.clone(), true),
        Some(i) => (
            t.indexes[i].name.clone(),
            t.indexes[i].columns.clone(),
            t.indexes[i].unique,
        ),
    };
    format!(
        "CREATE {}INDEX {name} ON public.{} USING btree ({})",
        if unique { "UNIQUE " } else { "" },
        t.name,
        quoted_list(t, &cols)
    )
}

fn data_type_name(c: &Column) -> String {
    let (oid, _) = type_of(c);
    if c.ty.element().is_some() {
        return "ARRAY".into();
    }
    format_type(oid, None).unwrap_or_else(|| c.ty.name().into())
}

/// The rows of catalog table `DEFS[k]`.
fn rows(k: usize, txn: &dyn KvTxn, registry: &Registry) -> Result<Vec<Row>> {
    let short = DEFS[k].name;
    let def = def_table(k);
    let tables = catalog::tables(txn)?;
    let r = |vals: &[(&str, Value)]| row(&def, vals);
    let mut out = Vec::new();
    match short {
        "pg_namespace" => {
            for ns in [NS_CATALOG, NS_PUBLIC, NS_INFO, NS_NC] {
                out.push(r(&[
                    ("oid", int(ns)),
                    ("nspname", text(ns_name(ns).expect("known"))),
                    ("nspowner", int(OWNER)),
                ]));
            }
        }
        "pg_am" => {
            out.push(r(&[
                ("oid", int(2)),
                ("amname", text("heap")),
                ("amhandler", text("heap_tableam_handler")),
                ("amtype", text("t")),
            ]));
            out.push(r(&[
                ("oid", int(403)),
                ("amname", text("btree")),
                ("amhandler", text("bthandler")),
                ("amtype", text("i")),
            ]));
        }
        // Comments: tables' (objsubid 0) and columns' (their number).
        "pg_description" => {
            for t in &tables {
                let comments = std::iter::once((0, &t.comment)).chain(
                    t.columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (i as i64 + 1, &c.comment)),
                );
                for (sub, comment) in comments {
                    if let Some(c) = comment {
                        out.push(r(&[
                            ("objoid", int(rel_oid(t))),
                            ("classoid", int(1259)),
                            ("objsubid", int(sub)),
                            ("description", text(c)),
                        ]));
                    }
                }
            }
        }
        "pg_class" => {
            for rel in relations(&tables, registry) {
                out.push(r(&[
                    ("oid", int(rel.oid)),
                    ("relname", text(rel.name)),
                    ("relnamespace", int(rel.ns)),
                    ("reltype", int(0)),
                    ("reloftype", int(0)),
                    (
                        "relowner",
                        int(rel
                            .table
                            .as_ref()
                            .and_then(|(t, _)| t.owner.as_deref())
                            .map_or(OWNER, role_oid)),
                    ),
                    ("relam", int(rel.am)),
                    ("relfilenode", int(rel.oid)),
                    ("reltablespace", int(0)),
                    ("relpages", int(0)),
                    ("reltuples", Value::Float(-1.0)),
                    ("relallvisible", int(0)),
                    ("reltoastrelid", int(0)),
                    ("relhasindex", Value::Bool(rel.has_index)),
                    ("relisshared", Value::Bool(false)),
                    ("relpersistence", text("p")),
                    ("relkind", text(rel.kind)),
                    ("relnatts", int(rel.cols.len() as i64)),
                    ("relchecks", int(rel.checks)),
                    ("relhasrules", Value::Bool(false)),
                    ("relhastriggers", Value::Bool(rel.has_triggers)),
                    ("relhassubclass", Value::Bool(false)),
                    ("relrowsecurity", Value::Bool(false)),
                    ("relforcerowsecurity", Value::Bool(false)),
                    ("relispopulated", Value::Bool(true)),
                    (
                        "relreplident",
                        text(if rel.kind == "r" { "d" } else { "n" }),
                    ),
                    ("relispartition", Value::Bool(false)),
                    ("relrewrite", int(0)),
                    ("relfrozenxid", int(0)),
                    ("relminmxid", int(0)),
                ]));
            }
        }
        "pg_attribute" => {
            for rel in relations(&tables, registry) {
                for (i, (name, typ, typmod, notnull, hasdef, coll)) in
                    rel.cols.into_iter().enumerate()
                {
                    let len = TYPES.iter().find(|t| t.0 == typ).map_or(-1, |t| t.2);
                    out.push(r(&[
                        ("attrelid", int(rel.oid)),
                        ("attname", text(name)),
                        ("atttypid", int(typ)),
                        ("attlen", int(len)),
                        ("attnum", int(i as i64 + 1)),
                        ("attcacheoff", int(-1)),
                        ("atttypmod", int(typmod)),
                        ("attndims", int(0)),
                        ("attbyval", Value::Bool(len > 0 && len <= 8)),
                        ("attalign", text("d")),
                        ("attstorage", text(if len < 0 { "x" } else { "p" })),
                        ("attcompression", text("")),
                        ("attnotnull", Value::Bool(notnull)),
                        ("atthasdef", Value::Bool(hasdef)),
                        ("atthasmissing", Value::Bool(false)),
                        ("attidentity", text("")),
                        ("attgenerated", text("")),
                        ("attisdropped", Value::Bool(false)),
                        ("attislocal", Value::Bool(true)),
                        ("attinhcount", int(0)),
                        ("attstattarget", int(-1)),
                        ("attcollation", int(coll)),
                    ]));
                }
            }
        }
        "pg_attrdef" => {
            for t in &tables {
                for (c, col) in t.columns.iter().enumerate() {
                    if let Some(d) = &col.default {
                        out.push(r(&[
                            ("oid", int(rel_oid(t) + 300 + c as i64)),
                            ("adrelid", int(rel_oid(t))),
                            ("adnum", int(c as i64 + 1)),
                            ("adbin", text(d.clone())),
                        ]));
                    }
                }
            }
        }
        "pg_type" => {
            for &(oid, name, len, cat, elem, array, coll) in TYPES {
                out.push(r(&[
                    ("oid", int(oid)),
                    ("typname", text(name)),
                    ("typnamespace", int(NS_CATALOG)),
                    ("typowner", int(OWNER)),
                    ("typlen", int(len)),
                    ("typbyval", Value::Bool(len > 0 && len <= 8)),
                    ("typtype", text("b")),
                    ("typcategory", text(cat)),
                    ("typispreferred", Value::Bool(matches!(oid, 25 | 701 | 16))),
                    ("typisdefined", Value::Bool(true)),
                    ("typdelim", text(",")),
                    ("typrelid", int(0)),
                    ("typelem", int(elem)),
                    ("typarray", int(array)),
                    ("typnotnull", Value::Bool(false)),
                    ("typbasetype", int(0)),
                    ("typtypmod", int(-1)),
                    ("typndims", int(0)),
                    ("typcollation", int(coll)),
                ]));
            }
        }
        "pg_collation" => {
            for (oid, name, provider) in
                [(100, "default", "d"), (950, "C", "c"), (951, "POSIX", "c")]
            {
                out.push(r(&[
                    ("oid", int(oid)),
                    ("collname", text(name)),
                    ("collnamespace", int(NS_CATALOG)),
                    ("collowner", int(OWNER)),
                    ("collprovider", text(provider)),
                    ("collisdeterministic", Value::Bool(true)),
                    ("collencoding", int(-1)),
                    (
                        "collcollate",
                        if oid == 100 { Value::Null } else { text(name) },
                    ),
                    (
                        "collctype",
                        if oid == 100 { Value::Null } else { text(name) },
                    ),
                ]));
            }
        }
        "pg_index" => {
            for t in &tables {
                let mut list: Vec<(Option<usize>, Vec<usize>, bool, bool)> = Vec::new();
                if !t.primary_key.is_empty() {
                    list.push((None, t.primary_key.clone(), true, true));
                }
                for (i, ix) in t.indexes.iter().enumerate() {
                    list.push((Some(i), ix.columns.clone(), ix.unique, false));
                }
                for (i, cols, unique, primary) in list {
                    let n = cols.len() as i64;
                    out.push(r(&[
                        ("indexrelid", int(index_oid(t, i))),
                        ("indrelid", int(rel_oid(t))),
                        ("indnatts", int(n)),
                        ("indnkeyatts", int(n)),
                        ("indisunique", Value::Bool(unique)),
                        ("indnullsnotdistinct", Value::Bool(false)),
                        ("indisprimary", Value::Bool(primary)),
                        ("indisexclusion", Value::Bool(false)),
                        ("indimmediate", Value::Bool(true)),
                        ("indisclustered", Value::Bool(false)),
                        ("indisvalid", Value::Bool(true)),
                        ("indcheckxmin", Value::Bool(false)),
                        ("indisready", Value::Bool(true)),
                        ("indislive", Value::Bool(true)),
                        ("indisreplident", Value::Bool(false)),
                        ("indkey", attnums(&cols)),
                        ("indcollation", ints(cols.iter().map(|_| 0))),
                        ("indclass", ints(cols.iter().map(|_| 0))),
                        ("indoption", ints(cols.iter().map(|_| 0))),
                    ]));
                }
            }
        }
        "pg_statistic_ext" => {
            for t in &tables {
                for (i, st) in t.statistics.iter().enumerate() {
                    out.push(r(&[
                        ("oid", int(rel_oid(t) + 900 + i as i64)),
                        ("stxrelid", int(rel_oid(t))),
                        ("stxname", text(st.name.clone())),
                        ("stxnamespace", int(NS_PUBLIC)),
                        ("stxowner", int(t.owner.as_deref().map_or(OWNER, role_oid))),
                        ("stxstattarget", Value::Null),
                        ("stxkeys", attnums(&st.columns)),
                        ("stxkind", Value::Array(vec![text("d"), text("f")])),
                        ("stxexprs", Value::Null),
                    ]));
                }
            }
        }
        "pg_constraint" => {
            for c in constraints(&tables) {
                out.push(r(&[
                    ("oid", int(c.oid)),
                    ("conname", text(c.name)),
                    ("connamespace", int(NS_PUBLIC)),
                    ("contype", text(c.kind)),
                    ("condeferrable", Value::Bool(false)),
                    ("condeferred", Value::Bool(false)),
                    ("convalidated", Value::Bool(true)),
                    ("conrelid", int(rel_oid(&c.table))),
                    ("contypid", int(0)),
                    ("conindid", int(c.index)),
                    ("conparentid", int(0)),
                    ("confrelid", int(c.references.as_ref().map_or(0, rel_oid))),
                    ("confupdtype", text(c.on_update)),
                    ("confdeltype", text(c.on_delete)),
                    ("confmatchtype", text(if c.kind == "f" { "s" } else { " " })),
                    ("conislocal", Value::Bool(true)),
                    ("coninhcount", int(0)),
                    ("connoinherit", Value::Bool(false)),
                    (
                        "conkey",
                        if c.cols.is_empty() {
                            Value::Null
                        } else {
                            attnums(&c.cols)
                        },
                    ),
                    (
                        "confkey",
                        if c.ref_cols.is_empty() {
                            Value::Null
                        } else {
                            attnums(&c.ref_cols)
                        },
                    ),
                    (
                        "conbin",
                        if c.kind == "c" {
                            text(c.def)
                        } else {
                            Value::Null
                        },
                    ),
                ]));
            }
        }
        "pg_roles" => {
            let roles = catalog::roles(txn)?;
            if roles.is_empty() {
                // An unclaimed database: its bootstrap administrator.
                out.push(r(&[
                    ("oid", int(OWNER)),
                    ("rolname", text(ROLE)),
                    ("rolsuper", Value::Bool(true)),
                    ("rolinherit", Value::Bool(true)),
                    ("rolcreaterole", Value::Bool(true)),
                    ("rolcreatedb", Value::Bool(true)),
                    ("rolcanlogin", Value::Bool(true)),
                    ("rolreplication", Value::Bool(false)),
                    ("rolconnlimit", int(-1)),
                    ("rolpassword", text("********")),
                    ("rolbypassrls", Value::Bool(true)),
                ]));
            }
            for role in roles {
                out.push(r(&[
                    ("oid", int(role_oid(&role.name))),
                    ("rolname", text(&role.name)),
                    ("rolsuper", Value::Bool(role.superuser)),
                    ("rolinherit", Value::Bool(role.inherit)),
                    ("rolcreaterole", Value::Bool(role.createrole)),
                    ("rolcreatedb", Value::Bool(role.superuser)),
                    ("rolcanlogin", Value::Bool(role.login)),
                    ("rolreplication", Value::Bool(false)),
                    (
                        "rolconnlimit",
                        int(role.connection_limit.map_or(-1, i64::from)),
                    ),
                    ("rolpassword", text("********")),
                    (
                        "rolvaliduntil",
                        role.valid_until.map_or(Value::Null, Value::TimestampTz),
                    ),
                    ("rolbypassrls", Value::Bool(role.superuser)),
                ]));
            }
        }
        "pg_auth_members" => {
            for m in catalog::memberships(txn)? {
                out.push(r(&[
                    ("oid", int(role_oid(&format!("{}\0{}", m.role, m.member)))),
                    ("roleid", int(role_oid(&m.role))),
                    ("member", int(role_oid(&m.member))),
                    ("grantor", int(OWNER)),
                    ("admin_option", Value::Bool(m.admin_option)),
                ]));
            }
        }
        "pg_database" => {
            out.push(r(&[
                ("oid", int(DATABASE_OID)),
                ("datname", text("nucleus")),
                ("datdba", int(OWNER)),
                ("encoding", int(6)),
                ("datlocprovider", text("c")),
                ("datistemplate", Value::Bool(false)),
                ("datallowconn", Value::Bool(true)),
                ("datconnlimit", int(-1)),
                ("datfrozenxid", int(0)),
                ("datminmxid", int(1)),
                ("dattablespace", int(1663)),
                ("datcollate", text("C")),
                ("datctype", text("C")),
            ]));
        }
        "pg_tablespace" => {
            for (oid, name) in [(1663, "pg_default"), (1664, "pg_global")] {
                out.push(r(&[
                    ("oid", int(oid)),
                    ("spcname", text(name)),
                    ("spcowner", int(OWNER)),
                ]));
            }
        }
        "pg_tables" => {
            for t in &tables {
                out.push(r(&[
                    ("schemaname", text("public")),
                    ("tablename", text(t.name.clone())),
                    ("tableowner", text(ROLE)),
                    (
                        "hasindexes",
                        Value::Bool(!t.primary_key.is_empty() || !t.indexes.is_empty()),
                    ),
                    ("hasrules", Value::Bool(false)),
                    (
                        "hastriggers",
                        Value::Bool(!t.foreign_keys.is_empty() || !t.referenced_by.is_empty()),
                    ),
                    ("rowsecurity", Value::Bool(false)),
                ]));
            }
        }
        "pg_indexes" => {
            for t in &tables {
                let mut list = Vec::new();
                if !t.primary_key.is_empty() {
                    list.push(None);
                }
                list.extend((0..t.indexes.len()).map(Some));
                for i in list {
                    let name =
                        i.map_or_else(|| format!("{}_pkey", t.name), |i| t.indexes[i].name.clone());
                    out.push(r(&[
                        ("schemaname", text("public")),
                        ("tablename", text(t.name.clone())),
                        ("indexname", text(name)),
                        ("indexdef", text(index_def(t, i))),
                    ]));
                }
            }
        }
        "schemata" => {
            for ns in [NS_CATALOG, NS_PUBLIC, NS_INFO, NS_NC] {
                out.push(r(&[
                    ("catalog_name", text("nucleus")),
                    ("schema_name", text(ns_name(ns).expect("known"))),
                    ("schema_owner", text(ROLE)),
                ]));
            }
        }
        "tables" => {
            for t in &tables {
                out.push(r(&[
                    ("table_catalog", text("nucleus")),
                    ("table_schema", text("public")),
                    ("table_name", text(t.name.clone())),
                    ("table_type", text("BASE TABLE")),
                    ("is_insertable_into", text("YES")),
                    ("is_typed", text("NO")),
                ]));
            }
            for d in DEFS {
                out.push(r(&[
                    ("table_catalog", text("nucleus")),
                    ("table_schema", text(ns_name(d.ns).expect("known"))),
                    ("table_name", text(d.name)),
                    (
                        "table_type",
                        text(if d.ns == NS_INFO {
                            "VIEW"
                        } else {
                            "BASE TABLE"
                        }),
                    ),
                    ("is_insertable_into", text("NO")),
                    ("is_typed", text("NO")),
                ]));
            }
        }
        "columns" => {
            for t in &tables {
                for (c, col) in t.columns.iter().enumerate() {
                    let nullable = col.nullable && !t.primary_key.contains(&c);
                    let (precision, scale) = match (col.ty, col.numeric) {
                        (DataType::BigInt, _) => (Value::Int(64), Value::Int(0)),
                        (DataType::Float, _) => (Value::Int(53), Value::Null),
                        (DataType::Numeric, Some((p, s))) => (int(p), int(s)),
                        _ => (Value::Null, Value::Null),
                    };
                    let (oid, _) = type_of(col);
                    let udt = TYPES.iter().find(|x| x.0 == oid).map_or("", |x| x.1);
                    out.push(r(&[
                        ("table_catalog", text("nucleus")),
                        ("table_schema", text("public")),
                        ("table_name", text(t.name.clone())),
                        ("column_name", text(col.name.clone())),
                        ("ordinal_position", int(c as i64 + 1)),
                        (
                            "column_default",
                            col.default.clone().map_or(Value::Null, text),
                        ),
                        ("is_nullable", text(if nullable { "YES" } else { "NO" })),
                        ("data_type", text(data_type_name(col))),
                        (
                            "character_maximum_length",
                            col.max_len.map_or(Value::Null, int),
                        ),
                        ("numeric_precision", precision),
                        ("numeric_scale", scale),
                        (
                            "datetime_precision",
                            if col.ty.is_datetime() || col.ty == DataType::Interval {
                                int(if col.ty == DataType::Date { 0 } else { 6 })
                            } else {
                                Value::Null
                            },
                        ),
                        ("udt_name", text(udt)),
                        ("is_identity", text("NO")),
                        ("is_generated", text("NEVER")),
                    ]));
                }
            }
        }
        "table_constraints" | "key_column_usage" => {
            for c in constraints(&tables) {
                let kind = match c.kind {
                    "p" => "PRIMARY KEY",
                    "u" => "UNIQUE",
                    "c" => "CHECK",
                    _ => "FOREIGN KEY",
                };
                let common = [
                    ("constraint_catalog", text("nucleus")),
                    ("constraint_schema", text("public")),
                    ("constraint_name", text(c.name.clone())),
                    ("table_catalog", text("nucleus")),
                    ("table_schema", text("public")),
                    ("table_name", text(c.table.name.clone())),
                ];
                if short == "table_constraints" {
                    let mut vals = common.to_vec();
                    vals.push(("constraint_type", text(kind)));
                    vals.push(("is_deferrable", text("NO")));
                    vals.push(("initially_deferred", text("NO")));
                    out.push(r(&vals));
                } else {
                    for (i, &col) in c.cols.iter().enumerate() {
                        let mut vals = common.to_vec();
                        vals.push(("column_name", text(c.table.columns[col].name.clone())));
                        vals.push(("ordinal_position", int(i as i64 + 1)));
                        out.push(r(&vals));
                    }
                }
            }
        }
        // Objects nucleus does not have: always empty.
        _ => {}
    }
    Ok(out)
}

// ------------------------------------------------------------- functions

fn relation_by_oid(tables: &[Table], registry: &Registry, oid: i64) -> Option<String> {
    relations(tables, registry)
        .into_iter()
        .find(|r| r.oid == oid)
        .map(|r| r.name)
}

/// `'name'::regclass` / `'16384'::regclass`: the relation's oid.
fn regclass_oid(tables: &[Table], registry: &Registry, s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(oid) = s.parse::<i64>() {
        return Ok(oid);
    }
    let unquoted = s.trim_matches('"');
    let (schema, name) = match unquoted.split_once('.') {
        Some((a, b)) => (Some(a.trim_matches('"')), b.trim_matches('"')),
        None => (None, unquoted),
    };
    relations(tables, registry)
        .into_iter()
        .find(|r| r.name == name && schema.is_none_or(|sch| ns_name(r.ns) == Some(sch)))
        .map(|r| r.oid)
        .ok_or_else(|| Error::undefined_table(format!("relation \"{s}\" does not exist")))
}

/// The `pg-compat` functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PgFn {
    /// `x::regclass`: a name (or oid text) to an oid, an oid to a name.
    RegClass,
    /// `oid::regtype`: a type's name.
    RegType,
    /// `oid::regnamespace`: a schema's name.
    RegNamespace,
    /// `oid::regrole`: a role's name.
    RegRole,
    /// `pg_get_userbyid(oid)`.
    GetUserById,
    /// `pg_table_is_visible(oid)` (and the type / function variants).
    TableIsVisible,
    /// `format_type(type oid, typmod)`.
    FormatType,
    /// `pg_get_expr(expr, relation [, pretty])`.
    GetExpr,
    /// `pg_get_indexdef(index oid [, column, pretty])`.
    GetIndexdef,
    /// `pg_get_constraintdef(constraint oid [, pretty])`.
    GetConstraintdef,
    /// `pg_partition_ancestors(relation)`: the relation (nothing is
    /// partitioned).
    PartitionAncestors,
    /// `pg_get_statisticsobjdef_columns(oid)`: a `CREATE STATISTICS`'s
    /// columns.
    StatisticsObjColumns,
    /// `pg_relation_is_publishable(oid)` (always false).
    RelationIsPublishable,
    /// `pg_get_triggerdef(oid [, pretty])` (always NULL).
    GetTriggerdef,
    /// `pg_encoding_to_char(encoding)`.
    EncodingToChar,
    /// `obj_description(oid [, catalog])`: a table's comment.
    ObjDescription,
    /// `col_description(table oid, column number)`: a column's comment.
    ColDescription,
    /// `shobj_description(oid, catalog)` (NULL: no shared objects have
    /// comments).
    ShobjDescription,
    /// `has_table_privilege` and friends (always true: one role).
    HasPrivilege,
    /// `pg_backend_pid()`.
    BackendPid,
    /// `pg_typeof(x)`: the argument's type name, a constant.
    TypeOf,
}

/// The functions: names (the first is shown by `EXPLAIN`) and argument
/// counts.
const FUNCTIONS: &[(PgFn, &[&str], RangeInclusive<usize>)] = &[
    (PgFn::RegClass, &["__regclass"], 1..=1),
    (PgFn::RegType, &["__regtype"], 1..=1),
    (PgFn::RegNamespace, &["__regnamespace"], 1..=1),
    (PgFn::RegRole, &["__regrole"], 1..=1),
    (PgFn::GetUserById, &["pg_get_userbyid"], 1..=1),
    (
        PgFn::TableIsVisible,
        &[
            "pg_table_is_visible",
            "pg_type_is_visible",
            "pg_function_is_visible",
        ],
        1..=1,
    ),
    (PgFn::FormatType, &["format_type"], 2..=2),
    (PgFn::GetExpr, &["pg_get_expr"], 2..=3),
    (PgFn::GetIndexdef, &["pg_get_indexdef"], 1..=3),
    (PgFn::GetConstraintdef, &["pg_get_constraintdef"], 1..=2),
    (PgFn::PartitionAncestors, &["pg_partition_ancestors"], 1..=1),
    (
        PgFn::StatisticsObjColumns,
        &["pg_get_statisticsobjdef_columns"],
        1..=1,
    ),
    (
        PgFn::RelationIsPublishable,
        &["pg_relation_is_publishable"],
        1..=1,
    ),
    (PgFn::GetTriggerdef, &["pg_get_triggerdef"], 1..=2),
    (PgFn::EncodingToChar, &["pg_encoding_to_char"], 1..=1),
    (PgFn::ObjDescription, &["obj_description"], 1..=2),
    (PgFn::ColDescription, &["col_description"], 2..=2),
    (PgFn::ShobjDescription, &["shobj_description"], 2..=2),
    (
        PgFn::HasPrivilege,
        &[
            "has_table_privilege",
            "has_schema_privilege",
            "has_database_privilege",
            "has_column_privilege",
            "has_sequence_privilege",
            "has_function_privilege",
            "pg_has_role",
        ],
        1..=4,
    ),
    (PgFn::BackendPid, &["pg_backend_pid"], 0..=0),
    (PgFn::TypeOf, &["pg_typeof"], 1..=1),
];

struct PgFunction {
    f: PgFn,
    arity: RangeInclusive<usize>,
}

impl ScalarFunction for PgFunction {
    fn name(&self) -> &str {
        match self.f {
            PgFn::RegClass => "regclass",
            PgFn::RegType => "regtype",
            PgFn::RegNamespace => "regnamespace",
            PgFn::RegRole => "regrole",
            f => FUNCTIONS.iter().find(|(g, ..)| *g == f).expect("listed").1[0],
        }
    }

    fn arity(&self) -> RangeInclusive<usize> {
        self.arity.clone()
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let n = args.len();
        let keep = |t| Ok(Signature::keep(n, Some(t)));
        match self.f {
            // regclass: text (a name) to an oid, an oid to its name.
            PgFn::RegClass => keep(if args[0].ty == Some(I) { T } else { I }),
            PgFn::TableIsVisible | PgFn::RelationIsPublishable | PgFn::HasPrivilege => keep(B),
            PgFn::PartitionAncestors => Ok(Signature::new(
                vec![Arg::Untyped(I)],
                Returns::Type(Some(I)),
            )),
            PgFn::BackendPid => keep(I),
            PgFn::TypeOf => Ok(Signature::new(
                vec![Arg::Keep],
                Returns::Constant(Value::Text(match args[0].ty {
                    Some(DataType::BigInt) => "bigint".into(),
                    Some(t) => t.name().into(),
                    None => "unknown".into(),
                })),
            )),
            _ => keep(T),
        }
    }

    /// They take `NULL`s their own way.
    fn strict(&self) -> bool {
        false
    }

    /// They read the catalog.
    fn volatility(&self) -> Volatility {
        Volatility::Stable
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        call(self.f, args, cx.txn(), cx.registry())
    }
}

/// Evaluates a `pg-compat` function.
fn call(f: PgFn, args: &[Value], txn: &dyn KvTxn, registry: &Registry) -> Result<Value> {
    let as_int = |i: usize| match args.get(i) {
        Some(Value::Int(n)) => Some(*n),
        Some(Value::Numeric(d)) => d.to_i64().ok(),
        Some(Value::Text(s)) => s.trim().parse().ok(),
        _ => None,
    };
    Ok(match f {
        PgFn::RegClass => match args.first() {
            Some(Value::Text(s)) => Value::Int(regclass_oid(&catalog::tables(txn)?, registry, s)?),
            Some(Value::Int(oid)) => Value::Text(
                relation_by_oid(&catalog::tables(txn)?, registry, *oid)
                    .unwrap_or_else(|| oid.to_string()),
            ),
            _ => Value::Null,
        },
        PgFn::RegType => match as_int(0) {
            Some(oid) => Value::Text(format_type(oid, None).unwrap_or_else(|| oid.to_string())),
            None => Value::Null,
        },
        PgFn::RegNamespace => match as_int(0) {
            Some(oid) => Value::Text(ns_name(oid).map_or_else(|| oid.to_string(), str::to_string)),
            None => Value::Null,
        },
        PgFn::RegRole | PgFn::GetUserById => match as_int(0) {
            None => Value::Null,
            Some(oid) => {
                let roles = catalog::roles(txn)?;
                Value::Text(
                    roles
                        .into_iter()
                        .map(|r| r.name)
                        .find(|n| role_oid(n) == oid)
                        .unwrap_or_else(|| ROLE.into()),
                )
            }
        },
        // On the search path: public and pg_catalog.
        PgFn::TableIsVisible => match as_int(0) {
            Some(oid) => Value::Bool(
                relations(&catalog::tables(txn)?, registry)
                    .iter()
                    .find(|r| r.oid == oid)
                    .is_none_or(|r| r.ns == NS_PUBLIC || r.ns == NS_CATALOG),
            ),
            None => Value::Null,
        },
        PgFn::FormatType => match as_int(0) {
            Some(oid) => format_type(oid, as_int(1)).map_or(Value::Null, Value::Text),
            None => Value::Null,
        },
        PgFn::GetExpr => match args.first() {
            Some(Value::Text(s)) => Value::Text(s.clone()),
            _ => Value::Null,
        },
        PgFn::GetIndexdef => {
            let Some(oid) = as_int(0) else {
                return Ok(Value::Null);
            };
            let tables = catalog::tables(txn)?;
            let column = as_int(1).filter(|c| *c > 0);
            relations(&tables, registry)
                .into_iter()
                .find(|r| r.oid == oid && r.kind == "i")
                .and_then(|r| match column {
                    // pg_get_indexdef(index, n, …): the n-th key column.
                    Some(n) => r.cols.get(n as usize - 1).map(|c| c.0.clone()),
                    None => r.table.map(|(t, i)| index_def(&t, i.flatten())),
                })
                .map_or(Value::Null, Value::Text)
        }
        PgFn::GetConstraintdef => {
            let Some(oid) = as_int(0) else {
                return Ok(Value::Null);
            };
            constraints(&catalog::tables(txn)?)
                .into_iter()
                .find(|c| c.oid == oid)
                .map_or(Value::Null, |c| Value::Text(c.def))
        }
        // No partitioning: a table is its own (only) ancestor.
        PgFn::PartitionAncestors => args.first().cloned().unwrap_or(Value::Null),
        PgFn::ObjDescription => {
            let class = match args.get(1) {
                Some(Value::Text(c)) => Some(c.as_str()),
                _ => None,
            };
            match as_int(0) {
                Some(oid) if class.is_none_or(|c| c == "pg_class") => catalog::tables(txn)?
                    .into_iter()
                    .find(|t| rel_oid(t) == oid)
                    .and_then(|t| t.comment)
                    .map_or(Value::Null, Value::Text),
                _ => Value::Null,
            }
        }
        PgFn::ColDescription => match (as_int(0), as_int(1)) {
            (Some(oid), Some(n)) if n > 0 => catalog::tables(txn)?
                .into_iter()
                .find(|t| rel_oid(t) == oid)
                .and_then(|t| {
                    t.columns
                        .get(n as usize - 1)
                        .and_then(|c| c.comment.clone())
                })
                .map_or(Value::Null, Value::Text),
            _ => Value::Null,
        },
        PgFn::StatisticsObjColumns => {
            let Some(oid) = as_int(0) else {
                return Ok(Value::Null);
            };
            catalog::tables(txn)?
                .iter()
                .find_map(|t| {
                    let i = usize::try_from(oid - rel_oid(t) - 900).ok()?;
                    let st = t.statistics.get(i)?;
                    let names: Vec<&str> = st
                        .columns
                        .iter()
                        .map(|&c| t.columns[c].name.as_str())
                        .collect();
                    Some(Value::Text(names.join(", ")))
                })
                .unwrap_or(Value::Null)
        }
        PgFn::GetTriggerdef | PgFn::ShobjDescription => Value::Null,
        PgFn::RelationIsPublishable => Value::Bool(false),
        PgFn::EncodingToChar => {
            if as_int(0) == Some(6) {
                Value::Text("UTF8".into())
            } else {
                Value::Text(String::new())
            }
        }
        PgFn::HasPrivilege => Value::Bool(true),
        PgFn::BackendPid => Value::Int(i64::from(std::process::id())),
        PgFn::TypeOf => unreachable!("a constant"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(format_type(20, None).as_deref(), Some("bigint"));
        assert_eq!(
            format_type(1043, Some(24)).as_deref(),
            Some("character varying(20)")
        );
        assert_eq!(
            format_type(1700, Some(((10 << 16) | 2) + 4)).as_deref(),
            Some("numeric(10,2)")
        );
        assert_eq!(format_type(1009, None).as_deref(), Some("text[]"));
        assert_eq!(format_type(424242, None), None);
        // Every view is registered, pg_catalog's found unqualified too.
        let mut r = Registry::builtin();
        r.add(&PgCompat).unwrap();
        for d in DEFS {
            assert!(r.view(ns_name(d.ns), d.name).is_some());
            assert_eq!(r.view(None, d.name).is_some(), d.ns == NS_CATALOG);
        }
    }
}
