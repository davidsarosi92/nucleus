//! Privileges on objects (PLAN.md §M14.25): `GRANT` / `REVOKE` on tables
//! (and their columns), schemas, functions, types and the database, with
//! `WITH GRANT OPTION`, default privileges for tables not yet made, and
//! the checks that refuse what a role lacks (`42501`, naming the privilege
//! and the object).
//!
//! An object's ACL is one catalog record, `00 0C <kind> <name>`: per
//! grantee (a role, or `PUBLIC`) the privileges, those it may grant on,
//! and per column the same.  An object without a record has its kind's
//! default ACL — as PostgreSQL's: the owner has everything; `PUBLIC` may
//! `EXECUTE` functions, use types and schema `public`, and `CONNECT` to
//! the database; nothing else.  A first `GRANT` or `REVOKE` writes the
//! default out, then changes it.  Administrators (and owners) are never
//! refused.
//!
//! Default privileges, `00 0D <role> 00 <kind>`: what objects that role
//! makes start with (`ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT … ON
//! TABLES TO …`).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use nucleus_kv::KvTxn;

use crate::access::Who;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const ACLS: [u8; 2] = [0x00, 0x0C];
const DEFAULTS: [u8; 2] = [0x00, 0x0D];
const VERSION: u8 = 1;

/// The grantee every role belongs to.
pub const PUBLIC: &str = "public";

/// A kind of object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// A table.
    Table,
    /// A schema.
    Schema,
    /// A function (by name).
    Function,
    /// A type (by name).
    Type,
    /// The database.
    Database,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::Table => 1,
            Kind::Schema => 2,
            Kind::Function => 3,
            Kind::Type => 4,
            Kind::Database => 5,
        }
    }

    /// Its SQL word.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::Schema => "schema",
            Kind::Function => "function",
            Kind::Type => "type",
            Kind::Database => "database",
        }
    }

    /// The privileges an object of this kind can have.
    pub fn privileges(self) -> u32 {
        match self {
            Kind::Table => SELECT | INSERT | UPDATE | DELETE | TRUNCATE | REFERENCES,
            Kind::Schema => USAGE | CREATE,
            Kind::Function => EXECUTE,
            Kind::Type => USAGE,
            Kind::Database => CONNECT | CREATE | TEMPORARY,
        }
    }

    /// What `PUBLIC` has by default.
    fn public_default(self, name: &str) -> u32 {
        match self {
            Kind::Function => EXECUTE,
            Kind::Type => USAGE,
            Kind::Schema if name == "public" => USAGE,
            Kind::Database => CONNECT | TEMPORARY,
            _ => 0,
        }
    }
}

/// `SELECT`.
pub const SELECT: u32 = 1;
/// `INSERT`.
pub const INSERT: u32 = 2;
/// `UPDATE`.
pub const UPDATE: u32 = 4;
/// `DELETE`.
pub const DELETE: u32 = 8;
/// `TRUNCATE`.
pub const TRUNCATE: u32 = 16;
/// `REFERENCES`.
pub const REFERENCES: u32 = 32;
/// `USAGE`.
pub const USAGE: u32 = 64;
/// `CREATE`.
pub const CREATE: u32 = 128;
/// `EXECUTE`.
pub const EXECUTE: u32 = 256;
/// `CONNECT`.
pub const CONNECT: u32 = 512;
/// `TEMPORARY`.
pub const TEMPORARY: u32 = 1024;

const NAMES: [(u32, &str, char); 11] = [
    (SELECT, "SELECT", 'r'),
    (INSERT, "INSERT", 'a'),
    (UPDATE, "UPDATE", 'w'),
    (DELETE, "DELETE", 'd'),
    (TRUNCATE, "TRUNCATE", 'D'),
    (REFERENCES, "REFERENCES", 'x'),
    (USAGE, "USAGE", 'U'),
    (CREATE, "CREATE", 'C'),
    (EXECUTE, "EXECUTE", 'X'),
    (CONNECT, "CONNECT", 'c'),
    (TEMPORARY, "TEMPORARY", 'T'),
];

/// A privilege by its SQL name.
pub fn by_name(name: &str) -> Option<u32> {
    let n = name.to_ascii_uppercase();
    let n = if n == "TEMP" {
        "TEMPORARY".to_string()
    } else {
        n
    };
    NAMES.iter().find(|(_, w, _)| *w == n).map(|(b, _, _)| *b)
}

