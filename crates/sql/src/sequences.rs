//! Sequences (issue #23): `CREATE SEQUENCE`, `ALTER SEQUENCE`, `DROP
//! SEQUENCE`, and `nextval`, `currval`, `setval`, `lastval`, as
//! PostgreSQL's.
//!
//! A sequence's **definition** is in the catalog (`00 1E <name>`), as a
//! table's is: made, changed and dropped by transactions.  Its **value**
//! is not transactional — as PostgreSQL's, a value handed out is never
//! handed out again, whatever becomes of the transaction that took it, and
//! two transactions taking values never wait on each other nor conflict.
//!
//! The value lives in memory, one state per sequence of a database (and
//! branch), shared by its sessions: the last value handed out, exact.
//! What the store holds (`00 1F <id>`, outside what the catalog cache
//! watches) is a **reservation**: a value no smaller (in the sequence's
//! direction) than any handed out.  `nextval` takes values from the
//! reservation; when it runs out, a transaction of its own reserves the
//! next run and commits before a value of it is handed out — so after a
//! crash or a restart the sequence goes on past the reservation: values
//! may be skipped, never repeated (PostgreSQL logs 32 values ahead,
//! `SEQ_LOG_VALS`, and skips them after a crash so).  The run starts at
//! 32 values and doubles while runs go fast, up to [`MOST_RESERVED`].
//!
//! A sequence's id comes from a counter of its own, advanced by a
//! transaction of its own too, so that two sequences made at once never
//! share one (and a sequence made in a transaction rolled back leaves its
//! id unused).
//!
//! `currval` and `lastval` are the session's ([`SessionValues`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use nucleus_kv::{Kv, KvTxn, TxnOptions};

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::ext::{
    Arg, ArgInfo, CallContext, Registry, Returns, ScalarFunction, Signature, Volatility,
};
use crate::privileges::{Checker, Kind, SELECT, UPDATE, USAGE};
use crate::types::{DataType, Value};

/// Definitions: `00 1E <name>`.
const DEFS: &[u8] = &[0x00, 0x1E];
/// Reservations: `00 1F <id u64 BE>` → last (i64 BE), called (u8); the
/// next id: `00 1F` alone.
const VALUES: &[u8] = &[0x00, 0x1F];

const VERSION: u8 = 1;

/// The first run of values a sequence reserves, and the most.
const FIRST_RESERVED: u64 = 32;
/// See [`FIRST_RESERVED`].
pub const MOST_RESERVED: u64 = 4096;

/// A sequence's definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    /// Its id (its value's key).
    pub id: u64,
    /// Name.
    pub name: String,
    /// Its maker.
    pub owner: String,
    /// `smallint`, `integer` or `bigint`.
    pub ty: DataType,
    /// `INCREMENT BY`, not zero.
    pub increment: i64,
    /// `MINVALUE`.
    pub min: i64,
    /// `MAXVALUE`.
    pub max: i64,
    /// `START WITH`: what `RESTART` goes back to.
    pub start: i64,
    /// `CACHE` (kept and shown; the reservation is the engine's own).
    pub cache: i64,
    /// `CYCLE`: past a bound, the other one.
    pub cycle: bool,
    /// `OWNED BY table.column`: dropped with that column.
    pub owned_by: Option<(String, String)>,
    /// Changed at each `ALTER SEQUENCE` of its options.
    pub version: u64,
}

impl Sequence {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .u64(self.id)
            .str(&self.name)
            .str(&self.owner)
            .u8(match self.ty {
                DataType::SmallInt => 2,
                DataType::Integer => 4,
                _ => 8,
            })
            .u64(self.increment as u64)
            .u64(self.min as u64)
            .u64(self.max as u64)
            .u64(self.start as u64)
            .u64(self.cache as u64)
            .u8(u8::from(self.cycle))
            .opt_str(self.owned_by.as_ref().map(|(t, _)| t.as_str()))
            .opt_str(self.owned_by.as_ref().map(|(_, c)| c.as_str()))
            .u64(self.version);
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown sequence record version"));
        }
        let id = r.u64()?;
        let name = r.str()?;
        let owner = r.str()?;
        let ty = match r.u8()? {
            2 => DataType::SmallInt,
            4 => DataType::Integer,
            _ => DataType::BigInt,
        };
        let increment = r.u64()? as i64;
        let min = r.u64()? as i64;
        let max = r.u64()? as i64;
        let start = r.u64()? as i64;
        let cache = r.u64()? as i64;
        let cycle = r.u8()? != 0;
        let table = r.opt_str()?;
        let column = r.opt_str()?;
        let version = r.u64()?;
        Ok(Sequence {
            id,
            name,
            owner,
            ty,
            increment,
            min,
            max,
            start,
            cache,
            cycle,
            owned_by: table.zip(column),
            version,
        })
    }
}

