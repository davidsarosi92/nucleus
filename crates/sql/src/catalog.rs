//! The catalog — table definitions — and the keyspace layout.  Both live in
//! the KV store (PLAN.md principle 8), so DDL is transactional like any
//! other write.
//!
//! **Keyspace:**
//!
//! | Key | Value |
//! |-----|-------|
//! | `00 00` | the format record: [`KEYSPACE_VERSION`] and [`KEY_ENCODING_VERSION`], `u16` each |
//! | `00 01 <table name>` | the table definition |
//! | `00 02 <sequence name>` | a `u64` counter (`table_id`) |
//! | `00 03 <table id u32 BE>` | next hidden row id of a table without a primary key |
//! | `00 04 <table id u32 BE>` | table statistics (§M13) |
//! | `00 05 <type name> 00 <ordering name>` | an ordering taught to a type (`ALTER TYPE … ADD ORDERING`): its mapping |
//! | `00 06 <symbol> 00 <left type> 00 <right type>` | an operator taught (`ALTER TYPE … ADD OPERATOR`): its function |
//! | `00 07 <from type> 00 <to type>` | a cast taught (`ALTER TYPE … ADD CAST`): its function and context |
//! | `00 08 <type name>` | the oldest encoding version a registered type's stored values may be in (`u16`) |
//! | `01 <table id u32 BE> <index id u32 BE> <key>` | table and index entries |
//!
//! Index 0 of a table is its primary index: key = the primary key columns
//! (or a hidden row id), value = the encoded row.  Secondary index `n`: key =
//! the index columns followed by the primary key, value empty — so entries
//! are unique even for duplicate index values, and the row is one more
//! lookup away.  A `UNIQUE` index checks for an existing entry with the same
//! non-NULL index columns before inserting.

use std::ops::Bound;

use nucleus_kv::{KvTxn, prefix_end};

use crate::ast::FkAction;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::types::{DataType, Value};

const FORMAT: &[u8] = &[0x00, 0x00];
const CATALOG: &[u8] = &[0x00, 0x01];
const SEQUENCE: &[u8] = &[0x00, 0x02];
const ROWID: &[u8] = &[0x00, 0x03];
const STATS: &[u8] = &[0x00, 0x04];
const ORDERINGS: &[u8] = &[0x00, 0x05];
const ORDERING_VERSION: u8 = 1;
const OPERATORS: &[u8] = &[0x00, 0x06];
const CASTS: &[u8] = &[0x00, 0x07];
const TYPE_VERSIONS: &[u8] = &[0x00, 0x08];

/// The oldest encoding version values of registered type `name` may be
/// stored in (`None`: no column of it was ever created).
pub fn stored_version(txn: &dyn KvTxn, name: &str) -> Result<Option<u16>> {
    txn.get(&names_key(TYPE_VERSIONS, &[name]))?
        .map(|v| Reader::new(&v).u16())
        .transpose()
}

/// Records that values of `name` may be stored in `version` and later.
pub fn put_stored_version(txn: &mut dyn KvTxn, name: &str, version: u16) -> Result<()> {
    let mut w = Writer::default();
    w.u16(version);
    txn.put(&names_key(TYPE_VERSIONS, &[name]), &w.0)?;
    Ok(())
}

/// Notes the registered types among `columns` as stored: their current
/// encoding version, unless an older one is on record.
pub fn note_stored_types(txn: &mut dyn KvTxn, columns: &[Column]) -> Result<()> {
    for c in columns {
        if let DataType::Ext(t) = c.ty
            && stored_version(&*txn, t.name())?.is_none()
        {
            put_stored_version(txn, t.name(), t.imp().encoding_version())?;
        }
    }
    Ok(())
}
const DATA: u8 = 0x01;
/// Version 2 added `NUMERIC(p, s)` modifiers, version 3 foreign keys,
/// version 4 registered types (tag `7F` and the type's name), version 5
/// indexes in taught orderings, version 6 `smallint` and `integer` (type
/// tags 10 and 11: before it, every integer column was `bigint`, tag 2,
/// and still reads as one) and `real` (tag 12; before, `double
/// precision`'s tag 3), `bytea` (tag 13), `time` (14) and `char(n)` (15;
/// before, `text`'s 4, and still read as text), `inet` and `cidr` (16, 17);
/// version 7 an owner, version 8 row-level security, version 9 comments
/// on the table and its columns; older versions still read.
const TABLE_VERSION: u8 = 12;

/// The version of the keyspace layout above (PLAN.md principle 6).
pub const KEYSPACE_VERSION: u16 = 1;
/// The version of the key encoding ([`crate::encoding`]).  Version 2: a
/// `NULL` array element sorts after the others (version 1 put it first,
/// which the law kit caught).
pub const KEY_ENCODING_VERSION: u16 = 2;

