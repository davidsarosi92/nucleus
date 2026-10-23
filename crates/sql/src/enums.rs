//! Enums (PLAN.md §M14.26): `CREATE TYPE mood AS ENUM ('sad', 'ok',
//! 'happy')`, ordered by declaration.
//!
//! An enum is the database's, in its catalog (`00 09 <name>`): its labels,
//! each with a **sort key** — a base-256 fraction in `(0, 1)` without
//! trailing zero bytes, so byte order is numeric order.  A value is stored
//! as its sort key alone, and there is always room between two keys: so
//! `ALTER TYPE … ADD VALUE … BEFORE / AFTER` puts a new label anywhere and
//! `RENAME VALUE` changes a label, neither rewriting a row.
//!
//! A value carries its label (`Value::Enum`); reading a row fills it in
//! from the catalog ([`Labels`]), and text becomes an enum where a
//! transaction is at hand ([`from_text`]: binding, casts as they run,
//! assignment to a column).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_kv::KvTxn;

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};
use crate::types::{DataType, Value};

/// An enum, by name — the same name in any database is the same
/// `EnumRef`, its labels that database's.
#[derive(Clone, Copy)]
pub struct EnumRef(&'static str);

impl EnumRef {
    /// The `EnumRef` called `name` (interned).
    pub fn named(name: &str) -> EnumRef {
        static NAMES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
        let mut names = NAMES
            .get_or_init(Default::default)
            .lock()
            .expect("enum names");
        if let Some(n) = names.get(name) {
            return EnumRef(n);
        }
        let n: &'static str = Box::leak(name.to_string().into_boxed_str());
        names.insert(name.to_string(), n);
        EnumRef(n)
    }

    /// Its name.
    pub fn name(self) -> &'static str {
        self.0
    }
}

impl PartialEq for EnumRef {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for EnumRef {}

impl std::hash::Hash for EnumRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for EnumRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A value of an enum: its sort key and label.  A value just read from a
/// row's bytes has neither type nor label until [`Labels::fill`].
#[derive(Clone, Debug)]
pub struct EnumValue {
    /// The enum (`None`: not resolved yet).
    pub ty: Option<EnumRef>,
    /// The sort key.
    pub key: Arc<[u8]>,
    /// The label.
    pub label: Arc<str>,
}

/// An enum's definition.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumDef {
    /// Its name.
    pub name: String,
    /// Its labels with their sort keys, in order.
    pub labels: Vec<(Vec<u8>, String)>,
    /// The role that owns it (`None`: made before roles).
    pub owner: Option<String>,
}

const PREFIX: [u8; 2] = [0x00, 0x09];
/// Version 2 added the owner; version 1 still reads.
const VERSION: u8 = 2;

fn key_of(name: &str) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

impl EnumDef {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION).str(&self.name).u32(self.labels.len() as u32);
        for (k, l) in &self.labels {
            w.u32(k.len() as u32);
            w.0.extend_from_slice(k);
            w.str(l);
        }
        w.opt_str(self.owner.as_deref());
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=VERSION).contains(&version) {
            return Err(Error::internal("unknown enum definition version"));
        }
        let name = r.str()?;
        let n = r.u32()? as usize;
        let mut labels = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let len = r.u32()? as usize;
            let k = r.take(len)?.to_vec();
            labels.push((k, r.str()?));
        }
        let owner = if version >= 2 { r.opt_str()? } else { None };
        Ok(EnumDef {
            name,
            labels,
            owner,
        })
    }

    /// The value of `label`, or `22P02`.
    pub fn value(&self, label: &str) -> Result<Value> {
        let (k, l) = self
            .labels
            .iter()
            .find(|(_, l)| l == label)
            .ok_or_else(|| {
                Error::invalid_text(format!(
                    "invalid input value for enum {}: \"{label}\"",
                    self.name
                ))
            })?;
        Ok(Value::Enum(EnumValue {
            ty: Some(EnumRef::named(&self.name)),
            key: k.as_slice().into(),
            label: l.as_str().into(),
        }))
    }

    fn label_of(&self, key: &[u8]) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, l)| l.as_str())
    }
}

/// The enum called `name`, if this database has it.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<EnumDef>> {
    txn.get(&key_of(name))?
        .map(|b| EnumDef::decode(&b))
        .transpose()
}