fn def_key(name: &str) -> Vec<u8> {
    [DEFS, name.as_bytes()].concat()
}

fn value_key(id: u64) -> Vec<u8> {
    [VALUES, &id.to_be_bytes()[..]].concat()
}

/// Sequence `name`, if there is one.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<Sequence>> {
    txn.get(&def_key(name))?
        .map(|b| Sequence::decode(&b))
        .transpose()
}

/// Sequence `name`; `42P01` if there is none.
pub fn existing(txn: &dyn KvTxn, name: &str) -> Result<Sequence> {
    get(txn, name)?.ok_or_else(|| missing(name))
}

/// Sequence `name` through the session's catalog cache (a call of
/// `nextval` a row).
fn cached(txn: &dyn KvTxn, name: &str) -> Result<Arc<Sequence>> {
    crate::catalog_cache::memo(txn, crate::catalog_cache::Part::Sequence, name, || {
        Ok(get(txn, name)?.map(Arc::new))
    })?
    .ok_or_else(|| missing(name))
}

fn missing(name: &str) -> Error {
    Error::undefined_table(format!("relation \"{name}\" does not exist"))
}

/// Every sequence, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Sequence>> {
    let end = nucleus_kv::prefix_end(DEFS).expect("sequence prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(DEFS),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Sequence::decode(&e?.1)?);
    }
    Ok(out)
}

fn put(txn: &mut dyn KvTxn, s: &Sequence) -> Result<()> {
    txn.put(&def_key(&s.name), &s.encode())?;
    Ok(())
}

// ------------------------------------------------------------- options

/// An option of `CREATE` or `ALTER SEQUENCE`.
#[derive(Clone, Debug, PartialEq)]
pub enum SeqOption {
    /// `AS type`.
    As(String),
    /// `INCREMENT [BY] n`.
    Increment(i64),
    /// `MINVALUE n` / `NO MINVALUE` (`None`).
    MinValue(Option<i64>),
    /// `MAXVALUE n` / `NO MAXVALUE`.
    MaxValue(Option<i64>),
    /// `START [WITH] n`.
    Start(i64),
    /// `RESTART [[WITH] n]` (`ALTER` only).
    Restart(Option<i64>),
    /// `CACHE n`.
    Cache(i64),
    /// `[NO] CYCLE`.
    Cycle(bool),
    /// `OWNED BY table.column` / `OWNED BY NONE`.
    OwnedBy(Option<(String, String)>),
}

fn bounds_of(ty: DataType) -> RangeInclusive<i64> {
    match ty {
        DataType::SmallInt => i64::from(i16::MIN)..=i64::from(i16::MAX),
        DataType::Integer => i64::from(i32::MIN)..=i64::from(i32::MAX),
        _ => i64::MIN..=i64::MAX,
    }
}

/// The SQL name of a sequence's type.
pub fn type_word(ty: DataType) -> &'static str {
    match ty {
        DataType::SmallInt => "smallint",
        DataType::Integer => "integer",
        _ => "bigint",
    }
}

fn bad(m: String) -> Error {
    Error::invalid_parameter(m)
}