/// Checks the format record; on a database the SQL layer has not used yet,
/// writes it (unless `txn` is read-only: there is nothing to misread yet).
/// A database written in a newer format is refused rather than misread;
/// one in an older key encoding is upgraded — the keys of every table with
/// an array in a key are rewritten — by the first transaction that may
/// write.  Returns whether the record is in place and current.
pub fn check_format(txn: &mut dyn KvTxn, reg: &crate::ext::Registry) -> Result<bool> {
    let current = || {
        let mut w = Writer::default();
        w.u16(KEYSPACE_VERSION).u16(KEY_ENCODING_VERSION);
        w.0
    };
    let Some(b) = txn.get(FORMAT)? else {
        return match txn.put(FORMAT, &current()) {
            Ok(()) => Ok(true),
            Err(nucleus_kv::Error::ReadOnly) => Ok(false),
            Err(e) => Err(e.into()),
        };
    };
    let mut r = Reader::new(&b);
    let (keyspace, keys) = (r.u16()?, r.u16()?);
    if keyspace > KEYSPACE_VERSION || keys > KEY_ENCODING_VERSION {
        return Err(Error::feature(format!(
            "the database is in a newer SQL format (keyspace {keyspace}, key encoding \
             {keys}) than this build reads (keyspace {KEYSPACE_VERSION}, key encoding \
             {KEY_ENCODING_VERSION}); upgrade nucleus"
        )));
    }
    if keys < KEY_ENCODING_VERSION {
        if let Err(e) = txn.put(FORMAT, &current()) {
            return match e {
                nucleus_kv::Error::ReadOnly => Ok(false),
                e => Err(e.into()),
            };
        }
        for t in tables(&*txn)? {
            let array_key = t
                .primary_key
                .iter()
                .chain(t.indexes.iter().flat_map(|i| &i.columns))
                .any(|&c| t.columns[c].ty.element().is_some());
            if array_key {
                crate::dml::rewrite_keys(txn, reg, &t)?;
            }
        }
    }
    // Registered types: refuse values newer than this build; rewrite those
    // whose upgrade changes their keys before anything uses the keys.
    for (ty, _) in reg.types() {
        let DataType::Ext(t) = *ty else { continue };
        let Some(stored) = stored_version(&*txn, t.name())? else {
            continue;
        };
        let current = t.imp().encoding_version();
        if stored > current {
            return Err(Error::feature(format!(
                "the database stores type {} in encoding version {stored}, newer than this \
                 build's ({current}); upgrade nucleus",
                t.name()
            )));
        }
        if stored < current && t.imp().upgrade_changes_keys(stored) {
            match put_stored_version(txn, t.name(), stored) {
                Err(e) if e.code == "25006" => {
                    return Err(Error::read_only(format!(
                        "type {}'s keys change with its encoding version {current}: a writing \
                         statement (or ALTER TYPE {} UPGRADE) upgrades the database first",
                        t.name(),
                        t.name()
                    )));
                }
                Err(e) => return Err(e),
                Ok(()) => upgrade_type(txn, reg, t)?,
            }
        }
    }
    Ok(true)
}

/// Rewrites every table with a column of registered type `t` — rows in its
/// current encoding version, and their keys — and records the version.
pub fn upgrade_type(
    txn: &mut dyn KvTxn,
    reg: &crate::ext::Registry,
    t: crate::ext::TypeRef,
) -> Result<()> {
    for table in tables(&*txn)? {
        if table.columns.iter().any(|c| c.ty == DataType::Ext(t)) {
            crate::dml::rewrite_keys(txn, reg, &table)?;
        }
    }
    put_stored_version(txn, t.name(), t.imp().encoding_version())
}

/// A column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// Name (lowercase unless quoted).
    pub name: String,
    /// Type.
    pub ty: DataType,
    /// Whether `NULL` is allowed.
    pub nullable: bool,
    /// The `DEFAULT` expression, as SQL text.
    pub default: Option<String>,
    /// `VARCHAR(n)`: the longest value allowed, in characters.
    pub max_len: Option<u32>,
    /// `NUMERIC(precision, scale)`.
    pub numeric: Option<(u32, u32)>,
    /// `COMMENT ON COLUMN`.
    pub comment: Option<String>,
}

/// An index.
#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    /// Id within its table (≥ 1; 0 is the primary index).
    pub id: u32,
    /// Name, unique in the database.
    pub name: String,
    /// The indexed columns, by position.
    pub columns: Vec<usize>,
    /// Whether two rows may not share non-NULL index values.
    pub unique: bool,
    /// Per column, the taught ordering of its type the column is indexed
    /// in (`CREATE INDEX … (col USING ordering)`): its entries hold the
    /// column's mapping.  `None`: the value itself.
    pub orderings: Vec<Option<String>>,
}

impl Index {
    /// A plain index (every column by its own value).
    pub fn plain(id: u32, name: String, columns: Vec<usize>, unique: bool) -> Self {
        let orderings = vec![None; columns.len()];
        Self {
            id,
            name,
            columns,
            unique,
            orderings,
        }
    }

    /// Whether some column is indexed in a taught ordering.
    pub fn mapped(&self) -> bool {
        self.orderings.iter().any(Option::is_some)
    }
}

/// A foreign key: columns of this table that must match a row of another
/// (or the same) table.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKey {
    /// Constraint name.
    pub name: String,
    /// The referencing columns, by position.
    pub columns: Vec<usize>,
    /// The referenced table.
    pub table: String,
    /// The referenced columns (its primary key or a unique index), by
    /// position, matching `columns` one to one.
    pub ref_columns: Vec<usize>,
    /// `ON DELETE`.
    pub on_delete: FkAction,
    /// `ON UPDATE`.
    pub on_update: FkAction,
}