/// Every enum, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<EnumDef>> {
    let end = nucleus_kv::prefix_end(&PREFIX).expect("enum prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&PREFIX[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(EnumDef::decode(&e?.1)?);
    }
    Ok(out)
}

fn put(txn: &mut dyn KvTxn, def: &EnumDef) -> Result<()> {
    txn.put(&key_of(&def.name), &def.encode())?;
    Ok(())
}

/// A sort key strictly between `low` (empty: 0) and `high` (`None`: 1).
pub fn between(low: &[u8], high: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut high = high;
    for i in 0.. {
        let lo = u16::from(low.get(i).copied().unwrap_or(0));
        let hi = high.map_or(256, |h| u16::from(h.get(i).copied().unwrap_or(0)));
        if hi >= lo + 2 {
            out.push(((lo + hi) / 2) as u8);
            return out;
        }
        out.push(lo as u8);
        if hi == lo + 1 {
            // Below `high` from here on, whatever follows.
            high = None;
        }
    }
    unreachable!("a key is found")
}

/// Evenly spaced keys for `n` labels.
fn initial_keys(n: usize) -> Result<Vec<Vec<u8>>> {
    if n >= 65_535 {
        return Err(Error::invalid_parameter(
            "an enum has at most 65 534 labels",
        ));
    }
    Ok((1..=n)
        .map(|i| {
            let k = ((i as u32 * 65_536) / (n as u32 + 1)) as u16;
            let b = k.to_be_bytes();
            if b[1] == 0 { vec![b[0]] } else { b.to_vec() }
        })
        .collect())
}

fn duplicate_label(label: &str) -> Error {
    Error::duplicate_object(format!("enum label \"{label}\" already exists"))
}

/// `CREATE TYPE name AS ENUM (labels)`.
pub fn create(
    txn: &mut dyn KvTxn,
    reg: &crate::ext::Registry,
    name: &str,
    labels: &[String],
) -> Result<()> {
    if reg.type_named(name).is_some() || get(&*txn, name)?.is_some() {
        return Err(Error::duplicate_object(format!(
            "type \"{name}\" already exists"
        )));
    }
    for (i, l) in labels.iter().enumerate() {
        if labels[..i].contains(l) {
            return Err(duplicate_label(l));
        }
        if l.len() > 63 {
            return Err(Error::invalid_parameter(format!(
                "invalid enum label \"{l}\": labels must be 63 bytes or less"
            )));
        }
    }
    let keys = initial_keys(labels.len())?;
    put(
        txn,
        &EnumDef {
            name: name.to_string(),
            labels: keys.into_iter().zip(labels.iter().cloned()).collect(),
            owner: Some(crate::access::current().user.clone()),
        },
    )
}

/// Where `ADD VALUE` puts a label.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// At the end.
    Last,
    /// Before this label.
    Before(String),
    /// After this label.
    After(String),
}

fn def_or_missing(txn: &dyn KvTxn, name: &str) -> Result<EnumDef> {
    get(txn, name)?
        .ok_or_else(|| Error::undefined_object(format!("type \"{name}\" does not exist")))
}

fn position(def: &EnumDef, label: &str) -> Result<usize> {
    def.labels
        .iter()
        .position(|(_, l)| l == label)
        .ok_or_else(|| {
            Error::invalid_parameter(format!("\"{label}\" is not an existing enum label"))
        })
}

/// `ALTER TYPE name ADD VALUE [IF NOT EXISTS] label [BEFORE | AFTER x]`:
/// a key between the neighbours; no row changes.
pub fn add_value(
    txn: &mut dyn KvTxn,
    name: &str,
    label: &str,
    if_not_exists: bool,
    place: &Place,
) -> Result<()> {
    let mut def = def_or_missing(&*txn, name)?;
    if def.labels.iter().any(|(_, l)| l == label) {
        return if if_not_exists {
            Ok(())
        } else {
            Err(duplicate_label(label))
        };
    }
    let at = match place {
        Place::Last => def.labels.len(),
        Place::Before(x) => position(&def, x)?,
        Place::After(x) => position(&def, x)? + 1,
    };
    let low = at
        .checked_sub(1)
        .map_or(&[][..], |i| def.labels[i].0.as_slice());
    let high = def.labels.get(at).map(|(k, _)| k.as_slice());
    let key = between(low, high);
    def.labels.insert(at, (key, label.to_string()));
    put(txn, &def)
}