/// The definition `options` make of `base` (a new sequence's defaults, or
/// the one altered), checked as PostgreSQL checks it; and a `RESTART`'s
/// value.
fn apply(base: &Sequence, options: &[SeqOption], new: bool) -> Result<(Sequence, Option<i64>)> {
    let mut s = base.clone();
    let mut min = None;
    let mut max = None;
    let mut start = None;
    let mut restart = None;
    for o in options {
        match o {
            SeqOption::As(t) => {
                s.ty = match t.as_str() {
                    "smallint" | "int2" => DataType::SmallInt,
                    "integer" | "int" | "int4" => DataType::Integer,
                    "bigint" | "int8" => DataType::BigInt,
                    _ => {
                        return Err(bad(
                            "sequence type must be smallint, integer, or bigint".into()
                        ));
                    }
                }
            }
            SeqOption::Increment(n) => s.increment = *n,
            SeqOption::MinValue(n) => min = Some(*n),
            SeqOption::MaxValue(n) => max = Some(*n),
            SeqOption::Start(n) => start = Some(*n),
            SeqOption::Restart(n) => restart = Some(*n),
            SeqOption::Cache(n) => s.cache = *n,
            SeqOption::Cycle(c) => s.cycle = *c,
            SeqOption::OwnedBy(o) => s.owned_by = o.clone(),
        }
    }
    if s.increment == 0 {
        return Err(bad("INCREMENT must not be zero".into()));
    }
    let range = bounds_of(s.ty);
    let old = bounds_of(base.ty);
    // A bound not given: the type's (for its direction) when new, given
    // `NO …`, or the old one was the old type's.
    let up = s.increment > 0;
    s.max = match max {
        Some(Some(n)) => n,
        Some(None) => {
            if up {
                *range.end()
            } else {
                -1
            }
        }
        None if new => {
            if up {
                *range.end()
            } else {
                -1
            }
        }
        None if base.max == *old.end() => *range.end(),
        None => base.max,
    };
    s.min = match min {
        Some(Some(n)) => n,
        Some(None) => {
            if up {
                1
            } else {
                *range.start()
            }
        }
        None if new => {
            if up {
                1
            } else {
                *range.start()
            }
        }
        None if base.min == *old.start() => *range.start(),
        None => base.min,
    };
    let word = type_word(s.ty);
    if !range.contains(&s.max) {
        return Err(bad(format!(
            "MAXVALUE ({}) is out of range for sequence data type {word}",
            s.max
        )));
    }
    if !range.contains(&s.min) {
        return Err(bad(format!(
            "MINVALUE ({}) is out of range for sequence data type {word}",
            s.min
        )));
    }
    if s.min >= s.max {
        return Err(bad(format!(
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            s.min, s.max
        )));
    }
    s.start = match start {
        Some(n) => n,
        None if new => {
            if up {
                s.min
            } else {
                s.max
            }
        }
        None => base.start,
    };
    if s.start < s.min {
        return Err(bad(format!(
            "START value ({}) cannot be less than MINVALUE ({})",
            s.start, s.min
        )));
    }
    if s.start > s.max {
        return Err(bad(format!(
            "START value ({}) cannot be greater than MAXVALUE ({})",
            s.start, s.max
        )));
    }
    let restart = restart.map(|r| r.unwrap_or(s.start));
    if let Some(r) = restart {
        if r < s.min {
            return Err(bad(format!(
                "RESTART value ({r}) cannot be less than MINVALUE ({})",
                s.min
            )));
        }
        if r > s.max {
            return Err(bad(format!(
                "RESTART value ({r}) cannot be greater than MAXVALUE ({})",
                s.max
            )));
        }
    }
    if s.cache <= 0 {
        return Err(bad(format!(
            "CACHE ({}) must be greater than zero",
            s.cache
        )));
    }
    Ok((s, restart))
}

/// The `CREATE SEQUENCE` that makes `s` (its options not the defaults;
/// not `OWNED BY`, which needs its table).
pub fn definition(s: &Sequence) -> String {
    let mut sql = format!("CREATE SEQUENCE {}", crate::definitions::ident(&s.name));
    if s.ty != DataType::BigInt {
        sql += &format!(" AS {}", type_word(s.ty));
    }
    // The bounds a new one of its type and direction has.
    let range = bounds_of(s.ty);
    let up = s.increment > 0;
    let (min, max) = if up {
        (1, *range.end())
    } else {
        (*range.start(), -1)
    };
    if s.increment != 1 {
        sql += &format!(" INCREMENT BY {}", s.increment);
    }
    if s.min != min {
        sql += &format!(" MINVALUE {}", s.min);
    }
    if s.max != max {
        sql += &format!(" MAXVALUE {}", s.max);
    }
    let start = if s.increment > 0 { s.min } else { s.max };
    if s.start != start {
        sql += &format!(" START WITH {}", s.start);
    }
    if s.cache != 1 {
        sql += &format!(" CACHE {}", s.cache);
    }
    if s.cycle {
        sql += " CYCLE";
    }
    sql
}