/// A table definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// Id, unique in the database, never reused.
    pub id: u32,
    /// Name.
    pub name: String,
    /// Columns in order.
    pub columns: Vec<Column>,
    /// The primary key columns by position; empty: a hidden row id.
    pub primary_key: Vec<usize>,
    /// Secondary indexes.
    pub indexes: Vec<Index>,
    /// `CHECK` constraints: (name, SQL text of the expression).
    pub checks: Vec<(String, String)>,
    /// The id the next index gets.
    pub next_index_id: u32,
    /// Foreign keys of this table.
    pub foreign_keys: Vec<ForeignKey>,
    /// The foreign keys referencing this table: (table, constraint name).
    pub referenced_by: Vec<(String, String)>,
    /// The role that owns it (`None`: made before roles, owned by the
    /// administrators).
    pub owner: Option<String>,
    /// Row-level security is on ([`crate::policies`]).
    pub row_security: bool,
    /// … for its owner too.
    pub force_row_security: bool,
    /// `COMMENT ON TABLE`.
    pub comment: Option<String>,
    /// How its rows are stored: a registered [`crate::storage::TableStorage`]
    /// (`row` by default).
    pub storage: String,
    /// `row, column`: the columns the column copy holds (empty: all).
    pub columnar: Vec<usize>,
    /// `row, column`: which copy is being built, if one is
    /// ([`crate::column_store::CopyState`]).
    pub copy: crate::column_store::CopyState,
    /// `PARTITION BY …`: a partitioned table (its rows in its partitions).
    pub partition_by: Option<crate::partition::Spec>,
    /// `PARTITION OF parent FOR VALUES …`: a partition, of which table, its
    /// bound.
    pub partition_of: Option<(String, crate::partition::PartBound)>,
    /// A partitioned table's storage policy: its partitions whose range
    /// ended longer ago than `after` are stored as `storage`.
    pub storage_policy: Option<(String, crate::datetime::Interval)>,
}

impl Table {
    /// The position of column `name`.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The key prefix of index `index_id` (0 = primary).
    pub fn index_prefix(&self, index_id: u32) -> Vec<u8> {
        let mut k = vec![DATA];
        k.extend_from_slice(&self.id.to_be_bytes());
        k.extend_from_slice(&index_id.to_be_bytes());
        k
    }