/// `ALTER TYPE name RENAME VALUE from TO to`: no row changes.
pub fn rename_value(txn: &mut dyn KvTxn, name: &str, from: &str, to: &str) -> Result<()> {
    let mut def = def_or_missing(&*txn, name)?;
    let i = position(&def, from)?;
    if def.labels.iter().any(|(_, l)| l == to) {
        return Err(duplicate_label(to));
    }
    def.labels[i].1 = to.to_string();
    put(txn, &def)
}

/// Enums owned by `from` become `to`'s (a renamed role).
pub fn rename_owner(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for mut e in all(&*txn)? {
        if e.owner.as_deref() == Some(from) {
            e.owner = Some(to.to_string());
            put(txn, &e)?;
        }
    }
    Ok(())
}

/// `DROP TYPE [IF EXISTS] name`: refused while a column has the type.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    if get(&*txn, name)?.is_none() {
        return if if_exists {
            Ok(())
        } else {
            Err(Error::undefined_object(format!(
                "type \"{name}\" does not exist"
            )))
        };
    }
    let e = EnumRef::named(name);
    for t in crate::catalog::tables(&*txn)? {
        if let Some(c) = t.columns.iter().find(|c| c.ty == DataType::Enum(e)) {
            return Err(Error::dependent_objects(format!(
                "cannot drop type {name} because column {}.{} uses it",
                t.name, c.name
            )));
        }
    }
    txn.delete(&key_of(name))?;
    Ok(())
}

/// `v` converted to `ty`, which may be an enum: text becomes its label's
/// value through the catalog; anything else is [`Value::cast`].
pub fn cast(txn: &dyn KvTxn, v: Value, ty: DataType) -> Result<Value> {
    match (v, ty) {
        (Value::Text(s), DataType::Enum(e)) => from_text(txn, e, &s),
        (Value::Char(s), DataType::Enum(e)) => from_text(txn, e, crate::types::unpadded(&s)),
        (v, ty) => v.cast(ty),
    }
}

/// The value of enum `e` labelled `label`.
pub fn from_text(txn: &dyn KvTxn, e: EnumRef, label: &str) -> Result<Value> {
    def_or_missing(txn, e.name())?.value(label)
}

/// The enums a scan has looked up, so each row's labels cost no read.
#[derive(Default)]
pub struct Labels(HashMap<&'static str, Option<EnumDef>>);

impl Labels {
    /// Gives `v`, read from a column of type `ty`, its type and label.
    pub fn fill(&mut self, txn: &dyn KvTxn, ty: DataType, v: &mut Value) -> Result<()> {
        let (DataType::Enum(e), Value::Enum(ev)) = (ty, &mut *v) else {
            return Ok(());
        };
        let def = match self.0.get(e.name()) {
            Some(d) => d,
            None => {
                let d = get(txn, e.name())?;
                self.0.entry(e.name()).or_insert(d)
            }
        };
        let label = def
            .as_ref()
            .and_then(|d| d.label_of(&ev.key))
            .ok_or_else(|| {
                Error::internal(format!(
                    "a {} value whose sort key {:?} has no label",
                    e.name(),
                    ev.key
                ))
            })?;
        ev.ty = Some(e);
        ev.label = label.into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_always_room_between_keys() {
        let mut keys = initial_keys(3).unwrap();
        assert_eq!(keys, [vec![0x40], vec![0x80], vec![0xc0]]);
        // Insert repeatedly at the front, at the back, and between two.
        for _ in 0..200 {
            let first = between(&[], Some(&keys[0]));
            keys.insert(0, first);
            let last = between(keys.last().unwrap(), None);
            keys.push(last);
            let at = keys.len() / 2;
            let mid = between(&keys[at - 1], Some(&keys[at]));
            keys.insert(at, mid);
        }
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "{w:?}");
            assert!(w[1].last() != Some(&0), "{w:?}");
        }
        assert_eq!(between(&[0x80], Some(&[0x81])), [0x80, 0x80]);
        assert_eq!(between(&[0x80, 0xff], Some(&[0x81])), [0x80, 0xff, 0x80]);
    }
}