fn check_owned_by(txn: &dyn KvTxn, s: &Sequence) -> Result<()> {
    if let Some((t, c)) = &s.owned_by {
        let table = crate::catalog::table(txn, t)?;
        if table.column(c).is_none() {
            return Err(Error::undefined_column(format!(
                "column \"{c}\" of relation \"{t}\" does not exist"
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ DDL

/// `CREATE SEQUENCE [IF NOT EXISTS] name options`.
pub fn create(
    txn: &mut dyn KvTxn,
    name: &str,
    options: &[SeqOption],
    if_not_exists: bool,
) -> Result<bool> {
    if get(&*txn, name)?.is_some() || crate::catalog::get_table(&*txn, name)?.is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(Error::duplicate_table(format!(
            "relation \"{name}\" already exists"
        )));
    }
    let base = Sequence {
        id: 0,
        name: name.to_string(),
        owner: crate::access::current().user.clone(),
        ty: DataType::BigInt,
        increment: 1,
        min: 1,
        max: i64::MAX,
        start: 1,
        cache: 1,
        cycle: false,
        owned_by: None,
        version: 0,
    };
    if options.iter().any(|o| matches!(o, SeqOption::Restart(_))) {
        return Err(Error::syntax("RESTART is ALTER SEQUENCE's"));
    }
    let (mut s, _) = apply(&base, options, true)?;
    check_owned_by(&*txn, &s)?;
    let kv = statement_kv()?;
    s.id = next_id(&kv)?;
    // Its value first, in a transaction of its own: there before anyone
    // can take one.
    store(&kv, s.id, s.start, false)?;
    put(txn, &s)?;
    Ok(true)
}

/// What `ALTER SEQUENCE` does.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterSequence {
    /// Options (and `RESTART`).
    Options(Vec<SeqOption>),
    /// `RENAME TO name`.
    Rename(String),
}

/// `ALTER SEQUENCE [IF EXISTS] name …`.  Its options are the
/// transaction's; a `RESTART` happens at once, as a `setval` does.
pub fn alter(
    txn: &mut dyn KvTxn,
    name: &str,
    action: &AlterSequence,
    if_exists: bool,
) -> Result<bool> {
    let Some(old) = get(&*txn, name)? else {
        if if_exists {
            return Ok(false);
        }
        return Err(missing(name));
    };
    Checker::new().require_owner(&*txn, Kind::Sequence, name)?;
    match action {
        AlterSequence::Options(options) => {
            let (mut s, restart) = apply(&old, options, false)?;
            check_owned_by(&*txn, &s)?;
            let changed = Sequence {
                version: old.version,
                ..s.clone()
            } != old;
            if changed {
                s.version = old.version + 1;
                put(txn, &s)?;
            }
            if let Some(r) = restart {
                let kv = statement_kv()?;
                set(&kv, &s, r, false)?;
            }
        }
        AlterSequence::Rename(to) => {
            if get(&*txn, to)?.is_some() || crate::catalog::get_table(&*txn, to)?.is_some() {
                return Err(Error::duplicate_table(format!(
                    "relation \"{to}\" already exists"
                )));
            }
            txn.delete(&def_key(name))?;
            let s = Sequence {
                name: to.clone(),
                ..old
            };
            put(txn, &s)?;
            crate::privileges::rename(txn, Kind::Sequence, name, to)?;
            rewrite_defaults(txn, name, Some(to))?;
        }
    }
    Ok(true)
}

/// `DROP SEQUENCE [IF EXISTS] name, … [CASCADE]`: a default calling one
/// is `2BP01` — dropped too with `CASCADE`.
pub fn drop(txn: &mut dyn KvTxn, names: &[String], if_exists: bool, cascade: bool) -> Result<()> {
    for name in names {
        if get(&*txn, name)?.is_none() {
            if if_exists {
                continue;
            }
            return Err(missing(name));
        }
        Checker::new().require_owner(&*txn, Kind::Sequence, name)?;
        let users = users(&*txn, name)?;
        if !users.is_empty() && !cascade {
            return Err(Error::dependent_objects(format!(
                "cannot drop sequence {name} because other objects depend on it: the default of {}",
                users
                    .iter()
                    .map(|(t, c)| format!("{t}.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        rewrite_defaults(txn, name, None)?;
        txn.delete(&def_key(name))?;
        // (Its reservation stays, unread: a dropped sequence's value is
        // not taken back by a rollback of the drop otherwise.)
        crate::privileges::forget(txn, Kind::Sequence, name)?;
    }
    Ok(())
}

/// The sequences a default calls by name (`nextval('s')`, as written).
pub fn called_in(default: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = default.to_ascii_lowercase();
    let mut at = 0;
    while let Some(i) = lower[at..].find("nextval(") {
        let from = at + i + "nextval(".len();
        at = from;
        let rest = default[from..].trim_start();
        let Some(body) = rest.strip_prefix('\'') else {
            continue;
        };
        let mut lit = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    lit.push('\'');
                    continue;
                }
                break;
            }
            lit.push(c);
        }
        if let Ok(name) = name_of(&lit) {
            out.push(name);
        }
    }
    out
}

/// The (table, column) defaults calling sequence `name`.
pub fn users(txn: &dyn KvTxn, name: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for t in crate::catalog::tables(txn)? {
        for c in &t.columns {
            if c.default
                .as_deref()
                .is_some_and(|d| called_in(d).iter().any(|n| n == name))
            {
                out.push((t.name.clone(), c.name.clone()));
            }
        }
    }
    Ok(out)
}

/// The defaults calling sequence `from`: calling `to` instead, or
/// dropped (`None`).
fn rewrite_defaults(txn: &mut dyn KvTxn, from: &str, to: Option<&str>) -> Result<()> {
    for mut t in crate::catalog::tables(&*txn)? {
        let mut changed = false;
        for c in &mut t.columns {
            let Some(d) = &c.default else { continue };
            if !called_in(d).iter().any(|n| n == from) {
                continue;
            }
            c.default = to.map(|to| {
                d.replace(
                    &format!("'{}'", from.replace('\'', "''")),
                    &literal_name(to),
                )
                .replace(&literal_name(from), &literal_name(to))
            });
            changed = true;
        }
        if changed {
            crate::catalog::update_table(txn, &t)?;
        }
    }
    Ok(())
}

/// `name` as `nextval`'s argument: `'name'`, quoted inside when it must be.
pub fn literal_name(name: &str) -> String {
    format!("'{}'", crate::definitions::ident(name).replace('\'', "''"))
}

/// The sequence a `regclass`-like text names: `s`, `"S"`, `public.s`.
pub fn name_of(text: &str) -> Result<String> {
    let t = text.trim();
    let mut parts = Vec::new();
    let mut chars = t.chars().peekable();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '.' if !quoted => {
                parts.push((std::mem::take(&mut cur), was_quoted));
                was_quoted = false;
            }
            c if quoted => cur.push(c),
            c => cur.push(c.to_ascii_lowercase()),
        }
    }
    parts.push((cur, was_quoted));
    let (name, _) = match parts.len() {
        1 => parts.pop().expect("one"),
        2 if parts[0].0 == "public" => parts.pop().expect("two"),
        2 => {
            return Err(Error::invalid_schema(format!(
                "schema \"{}\" does not exist",
                parts[0].0
            )));
        }
        _ => {
            return Err(Error::invalid_name(format!(
                "improper relation name (too many dotted names): {t}"
            )));
        }
    };
    if name.is_empty() {
        return Err(Error::invalid_name(format!("invalid name syntax: \"{t}\"")));
    }
    Ok(name)
}

// ------------------------------------------------------------ values

/// A sequence's value in memory.
#[derive(Debug)]
struct Live {
    /// The definition's version it reserved under.
    version: u64,
    /// The last value handed out (or, not `called`, the next to be).
    last: i64,
    called: bool,
    /// Values reserved past `last`, to hand out without a write.
    left: u64,
    /// The next reservation's length.
    run: u64,
    /// When the last reservation was made.
    reserved_at: Option<Instant>,
}

/// A database's (and branch's: [`Kv::instance`]) sequence, by id.
type LiveKey = (u64, u64);

fn lives() -> &'static Mutex<HashMap<LiveKey, Arc<Mutex<Live>>>> {
    static L: OnceLock<Mutex<HashMap<LiveKey, Arc<Mutex<Live>>>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The statement's database ([`crate::datetime::current_kv`]).
fn statement_kv() -> Result<Arc<dyn Kv>> {
    crate::datetime::current_kv()
        .ok_or_else(|| Error::internal("a sequence used outside a statement"))
}

/// Sequence `id`'s state in memory, read from its reservation the first
/// time — kept by the session too.
fn live(kv: &Arc<dyn Kv>, s: &Sequence) -> Result<Arc<Mutex<Live>>> {
    let key = (kv.instance(), s.id);
    if let Some(l) = session(|x| x.lives.get(&key).cloned()) {
        return Ok(l);
    }
    let l = shared_live(kv, s, key)?;
    session(|x| x.lives.insert(key, l.clone()));
    Ok(l)
}

fn shared_live(kv: &Arc<dyn Kv>, s: &Sequence, key: LiveKey) -> Result<Arc<Mutex<Live>>> {
    if let Some(l) = lock(lives()).get(&key) {
        return Ok(l.clone());
    }
    let (last, called) = load(kv, s.id)?.unwrap_or((s.start, false));
    let l = Arc::new(Mutex::new(Live {
        version: s.version,
        last,
        called,
        left: 0,
        run: FIRST_RESERVED,
        reserved_at: None,
    }));
    Ok(lock(lives()).entry(key).or_insert(l).clone())
}

fn load(kv: &Arc<dyn Kv>, id: u64) -> Result<Option<(i64, bool)>> {
    let txn = kv.begin(TxnOptions::read_only())?;
    Ok(txn.get(&value_key(id))?.and_then(|b| {
        (b.len() == 9).then(|| {
            (
                i64::from_be_bytes(b[..8].try_into().expect("8 bytes")),
                b[8] != 0,
            )
        })
    }))
}

/// Writes `id`'s reservation in a transaction of its own (retried on a
/// conflict).
fn store(kv: &Arc<dyn Kv>, id: u64, last: i64, called: bool) -> Result<()> {
    let mut v = last.to_be_bytes().to_vec();
    v.push(u8::from(called));
    write_alone(kv, |t| {
        t.put(&value_key(id), &v)?;
        Ok(())
    })
}

fn write_alone(kv: &Arc<dyn Kv>, mut f: impl FnMut(&mut dyn KvTxn) -> Result<()>) -> Result<()> {
    let mut attempt = 0;
    loop {
        let mut t = kv.begin(TxnOptions::default())?;
        f(t.as_mut())?;
        match t.commit() {
            Ok(_) => return Ok(()),
            Err(e) if e.is_retryable() && attempt < 20 => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    }
}

/// A new sequence's id.
fn next_id(kv: &Arc<dyn Kv>) -> Result<u64> {
    let mut id = 0;
    write_alone(kv, |t| {
        id = t
            .get(VALUES)?
            .map_or(1, |b| u64::from_be_bytes(b.try_into().unwrap_or([0; 8])));
        t.put(VALUES, &(id + 1).to_be_bytes())?;
        Ok(())
    })?;
    Ok(id)
}

/// The value after `v` (`None`: past a bound, no cycle).
fn after(s: &Sequence, v: i64) -> std::result::Result<i64, i64> {
    match v.checked_add(s.increment) {
        Some(n) if (s.min..=s.max).contains(&n) => Ok(n),
        _ if s.cycle => Ok(if s.increment > 0 { s.min } else { s.max }),
        _ => Err(if s.increment > 0 { s.max } else { s.min }),
    }
}

/// `nextval`: the sequence's next value.
fn next(kv: &Arc<dyn Kv>, s: &Sequence) -> Result<i64> {
    let l = live(kv, s)?;
    let mut l = lock(&l);
    if l.version != s.version {
        // Altered: what was reserved under the old options goes unused
        // (the last value stays: the next follows it by the new ones).
        l.version = s.version;
        l.left = 0;
    }
    let v = if l.called {
        after(s, l.last).map_err(|bound| {
            Error::sequence_limit(format!(
                "nextval: reached {} value of sequence \"{}\" ({bound})",
                if s.increment > 0 {
                    "maximum"
                } else {
                    "minimum"
                },
                s.name
            ))
        })?
    } else {
        l.last
    };
    if l.left == 0 {
        // Reserve a run: up to `run` values from `v` on, short of a bound
        // (a cycle's values after it are below the reservation).
        if l.reserved_at
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1))
        {
            l.run = (l.run * 2).min(MOST_RESERVED);
        } else {
            l.run = FIRST_RESERVED;
        }
        let mut end = v;
        let mut n = 1;
        while n < l.run {
            match end.checked_add(s.increment) {
                Some(e) if (s.min..=s.max).contains(&e) => {
                    end = e;
                    n += 1;
                }
                _ => break,
            }
        }
        store(kv, s.id, end, true)?;
        l.left = n;
        l.reserved_at = Some(Instant::now());
    }
    l.last = v;
    l.called = true;
    l.left -= 1;
    Ok(v)
}

/// `setval` (and `ALTER SEQUENCE … RESTART`): at once, in a transaction
/// of its own.
fn set(kv: &Arc<dyn Kv>, s: &Sequence, v: i64, called: bool) -> Result<()> {
    let l = live(kv, s)?;
    let mut l = lock(&l);
    store(kv, s.id, v, called)?;
    l.version = s.version;
    l.last = v;
    l.called = called;
    l.left = 0;
    Ok(())
}

/// The value `nc_catalog.sequences` shows: the last handed out, if any.
pub fn last_value(txn: &dyn KvTxn, s: &Sequence) -> Result<Option<i64>> {
    if let Some(kv) = crate::datetime::current_kv()
        && let Some(l) = lock(lives()).get(&(kv.instance(), s.id))
    {
        let l = lock(l);
        return Ok(l.called.then_some(l.last));
    }
    Ok(txn.get(&value_key(s.id))?.and_then(|b| {
        (b.len() == 9 && b[8] != 0).then(|| i64::from_be_bytes(b[..8].try_into().expect("8 bytes")))
    }))
}

// ------------------------------------------------------------ session

/// A session's `currval`s and `lastval`, and whether its transaction is
/// read-only as declared (`BEGIN READ ONLY`, a read-only token, a past
/// snapshot): there `nextval` and `setval` are `25006`.
#[derive(Debug, Default)]
pub struct SessionValues {
    values: HashMap<u64, i64>,
    /// The sequences' states it used.
    lives: HashMap<LiveKey, Arc<Mutex<Live>>>,
    last: Option<i64>,
    /// See the type.
    pub read_only: bool,
}

thread_local! {
    static SESSION: RefCell<Option<Arc<Mutex<SessionValues>>>> = const { RefCell::new(None) };
}

/// Runs `f` with `values` as the session's.
pub fn with_session<R>(values: &Arc<Mutex<SessionValues>>, f: impl FnOnce() -> R) -> R {
    let prev = SESSION.with(|s| s.replace(Some(values.clone())));
    let r = f();
    SESSION.with(|s| s.replace(prev));
    r
}

fn session<R>(f: impl FnOnce(&mut SessionValues) -> R) -> R {
    let s = SESSION.with(|s| s.borrow().clone());
    match s {
        Some(s) => f(&mut lock(&s)),
        None => f(&mut SessionValues::default()),
    }
}

// ------------------------------------------------------------ functions

#[derive(Clone, Copy, Debug, PartialEq)]
enum F {
    Nextval,
    Currval,
    Setval,
    Lastval,
}

struct SeqFn(F, &'static str);

/// Registers `nextval`, `currval`, `setval`, `lastval`.
pub fn register(r: &mut Registry) -> Result<()> {
    for (f, name) in [
        (F::Nextval, "nextval"),
        (F::Currval, "currval"),
        (F::Setval, "setval"),
        (F::Lastval, "lastval"),
    ] {
        r.add_function(&[name], Arc::new(SeqFn(f, name)))?;
    }
    Ok(())
}

impl ScalarFunction for SeqFn {
    fn name(&self) -> &str {
        self.1
    }

    fn arity(&self) -> RangeInclusive<usize> {
        match self.0 {
            F::Nextval | F::Currval => 1..=1,
            F::Setval => 2..=3,
            F::Lastval => 0..=0,
        }
    }

    fn resolve(&self, args: &[ArgInfo]) -> Result<Signature> {
        let mut want = vec![Arg::Untyped(DataType::Text)];
        match self.0 {
            F::Lastval => want.clear(),
            F::Setval => {
                want.push(Arg::Untyped(DataType::BigInt));
                if args.len() == 3 {
                    want.push(Arg::Untyped(DataType::Bool));
                }
            }
            _ => {}
        }
        Ok(Signature::new(want, Returns::Type(Some(DataType::BigInt))))
    }

    fn volatility(&self) -> Volatility {
        Volatility::Volatile
    }

    fn call(&self, cx: &CallContext<'_>, args: &[Value]) -> Result<Value> {
        if self.0 == F::Lastval {
            return session(|s| s.last)
                .map(Value::Int)
                .ok_or_else(|| Error::prerequisite("lastval is not yet defined in this session"));
        }
        let Value::Text(t) = &args[0] else {
            return Ok(Value::Null);
        };
        let txn = cx.txn();
        let name = name_of(t)?;
        let s = cached(txn, &name)?;
        let need = match self.0 {
            F::Nextval => USAGE | UPDATE,
            F::Currval => SELECT | USAGE,
            _ => UPDATE,
        };
        // Decided once while the catalog and the identity hold.
        // (By the function: each asks its own privileges.)
        let mut key = String::with_capacity(name.len() + 1);
        key.push(match self.0 {
            F::Nextval => 'n',
            F::Currval => 'c',
            _ => 's',
        });
        key.push_str(&name);
        let may =
            crate::catalog_cache::memo(txn, crate::catalog_cache::Part::SequenceUse, &key, || {
                Checker::new().has(txn, Kind::Sequence, &name, need, None)
            })?;
        if !may {
            return Err(crate::privileges::denied(Kind::Sequence, &name, need, None));
        }
        match self.0 {
            F::Nextval => {
                read_write("nextval()")?;
                let v = next(&statement_kv()?, &s)?;
                session(|x| {
                    x.values.insert(s.id, v);
                    x.last = Some(v);
                });
                Ok(Value::Int(v))
            }
            F::Currval => session(|x| x.values.get(&s.id).copied())
                .map(Value::Int)
                .ok_or_else(|| {
                    Error::prerequisite(format!(
                        "currval of sequence \"{name}\" is not yet defined in this session"
                    ))
                }),
            _ => {
                let Value::Int(v) = args[1] else {
                    return Ok(Value::Null);
                };
                let called = match args.get(2) {
                    None => true,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Ok(Value::Null),
                };
                read_write("setval()")?;
                if !(s.min..=s.max).contains(&v) {
                    return Err(Error::out_of_range(format!(
                        "setval: value {v} is out of bounds for sequence \"{name}\" ({}..{})",
                        s.min, s.max
                    )));
                }
                set(&statement_kv()?, &s, v, called)?;
                if called {
                    session(|x| {
                        x.values.insert(s.id, v);
                    });
                }
                Ok(Value::Int(v))
            }
        }
    }
}

fn read_write(what: &str) -> Result<()> {
    if session(|s| s.read_only) {
        return Err(Error::read_only(format!(
            "cannot execute {what} in a read-only transaction"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_as_regclass_reads_them() {
        assert_eq!(name_of("s").unwrap(), "s");
        assert_eq!(name_of("S").unwrap(), "s");
        assert_eq!(name_of("\"S\"").unwrap(), "S");
        assert_eq!(name_of("public.s").unwrap(), "s");
        assert_eq!(name_of("\"a\"\"b\"").unwrap(), "a\"b");
        assert_eq!(name_of("other.s").unwrap_err().code, "3F000");
        assert_eq!(
            called_in("nextval('t_id_seq'::regclass) + nextval('\"X\"')"),
            ["t_id_seq", "X"]
        );
        assert_eq!(literal_name("X"), "'\"X\"'");
    }
}