    /// The prefix of every key of the table.
    pub fn data_prefix(&self) -> Vec<u8> {
        let mut k = vec![DATA];
        k.extend_from_slice(&self.id.to_be_bytes());
        k
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(TABLE_VERSION)
            .u32(self.id)
            .str(&self.name)
            .u32(self.columns.len() as u32);
        for c in &self.columns {
            w.str(&c.name).u8(type_tag(c.ty));
            if let DataType::Ext(t) = c.ty {
                w.str(t.name());
            }
            if let DataType::Enum(e) = c.ty {
                w.str(e.name());
            }
            w.u8(u8::from(c.nullable))
                .opt_str(c.default.as_deref())
                .u32(c.max_len.map_or(0, |n| n + 1));
            let (p, s) = c.numeric.map_or((0, 0), |(p, s)| (p + 1, s));
            w.u32(p).u32(s);
        }
        w.u32(self.primary_key.len() as u32);
        for &p in &self.primary_key {
            w.u32(p as u32);
        }
        w.u32(self.indexes.len() as u32);
        for i in &self.indexes {
            w.u32(i.id)
                .str(&i.name)
                .u8(u8::from(i.unique))
                .u32(i.columns.len() as u32);
            for &c in &i.columns {
                w.u32(c as u32);
            }
            for o in &i.orderings {
                w.opt_str(o.as_deref());
            }
        }
        w.u32(self.checks.len() as u32);
        for (name, expr) in &self.checks {
            w.str(name).str(expr);
        }
        w.u32(self.next_index_id);
        w.u32(self.foreign_keys.len() as u32);
        for fk in &self.foreign_keys {
            w.str(&fk.name).u32(fk.columns.len() as u32);
            for &c in &fk.columns {
                w.u32(c as u32);
            }
            w.str(&fk.table).u32(fk.ref_columns.len() as u32);
            for &c in &fk.ref_columns {
                w.u32(c as u32);
            }
            w.u8(action_tag(fk.on_delete)).u8(action_tag(fk.on_update));
        }
        w.u32(self.referenced_by.len() as u32);
        for (t, c) in &self.referenced_by {
            w.str(t).str(c);
        }
        w.opt_str(self.owner.as_deref());
        w.u8(u8::from(self.row_security) | u8::from(self.force_row_security) << 1);
        w.opt_str(self.comment.as_deref());
        for c in &self.columns {
            w.opt_str(c.comment.as_deref());
        }
        w.str(&self.storage);
        w.u32(self.columnar.len() as u32);
        for &c in &self.columnar {
            w.u32(c as u32);
        }
        w.u8(self.copy.tag());
        match &self.partition_by {
            None => {
                w.u8(0);
            }
            Some(spec) => {
                w.u8(1);
                spec.encode(&mut w);
            }
        }
        match &self.partition_of {
            None => {
                w.u8(0);
            }
            Some((parent, bound)) => {
                w.u8(1).str(parent);
                bound.encode(&mut w);
            }
        }
        match &self.storage_policy {
            None => {
                w.u8(0);
            }
            Some((storage, after)) => {
                w.u8(1).str(storage).value(&Value::Interval(*after));
            }
        }
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=TABLE_VERSION).contains(&version) {
            return Err(Error::internal("unknown table definition version"));
        }
        let id = r.u32()?;
        let name = r.str()?;
        let n = r.u32()? as usize;
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r.str()?;
            let ty = match r.u8()? {
                EXT_TAG => DataType::Ext(crate::ext::TypeRef::named_or_missing(&r.str()?)),
                ENUM_TAG => DataType::Enum(crate::enums::EnumRef::named(&r.str()?)),
                tag => type_of_tag(tag)?,
            };
            let nullable = r.u8()? == 1;
            let default = r.opt_str()?;
            let max_len = match r.u32()? {
                0 => None,
                n => Some(n - 1),
            };
            let numeric = if version >= 2 {
                match (r.u32()?, r.u32()?) {
                    (0, _) => None,
                    (p, s) => Some((p - 1, s)),
                }
            } else {
                None
            };
            columns.push(Column {
                name,
                ty,
                nullable,
                default,
                max_len,
                numeric,
                comment: None,
            });
        }
        let n = r.u32()? as usize;
        let primary_key = (0..n)
            .map(|_| r.u32().map(|p| p as usize))
            .collect::<Result<_>>()?;
        let n = r.u32()? as usize;
        let mut indexes = Vec::with_capacity(n);
        for _ in 0..n {
            let id = r.u32()?;
            let name = r.str()?;
            let unique = r.u8()? == 1;
            let m = r.u32()? as usize;
            let columns = (0..m)
                .map(|_| r.u32().map(|c| c as usize))
                .collect::<Result<_>>()?;
            let orderings = if version >= 5 {
                (0..m).map(|_| r.opt_str()).collect::<Result<_>>()?
            } else {
                vec![None; m]
            };
            indexes.push(Index {
                id,
                name,
                columns,
                unique,
                orderings,
            });
        }
        let n = r.u32()? as usize;
        let checks = (0..n)
            .map(|_| Ok((r.str()?, r.str()?)))
            .collect::<Result<_>>()?;
        let next_index_id = r.u32()?;
        let (mut foreign_keys, mut referenced_by) = (Vec::new(), Vec::new());
        if version >= 3 {
            let positions = |r: &mut Reader| -> Result<Vec<usize>> {
                let n = r.u32()? as usize;
                (0..n).map(|_| r.u32().map(|c| c as usize)).collect()
            };
            for _ in 0..r.u32()? {
                let name = r.str()?;
                let columns = positions(&mut r)?;
                let table = r.str()?;
                let ref_columns = positions(&mut r)?;
                foreign_keys.push(ForeignKey {
                    name,
                    columns,
                    table,
                    ref_columns,
                    on_delete: action_of_tag(r.u8()?)?,
                    on_update: action_of_tag(r.u8()?)?,
                });
            }
            for _ in 0..r.u32()? {
                referenced_by.push((r.str()?, r.str()?));
            }
        }
        let owner = if version >= 7 { r.opt_str()? } else { None };
        let rls = if version >= 8 { r.u8()? } else { 0 };
        let mut comment = None;
        if version >= 9 {
            comment = r.opt_str()?;
            for c in &mut columns {
                c.comment = r.opt_str()?;
            }
        }
        let storage = if version >= 10 {
            r.str()?
        } else {
            crate::storage::ROW.to_string()
        };
        let (mut columnar, mut copy) = (Vec::new(), crate::column_store::CopyState::Ready);
        if version >= 11 {
            for _ in 0..r.u32()? {
                columnar.push(r.u32()? as usize);
            }
            copy = crate::column_store::CopyState::of_tag(r.u8()?)?;
        }
        let (mut partition_by, mut partition_of, mut storage_policy) = (None, None, None);
        if version >= 12 {
            if r.u8()? == 1 {
                partition_by = Some(crate::partition::Spec::decode(&mut r)?);
            }
            if r.u8()? == 1 {
                let parent = r.str()?;
                partition_of = Some((parent, crate::partition::PartBound::decode(&mut r)?));
            }
            if r.u8()? == 1 {
                let storage = r.str()?;
                let Value::Interval(after) = r.value()? else {
                    return Err(Error::internal("a storage policy without its interval"));
                };
                storage_policy = Some((storage, after));
            }
        }
        Ok(Self {
            id,
            name,
            columns,
            primary_key,
            indexes,
            checks,
            next_index_id,
            foreign_keys,
            referenced_by,
            owner,
            row_security: rls & 1 != 0,
            force_row_security: rls & 2 != 0,
            comment,
            storage,
            columnar,
            copy,
            partition_by,
            partition_of,
            storage_policy,
        })
    }
}

fn action_tag(a: FkAction) -> u8 {
    match a {
        FkAction::NoAction => 0,
        FkAction::Restrict => 1,
        FkAction::Cascade => 2,
        FkAction::SetNull => 3,
        FkAction::SetDefault => 4,
    }
}

fn action_of_tag(t: u8) -> Result<FkAction> {
    Ok(match t {
        0 => FkAction::NoAction,
        1 => FkAction::Restrict,
        2 => FkAction::Cascade,
        3 => FkAction::SetNull,
        4 => FkAction::SetDefault,
        _ => return Err(Error::internal("unknown foreign key action tag")),
    })
}

/// A registered type's column tag; its name follows.
const EXT_TAG: u8 = 0x7F;
/// An enum's column tag; its name follows.
const ENUM_TAG: u8 = 0x7E;