/// The SQL names of a set of privileges.
pub fn names(bits: u32) -> Vec<&'static str> {
    NAMES
        .iter()
        .filter(|(b, _, _)| bits & b != 0)
        .map(|(_, w, _)| *w)
        .collect()
}

/// PostgreSQL's letters for a set (`arwd`), with `*` after those that
/// can be granted on.
pub fn letters(bits: u32, grantable: u32) -> String {
    let mut s = String::new();
    for (b, _, c) in NAMES {
        if bits & b != 0 {
            s.push(c);
            if grantable & b != 0 {
                s.push('*');
            }
        }
    }
    s
}

/// One grantee's entry in an ACL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Privileges on the whole object.
    pub privileges: u32,
    /// Those it may grant to others.
    pub grantable: u32,
    /// Privileges on columns (tables only): column → (privileges,
    /// grantable).
    pub columns: BTreeMap<String, (u32, u32)>,
}

/// An object's ACL: grantee → entry.
pub type Acl = BTreeMap<String, Entry>;

fn acl_key(kind: Kind, name: &str) -> Vec<u8> {
    let mut k = ACLS.to_vec();
    k.push(kind.tag());
    k.extend_from_slice(name.as_bytes());
    k
}

fn encode(acl: &Acl) -> Vec<u8> {
    let mut w = Writer::default();
    w.u8(VERSION).u32(acl.len() as u32);
    for (grantee, e) in acl {
        w.str(grantee)
            .u32(e.privileges)
            .u32(e.grantable)
            .u32(e.columns.len() as u32);
        for (c, (p, g)) in &e.columns {
            w.str(c).u32(*p).u32(*g);
        }
    }
    w.0
}

fn decode(b: &[u8]) -> Result<Acl> {
    let mut r = Reader::new(b);
    if r.u8()? != VERSION {
        return Err(Error::internal("unknown ACL record version"));
    }
    let mut acl = Acl::new();
    for _ in 0..r.u32()? {
        let grantee = r.str()?;
        let mut e = Entry {
            privileges: r.u32()?,
            grantable: r.u32()?,
            columns: BTreeMap::new(),
        };
        for _ in 0..r.u32()? {
            let c = r.str()?;
            e.columns.insert(c, (r.u32()?, r.u32()?));
        }
        acl.insert(grantee, e);
    }
    Ok(acl)
}