fn type_tag(t: DataType) -> u8 {
    match t {
        DataType::Ext(_) => EXT_TAG,
        DataType::Enum(_) => ENUM_TAG,
        DataType::Array(e) => 0x80 | type_tag(*e),
        DataType::Bool => 1,
        DataType::BigInt => 2,
        DataType::Float => 3,
        DataType::Text => 4,
        DataType::Numeric => 5,
        DataType::Date => 6,
        DataType::Timestamp => 7,
        DataType::TimestampTz => 8,
        DataType::Interval => 9,
        DataType::SmallInt => 10,
        DataType::Integer => 11,
        DataType::Real => 12,
        DataType::Bytea => 13,
        DataType::Time => 14,
        DataType::Char => 15,
        DataType::Inet => 16,
        DataType::Cidr => 17,
    }
}

fn type_of_tag(t: u8) -> Result<DataType> {
    if t & 0x80 != 0 {
        return type_of_tag(t & 0x7f)?
            .array_of()
            .ok_or_else(|| Error::internal("unknown column type tag"));
    }
    Ok(match t {
        1 => DataType::Bool,
        2 => DataType::BigInt,
        3 => DataType::Float,
        4 => DataType::Text,
        5 => DataType::Numeric,
        6 => DataType::Date,
        7 => DataType::Timestamp,
        8 => DataType::TimestampTz,
        9 => DataType::Interval,
        10 => DataType::SmallInt,
        11 => DataType::Integer,
        12 => DataType::Real,
        13 => DataType::Bytea,
        14 => DataType::Time,
        15 => DataType::Char,
        16 => DataType::Inet,
        17 => DataType::Cidr,
        _ => return Err(Error::internal("unknown column type tag")),
    })
}