/// The owner of an object: a table's maker; the rest belong to the
/// administrators (`None`).
pub fn owner(txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<Option<String>> {
    Ok(match kind {
        Kind::Table => crate::catalog::get_table(txn, name)?.and_then(|t| t.owner),
        Kind::Type => crate::enums::get(txn, name)?.and_then(|e| e.owner),
        _ => None,
    })
}

/// An object's ACL: its record, or its kind's default.
pub fn acl(txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<Acl> {
    if let Some(b) = txn.get(&acl_key(kind, name))? {
        return decode(&b);
    }
    let mut acl = Acl::new();
    if let Some(o) = owner(txn, kind, name)? {
        let all = kind.privileges();
        acl.insert(
            o,
            Entry {
                privileges: all,
                grantable: all,
                columns: BTreeMap::new(),
            },
        );
    }
    let public = kind.public_default(name);
    if public != 0 {
        acl.insert(
            PUBLIC.into(),
            Entry {
                privileges: public,
                ..Entry::default()
            },
        );
    }
    Ok(acl)
}

/// Whether an object has an ACL record (not its default).
pub fn has_record(txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<bool> {
    Ok(txn.get(&acl_key(kind, name))?.is_some())
}

/// Stores an object's ACL.
pub fn put_acl(txn: &mut dyn KvTxn, kind: Kind, name: &str, acl: &Acl) -> Result<()> {
    txn.put(&acl_key(kind, name), &encode(acl))?;
    Ok(())
}

/// Forgets an object's ACL (it was dropped).
pub fn forget(txn: &mut dyn KvTxn, kind: Kind, name: &str) -> Result<()> {
    txn.delete(&acl_key(kind, name))?;
    Ok(())
}

/// Every ACL record: kind, object, ACL.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<(Kind, String, Acl)>> {
    let end = nucleus_kv::prefix_end(&ACLS).expect("ACL prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&ACLS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        let (k, v) = e?;
        let kind = match k.get(2) {
            Some(1) => Kind::Table,
            Some(2) => Kind::Schema,
            Some(3) => Kind::Function,
            Some(4) => Kind::Type,
            Some(5) => Kind::Database,
            _ => continue,
        };
        out.push((
            kind,
            String::from_utf8_lossy(&k[3..]).into_owned(),
            decode(&v)?,
        ));
    }
    Ok(out)
}

fn defaults_key(role: &str, kind: Kind) -> Vec<u8> {
    let mut k = DEFAULTS.to_vec();
    k.extend_from_slice(role.as_bytes());
    k.push(0);
    k.push(kind.tag());
    k
}

/// What the objects `role` makes start with, beside its own privileges.
pub fn default_acl(txn: &dyn KvTxn, role: &str, kind: Kind) -> Result<Acl> {
    txn.get(&defaults_key(role, kind))?
        .map(|b| decode(&b))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Stores `role`'s default privileges for objects of `kind`.
pub fn put_default_acl(txn: &mut dyn KvTxn, role: &str, kind: Kind, acl: &Acl) -> Result<()> {
    if acl
        .values()
        .all(|e| e.privileges == 0 && e.columns.is_empty())
    {
        txn.delete(&defaults_key(role, kind))?;
    } else {
        txn.put(&defaults_key(role, kind), &encode(acl))?;
    }
    Ok(())
}

/// Every default-privileges record: role, kind, ACL.
pub fn all_defaults(txn: &dyn KvTxn) -> Result<Vec<(String, Kind, Acl)>> {
    let end = nucleus_kv::prefix_end(&DEFAULTS).expect("defaults prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&DEFAULTS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        let (k, v) = e?;
        let rest = &k[2..];
        let Some(at) = rest.iter().position(|&b| b == 0) else {
            continue;
        };
        let kind = match rest.get(at + 1) {
            Some(1) => Kind::Table,
            _ => continue,
        };
        out.push((
            String::from_utf8_lossy(&rest[..at]).into_owned(),
            kind,
            decode(&v)?,
        ));
    }
    Ok(out)
}

/// A new table's ACL: its owner's default privileges applied (nothing to
/// store when there are none).
pub fn on_create_table(txn: &mut dyn KvTxn, table: &str, owner: &str) -> Result<()> {
    let extra = default_acl(&*txn, owner, Kind::Table)?;
    if extra.is_empty() {
        return Ok(());
    }
    let mut acl = acl(&*txn, Kind::Table, table)?;
    for (grantee, e) in extra {
        let into = acl.entry(grantee).or_default();
        into.privileges |= e.privileges;
        into.grantable |= e.grantable;
    }
    put_acl(txn, Kind::Table, table, &acl)
}

// ------------------------------------------------------------- checking

/// The privilege checks of one statement: who, and what it has looked up.
pub struct Checker {
    who: std::sync::Arc<Who>,
    roles: RefCell<Option<BTreeSet<String>>>,
    acls: RefCell<HashMap<(Kind, String), Acl>>,
    owners: RefCell<HashMap<(Kind, String), Option<String>>>,
}

impl Checker {
    /// Checks for the current identity ([`crate::access::current`]).
    pub fn new() -> Self {
        Checker {
            who: crate::access::current(),
            roles: RefCell::new(None),
            acls: RefCell::new(HashMap::new()),
            owners: RefCell::new(HashMap::new()),
        }
    }

    /// The roles whose privileges the current role has (itself, those it
    /// inherits, and `PUBLIC`).
    fn roles(&self, txn: &dyn KvTxn) -> Result<BTreeSet<String>> {
        if let Some(r) = &*self.roles.borrow() {
            return Ok(r.clone());
        }
        let mut r = crate::roles::effective_for(txn, &self.who)?;
        r.insert(PUBLIC.into());
        *self.roles.borrow_mut() = Some(r.clone());
        Ok(r)
    }

    fn acl(&self, txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<Acl> {
        let key = (kind, name.to_string());
        if let Some(a) = self.acls.borrow().get(&key) {
            return Ok(a.clone());
        }
        let a = acl(txn, kind, name)?;
        self.acls.borrow_mut().insert(key, a.clone());
        Ok(a)
    }

    /// Whose privileges a statement needs: the current role's (its roles,
    /// and whether it is an administrator) — and, under an access rule's
    /// `AS ROLE r` ceiling, `r`'s as well.
    fn subjects(&self, txn: &dyn KvTxn) -> Result<Vec<(BTreeSet<String>, bool)>> {
        let mut out = vec![(self.roles(txn)?, self.who.superuser)];
        if let Some(r) = &self.who.ceiling.as_role {
            let mut roles = crate::roles::effective(txn, r)?;
            roles.insert(PUBLIC.into());
            let superuser = crate::roles::get(txn, r)?.is_some_and(|role| role.superuser);
            out.push((roles, superuser));
        }
        Ok(out)
    }

    fn owner_of(&self, txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<Option<String>> {
        let key = (kind, name.to_string());
        let cached = self.owners.borrow().get(&key).cloned();
        Ok(match cached {
            Some(o) => o,
            None => {
                let o = owner(txn, kind, name)?;
                self.owners.borrow_mut().insert(key, o.clone());
                o
            }
        })
    }

    /// Whether the current role owns the object (or is an administrator).
    pub fn owns(&self, txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<bool> {
        if !self.within_ceiling(kind, name, 0) {
            return Ok(false);
        }
        let owner = self.owner_of(txn, kind, name)?;
        Ok(self.subjects(txn)?.iter().all(|(roles, superuser)| {
            *superuser || owner.as_ref().is_some_and(|o| roles.contains(o))
        }))
    }

    /// Whether the current role has `privilege` on the object — on the
    /// whole of it, or (`column`) on that column.
    pub fn has(
        &self,
        txn: &dyn KvTxn,
        kind: Kind,
        name: &str,
        privilege: u32,
        column: Option<&str>,
    ) -> Result<bool> {
        if !self.within_ceiling(kind, name, privilege) {
            return Ok(false);
        }
        let owner = self.owner_of(txn, kind, name)?;
        let acl = self.acl(txn, kind, name)?;
        Ok(self.subjects(txn)?.iter().all(|(roles, superuser)| {
            *superuser
                || owner.as_ref().is_some_and(|o| roles.contains(o))
                || acl
                    .iter()
                    .filter(|(g, _)| roles.contains(*g))
                    .any(|(_, e)| {
                        e.privileges & privilege != 0
                            || column.is_some_and(|c| {
                                e.columns.get(c).is_some_and(|(p, _)| p & privilege != 0)
                            })
                    })
        }))
    }

    /// Whether the session's ceiling ([`crate::access::Ceiling`]) lets it
    /// touch the object: with a table scope, no other table, and no table
    /// created.
    fn within_ceiling(&self, kind: Kind, name: &str, privilege: u32) -> bool {
        let c = &self.who.ceiling;
        match kind {
            Kind::Table => c.admits_table(name),
            Kind::Schema => c.tables.is_none() || privilege & CREATE == 0,
            _ => true,
        }
    }

    /// Whether the current role has `privilege` on any column of a table
    /// (or the whole table).
    pub fn has_any_column(&self, txn: &dyn KvTxn, table: &str, privilege: u32) -> Result<bool> {
        if self.has(txn, Kind::Table, table, privilege, None)? {
            return Ok(true);
        }
        if !self.within_ceiling(Kind::Table, table, privilege) {
            return Ok(false);
        }
        let owner = self.owner_of(txn, Kind::Table, table)?;
        let acl = self.acl(txn, Kind::Table, table)?;
        Ok(self.subjects(txn)?.iter().all(|(roles, superuser)| {
            *superuser
                || owner.as_ref().is_some_and(|o| roles.contains(o))
                || acl
                    .iter()
                    .filter(|(g, _)| roles.contains(*g))
                    .any(|(_, e)| {
                        e.privileges & privilege != 0
                            || e.columns.values().any(|(p, _)| p & privilege != 0)
                    })
        }))
    }

    /// `42501` unless the current role has `privilege` (see [`Checker::has`]).
    pub fn require(
        &self,
        txn: &dyn KvTxn,
        kind: Kind,
        name: &str,
        privilege: u32,
        column: Option<&str>,
    ) -> Result<()> {
        if self.has(txn, kind, name, privilege, column)? {
            return Ok(());
        }
        Err(denied(kind, name, privilege, column))
    }

    /// `42501` unless the current role owns the object.
    pub fn require_owner(&self, txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<()> {
        if self.owns(txn, kind, name)? {
            return Ok(());
        }
        Err(Error::insufficient_privilege(format!(
            "must be owner of {} {name}",
            kind.word()
        )))
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

/// `42501`: the privilege missing, and the object.
pub fn denied(kind: Kind, name: &str, privilege: u32, column: Option<&str>) -> Error {
    let p = names(privilege).join(", ");
    Error::insufficient_privilege(match column {
        Some(c) => format!(
            "permission denied: {p} on column {c} of {} {name}",
            kind.word()
        ),
        None => format!("permission denied: {p} on {} {name}", kind.word()),
    })
}

// ---------------------------------------------------------- GRANT, REVOKE

/// What a `GRANT` or `REVOKE` names.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// The kind of object.
    pub kind: Kind,
    /// The objects (`ALL TABLES IN SCHEMA public` is expanded when run).
    pub objects: Vec<String>,
    /// `ALL TABLES IN SCHEMA s`.
    pub all_in_schema: Option<String>,
}

/// One privilege as written: its bits (all of the kind's for `ALL`), and
/// columns it is limited to.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The privilege (`None`: `ALL`).
    pub privilege: Option<u32>,
    /// The columns (empty: the whole object).
    pub columns: Vec<String>,
}

/// Runs a `GRANT` (`revoke` false) or a `REVOKE`.
#[allow(clippy::too_many_arguments)]
pub fn grant(
    txn: &mut dyn KvTxn,
    items: &[Item],
    target: &Target,
    grantees: &[String],
    grant_option: bool,
    revoke: bool,
) -> Result<()> {
    let objects = match &target.all_in_schema {
        Some(s) if s == "public" => crate::catalog::tables(&*txn)?
            .into_iter()
            .map(|t| t.name)
            .collect(),
        Some(s) => {
            return Err(Error::invalid_schema(format!(
                "schema \"{s}\" has no tables of its own"
            )));
        }
        None => target.objects.clone(),
    };
    for g in grantees {
        if g != PUBLIC && crate::roles::get(&*txn, g)?.is_none() {
            return Err(Error::undefined_object(format!(
                "role \"{g}\" does not exist"
            )));
        }
    }
    let checker = Checker::new();
    for object in &objects {
        exists(&*txn, target.kind, object)?;
        let mut acl = acl(&*txn, target.kind, object)?;
        // What the current role may grant here: everything if it owns the
        // object, else what it holds with the grant option.
        let owns = checker.owns(&*txn, target.kind, object)?;
        let roles = checker.roles(&*txn)?;
        let mayg = |column: Option<&str>| -> u32 {
            if owns {
                return u32::MAX;
            }
            acl.iter()
                .filter(|(g, _)| roles.contains(*g))
                .map(|(_, e)| {
                    e.grantable | column.and_then(|c| e.columns.get(c)).map_or(0, |(_, g)| *g)
                })
                .fold(0, |a, b| a | b)
        };
        let mut changes: Vec<(Option<String>, u32)> = Vec::new();
        for item in items {
            let bits = item.privilege.unwrap_or(target.kind.privileges());
            if bits & !target.kind.privileges() != 0 {
                return Err(Error::invalid_grant(format!(
                    "invalid privilege type {} for {}",
                    names(bits & !target.kind.privileges()).join(", "),
                    target.kind.word()
                )));
            }
            if !item.columns.is_empty() {
                let t = crate::catalog::table(&*txn, object)?;
                for c in &item.columns {
                    if t.column(c).is_none() {
                        return Err(Error::undefined_column(format!(
                            "column \"{c}\" of relation \"{object}\" does not exist"
                        )));
                    }
                    changes.push((Some(c.clone()), bits));
                }
            } else {
                changes.push((None, bits));
            }
        }
        for (column, bits) in &changes {
            let allowed = mayg(column.as_deref()) & bits;
            if allowed != *bits {
                return Err(Error::insufficient_privilege(format!(
                    "permission denied: no grant option for {} on {} {object}",
                    names(bits & !allowed).join(", "),
                    target.kind.word()
                )));
            }
        }
        for g in grantees {
            let e = acl.entry(g.clone()).or_default();
            for (column, bits) in &changes {
                let (p, gr) = match column {
                    None => (&mut e.privileges, &mut e.grantable),
                    Some(c) => {
                        let slot = e.columns.entry(c.clone()).or_insert((0, 0));
                        (&mut slot.0, &mut slot.1)
                    }
                };
                match (revoke, grant_option) {
                    (false, false) => *p |= bits,
                    (false, true) => {
                        *p |= bits;
                        *gr |= bits;
                    }
                    (true, true) => *gr &= !bits,
                    (true, false) => {
                        *p &= !bits;
                        *gr &= !bits;
                    }
                }
                // Revoking on the table revokes on each column too (as
                // PostgreSQL does).
                if revoke && column.is_none() {
                    for (cp, cg) in e.columns.values_mut() {
                        if !grant_option {
                            *cp &= !bits;
                        }
                        *cg &= !bits;
                    }
                }
            }
            e.columns.retain(|_, (p, _)| *p != 0);
        }
        acl.retain(|_, e| e.privileges != 0 || !e.columns.is_empty());
        put_acl(txn, target.kind, object, &acl)?;
    }
    Ok(())
}

/// `ALTER DEFAULT PRIVILEGES [FOR ROLE r] GRANT | REVOKE … ON TABLES …`.
pub fn alter_defaults(
    txn: &mut dyn KvTxn,
    for_role: Option<&str>,
    privileges: Option<u32>,
    grantees: &[String],
    grant_option: bool,
    revoke: bool,
) -> Result<()> {
    let who = crate::access::current();
    let role = for_role.unwrap_or(&who.user).to_string();
    if !who.superuser && !crate::roles::is_member(&*txn, &who.user, &role)? {
        return Err(Error::insufficient_privilege(format!(
            "permission denied: default privileges of role \"{role}\""
        )));
    }
    let bits = privileges.unwrap_or(Kind::Table.privileges());
    let mut acl = default_acl(&*txn, &role, Kind::Table)?;
    for g in grantees {
        let e = acl.entry(g.clone()).or_default();
        if revoke {
            e.privileges &= !bits;
            e.grantable &= !bits;
        } else {
            e.privileges |= bits;
            if grant_option {
                e.grantable |= bits;
            }
        }
    }
    acl.retain(|_, e| e.privileges != 0);
    put_default_acl(txn, &role, Kind::Table, &acl)
}

/// `42P01` / `42883` / `42704` if the object does not exist.
fn exists(txn: &dyn KvTxn, kind: Kind, name: &str) -> Result<()> {
    let found = match kind {
        Kind::Table => crate::catalog::get_table(txn, name)?.is_some(),
        Kind::Schema => name == "public",
        Kind::Database => name == DATABASE,
        Kind::Function | Kind::Type => true, // checked by the session against its registry
    };
    if found {
        Ok(())
    } else {
        Err(match kind {
            Kind::Table => Error::undefined_table(format!("relation \"{name}\" does not exist")),
            Kind::Database => {
                Error::undefined_database(format!("database \"{name}\" does not exist"))
            }
            _ => Error::invalid_schema(format!("schema \"{name}\" does not exist")),
        })
    }
}

/// The objects `role` holds privileges on (`kind name`), for `DROP ROLE`.
pub fn held_by(txn: &dyn KvTxn, role: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = all(txn)?
        .into_iter()
        .filter(|(_, _, acl)| acl.contains_key(role))
        .map(|(k, n, _)| format!("privileges on {} {n}", k.word()))
        .collect();
    for (owner, _, acl) in all_defaults(txn)? {
        if owner == role || acl.contains_key(role) {
            out.push(format!("default privileges of role {owner}"));
        }
    }
    Ok(out)
}

/// ACL entries of `from` become `to`'s (a renamed role).
pub fn rename_grantee(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for (kind, name, mut acl) in all(&*txn)? {
        if let Some(e) = acl.remove(from) {
            acl.insert(to.to_string(), e);
            put_acl(txn, kind, &name, &acl)?;
        }
    }
    for (owner, kind, mut acl) in all_defaults(&*txn)? {
        let moved = acl.remove(from);
        if let Some(e) = &moved {
            acl.insert(to.to_string(), e.clone());
        }
        if owner == from {
            txn.delete(&defaults_key(from, kind))?;
            put_default_acl(txn, to, kind, &acl)?;
        } else if moved.is_some() {
            put_default_acl(txn, &owner, kind, &acl)?;
        }
    }
    Ok(())
}

/// The database's name (one per store).
pub const DATABASE: &str = "nucleus";

/// What a DDL statement needs of the current role, checked before it
/// runs: `CREATE` on schema `public` to make a table or a type, the
/// owner's rights to change or drop one or its indexes, `REFERENCES` on
/// the columns a foreign key references, `USAGE` on the types columns
/// get.  Other statements need nothing here.
pub fn authorize_ddl(
    txn: &dyn KvTxn,
    reg: &crate::ext::Registry,
    stmt: &crate::ast::Statement,
) -> Result<()> {
    use crate::ast::{SqlType, Statement, TableConstraint};
    let c = Checker::new();
    let references = |r: &crate::ast::References| -> Result<()> {
        let parent = crate::catalog::table(txn, &r.table)?;
        let cols: Vec<String> = if r.columns.is_empty() {
            parent
                .primary_key
                .iter()
                .map(|&i| parent.columns[i].name.clone())
                .collect()
        } else {
            r.columns.clone()
        };
        for col in &cols {
            c.require(txn, Kind::Table, &r.table, REFERENCES, Some(col))?;
        }
        Ok(())
    };
    let type_usage = |t: &SqlType| -> Result<()> {
        if let SqlType::Named { name, .. } = t {
            c.require(txn, Kind::Type, name, USAGE, None)?;
        }
        Ok(())
    };
    let _ = reg;
    match stmt {
        Statement::CreateTable {
            name,
            columns,
            constraints,
            ..
        } => {
            c.require(txn, Kind::Schema, "public", CREATE, None)?;
            // (A table referencing itself is its maker's.)
            let other = |r: &crate::ast::References| r.table != *name;
            for col in columns {
                type_usage(&col.ty)?;
                if let Some((_, r)) = &col.references
                    && other(r)
                {
                    references(r)?;
                }
            }
            for k in constraints {
                if let TableConstraint::ForeignKey { references: r, .. } = k
                    && other(r)
                {
                    references(r)?;
                }
            }
        }
        Statement::CreateEnum { .. } => c.require(txn, Kind::Schema, "public", CREATE, None)?,
        Statement::CreateIndex { table, .. }
        | Statement::AlterColumnType { table, .. }
        | Statement::DropConstraint { table, .. }
        | Statement::SetRowSecurity { table, .. }
        | Statement::DropPolicy { table, .. }
        | Statement::CreatePolicy(crate::policies::Policy { table, .. }) => {
            if crate::catalog::get_table(txn, table)?.is_some() {
                c.require_owner(txn, Kind::Table, table)?;
            }
        }
        Statement::AddColumn { table, column } => {
            if crate::catalog::get_table(txn, table)?.is_some() {
                c.require_owner(txn, Kind::Table, table)?;
            }
            type_usage(&column.ty)?;
            if let Some((_, r)) = &column.references {
                references(r)?;
            }
        }
        Statement::AddConstraint { table, constraint } => {
            if crate::catalog::get_table(txn, table)?.is_some() {
                c.require_owner(txn, Kind::Table, table)?;
            }
            if let TableConstraint::ForeignKey { references: r, .. } = constraint {
                references(r)?;
            }
        }
        Statement::DropIndex { name, .. } => {
            if let Some((t, _)) = crate::catalog::find_index(txn, name)? {
                c.require_owner(txn, Kind::Table, &t.name)?;
            }
        }
        Statement::AlterType { type_name, .. }
        | Statement::DropType {
            name: type_name, ..
        } => {
            c.require_owner(txn, Kind::Type, type_name)?;
        }
        Statement::Analyze(Some(table)) if crate::catalog::get_table(txn, table)?.is_some() => {
            c.require_owner(txn, Kind::Table, table)?;
        }
        _ => {}
    }
    Ok(())
}