fn table_key(name: &str) -> Vec<u8> {
    let mut k = CATALOG.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn id_key(prefix: &[u8], id: u32) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// Reads table `name`'s definition validated: the transaction's commit
/// fails if it changed since its snapshot — what a write does, so it does
/// not commit over a definition changed under it (a storage conversion's
/// switch, §M14.20).
pub fn validate_table(txn: &dyn KvTxn, name: &str) -> Result<()> {
    txn.get_validated(&table_key(name))?;
    Ok(())
}

/// The table named `name`, if it exists.
pub fn get_table(txn: &dyn KvTxn, name: &str) -> Result<Option<Table>> {
    if let Some(t) = HYPOTHETICAL.with(|h| h.borrow().clone())
        && t.name == name
    {
        return Ok(Some(t));
    }
    txn.get(&table_key(name))?
        .map(|b| Table::decode(&b))
        .transpose()
}

thread_local! {
    static HYPOTHETICAL: std::cell::RefCell<Option<Table>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `table` read in place of the stored table of its name —
/// a definition with a hypothetical index, for the index advisor to plan
/// with (`f` plans only; nothing is written).
pub fn with_hypothetical<T>(table: Table, f: impl FnOnce() -> T) -> T {
    let before = HYPOTHETICAL.with(|h| h.replace(Some(table)));
    let out = f();
    HYPOTHETICAL.with(|h| *h.borrow_mut() = before);
    out
}

/// The table named `name`, or `42P01`.
pub fn table(txn: &dyn KvTxn, name: &str) -> Result<Table> {
    match get_table(txn, name)? {
        Some(t) => Ok(t),
        None => Err(no_such_table(txn, name, &[])),
    }
}

/// `42P01` for `name`, with a hint: the table (or one of `views`) meant,
/// or the tables there are.
pub fn no_such_table(txn: &dyn KvTxn, name: &str, views: &[String]) -> Error {
    let tables: Vec<String> = tables(txn)
        .map(|ts| ts.into_iter().map(|t| t.name).collect())
        .unwrap_or_default();
    let mut names: Vec<&str> = tables.iter().map(String::as_str).collect();
    let e = Error::undefined_table(format!("relation \"{name}\" does not exist"));
    let hint = crate::hint::closest(
        name,
        names
            .iter()
            .copied()
            .chain(views.iter().map(String::as_str)),
    )
    .map(|c| format!("Perhaps you meant table \"{c}\"."))
    .or_else(|| {
        names.sort_unstable();
        match names.len() {
            0 => Some("There are no tables yet: CREATE TABLE makes one.".into()),
            1..=12 => Some(format!("The tables: {}.", names.join(", "))),
            _ => Some("SELECT name FROM nc_catalog.tables lists the tables.".into()),
        }
    });
    match hint {
        Some(h) => e.with_hint(h),
        None => e,
    }
}

/// Every table, by name.
pub fn tables(txn: &dyn KvTxn) -> Result<Vec<Table>> {
    let end = prefix_end(CATALOG).expect("catalog prefix");
    let mut out = Vec::new();
    for e in txn.scan(Bound::Included(CATALOG), Bound::Excluded(&end))? {
        out.push(Table::decode(&e?.1)?);
    }
    Ok(out)
}

/// The next value of sequence `name`.
pub fn next_sequence(txn: &mut dyn KvTxn, name: &str) -> Result<u64> {
    let mut key = SEQUENCE.to_vec();
    key.extend_from_slice(name.as_bytes());
    let next = txn
        .get(&key)?
        .map_or(1, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    txn.put(&key, &(next + 1).to_le_bytes())?;
    Ok(next)
}

/// The next hidden row id of `table`.
pub fn next_rowid(txn: &mut dyn KvTxn, table: &Table) -> Result<i64> {
    let key = id_key(ROWID, table.id);
    let next = txn
        .get(&key)?
        .map_or(1, |b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    txn.put(&key, &(next + 1).to_le_bytes())?;
    Ok(next as i64)
}

/// Stores a new table, giving it an id; fails if the name is taken.
pub fn create_table(txn: &mut dyn KvTxn, mut table: Table) -> Result<Table> {
    if get_table(txn, &table.name)?.is_some() {
        return Err(Error::duplicate_table(format!(
            "relation \"{}\" already exists",
            table.name
        )));
    }
    table.id = u32::try_from(next_sequence(txn, "table_id")?)
        .map_err(|_| Error::out_of_range("too many tables"))?;
    txn.put(&table_key(&table.name), &table.encode())?;
    Ok(table)
}

/// Replaces a table's definition (same name).
pub fn update_table(txn: &mut dyn KvTxn, table: &Table) -> Result<()> {
    txn.put(&table_key(&table.name), &table.encode())?;
    Ok(())
}

/// Deletes every entry in `[start, end)`; returns how many.
pub fn delete_range(txn: &mut dyn KvTxn, start: &[u8], end: Option<&[u8]>) -> Result<u64> {
    let end_bound = end.map_or(Bound::Unbounded, Bound::Excluded);
    let keys: Vec<Vec<u8>> = txn
        .scan(Bound::Included(start), end_bound)?
        .map(|e| e.map(|(k, _)| k))
        .collect::<nucleus_kv::Result<_>>()?;
    for k in &keys {
        txn.delete(k)?;
    }
    Ok(keys.len() as u64)
}

/// Drops a table and all its data.
pub fn drop_table(txn: &mut dyn KvTxn, table: &Table) -> Result<()> {
    let prefix = table.data_prefix();
    delete_range(txn, &prefix, prefix_end(&prefix).as_deref())?;
    txn.delete(&table_key(&table.name))?;
    txn.delete(&id_key(ROWID, table.id))?;
    txn.delete(&id_key(STATS, table.id))?;
    Ok(())
}

/// The table holding index `name`, if any.
pub fn find_index(txn: &dyn KvTxn, name: &str) -> Result<Option<(Table, usize)>> {
    for t in tables(txn)? {
        if let Some(i) = t.indexes.iter().position(|i| i.name == name) {
            return Ok(Some((t, i)));
        }
    }
    Ok(None)
}

/// The statistics key of a table.
pub fn stats_key(table: &Table) -> Vec<u8> {
    id_key(STATS, table.id)
}

/// What a constraint is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// The primary key.
    PrimaryKey,
    /// A `UNIQUE` constraint: `table.indexes[index]`.
    Unique(usize),
    /// `table.checks[index]`.
    Check(usize),
    /// `table.foreign_keys[index]`.
    ForeignKey(usize),
}

/// A constraint, described.
#[derive(Clone, Debug)]
pub struct Constraint<'a> {
    /// Its table.
    pub table: &'a Table,
    /// Its name.
    pub name: String,
    /// What it is.
    pub kind: ConstraintKind,
    /// Its definition, as SQL (`PRIMARY KEY (id)`, `CHECK (a > 0)`, …).
    pub definition: String,
}

impl Table {
    /// Whether `indexes[i]` is a `UNIQUE` constraint (the indexes those
    /// create are named `…_key`) rather than an index.
    pub fn is_unique_constraint(&self, i: usize) -> bool {
        self.indexes[i].unique && self.indexes[i].name.ends_with("_key")
    }
}

fn column_list(t: &Table, cols: &[usize]) -> String {
    cols.iter()
        .map(|&c| t.columns[c].name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every constraint of `tables`: per table its primary key, unique
/// constraints, checks and foreign keys, in that order.
pub fn constraints(tables: &[Table]) -> Vec<Constraint<'_>> {
    let mut out = Vec::new();
    for t in tables {
        let con = |name: String, kind, definition| Constraint {
            table: t,
            name,
            kind,
            definition,
        };
        if !t.primary_key.is_empty() {
            out.push(con(
                format!("{}_pkey", t.name),
                ConstraintKind::PrimaryKey,
                format!("PRIMARY KEY ({})", column_list(t, &t.primary_key)),
            ));
        }
        for (i, ix) in t.indexes.iter().enumerate() {
            if t.is_unique_constraint(i) {
                out.push(con(
                    ix.name.clone(),
                    ConstraintKind::Unique(i),
                    format!("UNIQUE ({})", column_list(t, &ix.columns)),
                ));
            }
        }
        for (j, (name, check)) in t.checks.iter().enumerate() {
            out.push(con(
                name.clone(),
                ConstraintKind::Check(j),
                format!("CHECK ({check})"),
            ));
        }
        for (j, fk) in t.foreign_keys.iter().enumerate() {
            let Some(parent) = tables.iter().find(|p| p.name == fk.table) else {
                continue;
            };
            let mut def = format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                column_list(t, &fk.columns),
                parent.name,
                column_list(parent, &fk.ref_columns)
            );
            if fk.on_update != FkAction::NoAction {
                def.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
            }
            if fk.on_delete != FkAction::NoAction {
                def.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
            }
            out.push(con(fk.name.clone(), ConstraintKind::ForeignKey(j), def));
        }
    }
    out
}

/// An ordering taught to a type: `ALTER TYPE t ADD ORDERING name BY
/// mapping`.  Values sort as their mappings do.
#[derive(Clone, Debug, PartialEq)]
pub struct TaughtOrdering {
    /// The type's name.
    pub type_name: String,
    /// The ordering's name.
    pub name: String,
    /// The mapping, as SQL over `value`.
    pub mapping: String,
}

fn ordering_key(type_name: &str, name: &str) -> Vec<u8> {
    let mut k = ORDERINGS.to_vec();
    k.extend_from_slice(type_name.as_bytes());
    k.push(0);
    k.extend_from_slice(name.as_bytes());
    k
}

/// The ordering `name` taught to `type_name`.
pub fn get_ordering(
    txn: &dyn KvTxn,
    type_name: &str,
    name: &str,
) -> Result<Option<TaughtOrdering>> {
    let Some(b) = txn.get(&ordering_key(type_name, name))? else {
        return Ok(None);
    };
    let mut r = Reader::new(&b);
    if r.u8()? != ORDERING_VERSION {
        return Err(Error::internal("unknown ordering record version"));
    }
    Ok(Some(TaughtOrdering {
        type_name: type_name.into(),
        name: name.into(),
        mapping: r.str()?,
    }))
}

/// Every taught ordering, by type and name.
pub fn orderings(txn: &dyn KvTxn) -> Result<Vec<TaughtOrdering>> {
    let end = prefix_end(ORDERINGS).expect("orderings prefix");
    let mut out = Vec::new();
    for e in txn.scan(Bound::Included(ORDERINGS), Bound::Excluded(&end))? {
        let (k, v) = e?;
        let rest = &k[ORDERINGS.len()..];
        let split = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::internal("malformed ordering key"))?;
        let text =
            |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| Error::internal("bad name"));
        let mut r = Reader::new(&v);
        r.u8()?;
        out.push(TaughtOrdering {
            type_name: text(&rest[..split])?,
            name: text(&rest[split + 1..])?,
            mapping: r.str()?,
        });
    }
    Ok(out)
}

/// Stores (or replaces) a taught ordering.
pub fn put_ordering(txn: &mut dyn KvTxn, o: &TaughtOrdering) -> Result<()> {
    let mut w = Writer::default();
    w.u8(ORDERING_VERSION).str(&o.mapping);
    txn.put(&ordering_key(&o.type_name, &o.name), &w.0)?;
    Ok(())
}

/// Forgets a taught ordering.
pub fn delete_ordering(txn: &mut dyn KvTxn, type_name: &str, name: &str) -> Result<()> {
    txn.delete(&ordering_key(type_name, name))?;
    Ok(())
}

/// The parts of a key `prefix name 00 name …`.
fn key_parts(k: &[u8], prefix: &[u8]) -> Result<Vec<String>> {
    k[prefix.len()..]
        .split(|&b| b == 0)
        .map(|p| String::from_utf8(p.to_vec()).map_err(|_| Error::internal("bad name")))
        .collect()
}

fn names_key(prefix: &[u8], names: &[&str]) -> Vec<u8> {
    let mut k = prefix.to_vec();
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            k.push(0);
        }
        k.extend_from_slice(n.as_bytes());
    }
    k
}

fn scan_prefix(txn: &dyn KvTxn, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = prefix_end(prefix).expect("a catalog prefix");
    let mut out = Vec::new();
    for e in txn.scan(Bound::Included(prefix), Bound::Excluded(&end))? {
        out.push(e?);
    }
    Ok(out)
}

/// An operator taught: `ALTER TYPE t ADD OPERATOR symbol (left, right) AS
/// function`.
#[derive(Clone, Debug, PartialEq)]
pub struct TaughtOperator {
    /// The symbol (`<->`).
    pub symbol: String,
    /// The left operand's type name.
    pub left: String,
    /// The right operand's type name.
    pub right: String,
    /// The function it calls with the two operands.
    pub function: String,
}

/// The operator `symbol` taught for these operand type names.
pub fn get_operator(
    txn: &dyn KvTxn,
    symbol: &str,
    left: &str,
    right: &str,
) -> Result<Option<TaughtOperator>> {
    txn.get(&names_key(OPERATORS, &[symbol, left, right]))?
        .map(|v| {
            Reader::new(&v).str().map(|function| TaughtOperator {
                symbol: symbol.into(),
                left: left.into(),
                right: right.into(),
                function,
            })
        })
        .transpose()
}

/// Every taught operator.
pub fn operators(txn: &dyn KvTxn) -> Result<Vec<TaughtOperator>> {
    scan_prefix(txn, OPERATORS)?
        .into_iter()
        .map(|(k, v)| {
            let p = key_parts(&k, OPERATORS)?;
            let [symbol, left, right] = <[String; 3]>::try_from(p)
                .map_err(|_| Error::internal("malformed operator key"))?;
            Ok(TaughtOperator {
                symbol,
                left,
                right,
                function: Reader::new(&v).str()?,
            })
        })
        .collect()
}

/// Stores a taught operator.
pub fn put_operator(txn: &mut dyn KvTxn, o: &TaughtOperator) -> Result<()> {
    let mut w = Writer::default();
    w.str(&o.function);
    txn.put(&names_key(OPERATORS, &[&o.symbol, &o.left, &o.right]), &w.0)?;
    Ok(())
}

/// Forgets a taught operator.
pub fn delete_operator(txn: &mut dyn KvTxn, symbol: &str, left: &str, right: &str) -> Result<()> {
    txn.delete(&names_key(OPERATORS, &[symbol, left, right]))?;
    Ok(())
}

/// Where a taught cast applies on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastContext {
    /// Only written: `CAST(x AS t)`, `x::t`.
    Explicit,
    /// Also when a value is stored in a column (`INSERT`, `UPDATE`).
    Assignment,
    /// Also wherever the types meet.
    Implicit,
}

impl CastContext {
    /// Its SQL keyword.
    pub fn sql(self) -> &'static str {
        match self {
            CastContext::Explicit => "explicit",
            CastContext::Assignment => "assignment",
            CastContext::Implicit => "implicit",
        }
    }
}

/// A cast taught: `ALTER TYPE t ADD CAST TO target AS function`.
#[derive(Clone, Debug, PartialEq)]
pub struct TaughtCast {
    /// The source type's name.
    pub from: String,
    /// The target type's name.
    pub to: String,
    /// The function converting.
    pub function: String,
    /// Where it applies on its own.
    pub context: CastContext,
}

fn decode_cast(from: String, to: String, v: &[u8]) -> Result<TaughtCast> {
    let mut r = Reader::new(v);
    let function = r.str()?;
    let context = match r.u8()? {
        0 => CastContext::Explicit,
        1 => CastContext::Assignment,
        _ => CastContext::Implicit,
    };
    Ok(TaughtCast {
        from,
        to,
        function,
        context,
    })
}

/// The cast taught from `from` to `to` (type names).
pub fn get_cast(txn: &dyn KvTxn, from: &str, to: &str) -> Result<Option<TaughtCast>> {
    txn.get(&names_key(CASTS, &[from, to]))?
        .map(|v| decode_cast(from.into(), to.into(), &v))
        .transpose()
}

/// Every taught cast.
pub fn casts(txn: &dyn KvTxn) -> Result<Vec<TaughtCast>> {
    scan_prefix(txn, CASTS)?
        .into_iter()
        .map(|(k, v)| {
            let p = key_parts(&k, CASTS)?;
            let [from, to] =
                <[String; 2]>::try_from(p).map_err(|_| Error::internal("malformed cast key"))?;
            decode_cast(from, to, &v)
        })
        .collect()
}

/// Stores a taught cast.
pub fn put_cast(txn: &mut dyn KvTxn, c: &TaughtCast) -> Result<()> {
    let mut w = Writer::default();
    w.str(&c.function).u8(match c.context {
        CastContext::Explicit => 0,
        CastContext::Assignment => 1,
        CastContext::Implicit => 2,
    });
    txn.put(&names_key(CASTS, &[&c.from, &c.to]), &w.0)?;
    Ok(())
}

/// Forgets a taught cast.
pub fn delete_cast(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    txn.delete(&names_key(CASTS, &[from, to]))?;
    Ok(())
}

/// A row's primary index key.
pub fn row_key(table: &Table, pk: &[Value]) -> Vec<u8> {
    let mut k = table.index_prefix(0);
    k.extend_from_slice(&crate::encoding::encode_key(pk));
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_roundtrip() {
        let t = Table {
            id: 7,
            name: "people".into(),
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: DataType::BigInt,
                    nullable: false,
                    default: None,
                    max_len: None,
                    numeric: None,
                    comment: None,
                },
                Column {
                    name: "name".into(),
                    ty: DataType::Text,
                    nullable: true,
                    default: Some("'anon'".into()),
                    max_len: Some(20),
                    numeric: None,
                    comment: Some("who".into()),
                },
                Column {
                    name: "price".into(),
                    ty: DataType::Numeric,
                    nullable: true,
                    default: None,
                    max_len: None,
                    numeric: Some((10, 2)),
                    comment: None,
                },
            ],
            primary_key: vec![0],
            indexes: vec![Index {
                id: 1,
                name: "people_name".into(),
                columns: vec![1],
                unique: true,
                orderings: vec![Some("ci".into())],
            }],
            checks: vec![("id_positive".into(), "id > 0".into())],
            next_index_id: 2,
            foreign_keys: vec![ForeignKey {
                name: "people_boss_fkey".into(),
                columns: vec![0],
                table: "bosses".into(),
                ref_columns: vec![0],
                on_delete: FkAction::Cascade,
                on_update: FkAction::SetNull,
            }],
            referenced_by: vec![("pets".into(), "pets_owner_fkey".into())],
            owner: Some("alice".into()),
            row_security: true,
            force_row_security: false,
            comment: Some("everyone we know".into()),
            storage: "row,column".into(),
            columnar: vec![0, 2],
            copy: crate::column_store::CopyState::BuildingColumns,
            partition_by: Some(crate::partition::Spec {
                kind: crate::partition::Kind::Range,
                columns: vec![1],
            }),
            partition_of: Some((
                "all".into(),
                crate::partition::PartBound::Range {
                    from: vec![Some(Value::Int(1))],
                    to: vec![None],
                },
            )),
            storage_policy: Some((
                "column".into(),
                crate::datetime::Interval {
                    months: 0,
                    days: 30,
                    micros: 0,
                },
            )),
        };
        assert_eq!(Table::decode(&t.encode()).unwrap(), t);
        assert!(t.index_prefix(0) < t.index_prefix(1));
        assert!(t.data_prefix().len() == 5 && t.index_prefix(1).starts_with(&t.data_prefix()));
    }
}
