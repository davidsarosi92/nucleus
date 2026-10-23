//! Row-level security (PLAN.md §M14.25): `ALTER TABLE t ENABLE ROW
//! LEVEL SECURITY`, then `CREATE POLICY` — the rows a role may see and
//! write, by predicates over the row and the session (`current_user`,
//! `current_setting('nucleus.tenant')`, …).
//!
//! Policies are in the catalog, `00 0E <table> 00 <policy>`.  On a table
//! with row-level security, for a role that is neither an administrator
//! nor — unless the table *forces* it — the owner, a command sees and
//! changes only rows that pass `(permissive USING …, OR-ed) AND
//! (restrictive USING …, AND-ed)` of the policies for that command and
//! role; with no permissive policy, no row.  New rows (`INSERT`, and
//! `UPDATE`'s result) must pass the `WITH CHECK` predicates (or `USING`
//! where a policy has none): `42501` otherwise.
//!
//! The binder applies them where a table is read, under a barrier
//! (`Plan::Secure`): the optimizer may narrow the scan by a key range,
//! but no predicate of the statement is evaluated on a row before the
//! policies have passed it — so an error or a side effect cannot reveal a
//! hidden row.

use nucleus_kv::KvTxn;

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const POLICIES: [u8; 2] = [0x00, 0x0E];
const VERSION: u8 = 1;

/// The command a policy is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Every command.
    All,
    /// `SELECT` (and the rows `UPDATE` and `DELETE` find).
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

impl Command {
    fn tag(self) -> u8 {
        match self {
            Command::All => 0,
            Command::Select => 1,
            Command::Insert => 2,
            Command::Update => 3,
            Command::Delete => 4,
        }
    }

    fn of_tag(t: u8) -> Result<Command> {
        Ok(match t {
            0 => Command::All,
            1 => Command::Select,
            2 => Command::Insert,
            3 => Command::Update,
            4 => Command::Delete,
            _ => return Err(Error::internal("unknown policy command")),
        })
    }

    /// Its SQL word.
    pub fn word(self) -> &'static str {
        match self {
            Command::All => "ALL",
            Command::Select => "SELECT",
            Command::Insert => "INSERT",
            Command::Update => "UPDATE",
            Command::Delete => "DELETE",
        }
    }
}

/// A policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Its name, unique on its table.
    pub name: String,
    /// Its table.
    pub table: String,
    /// `PERMISSIVE` (else `RESTRICTIVE`).
    pub permissive: bool,
    /// The command it is for.
    pub command: Command,
    /// The roles it applies to (`public`: every role).
    pub roles: Vec<String>,
    /// `USING (…)`: the rows it lets through, as SQL over the row.
    pub using: Option<String>,
    /// `WITH CHECK (…)`: the new rows it admits.
    pub check: Option<String>,
}

fn key(table: &str, name: &str) -> Vec<u8> {
    let mut k = POLICIES.to_vec();
    k.extend_from_slice(table.as_bytes());
    k.push(0);
    k.extend_from_slice(name.as_bytes());
    k
}

fn prefix(table: &str) -> Vec<u8> {
    let mut k = POLICIES.to_vec();
    k.extend_from_slice(table.as_bytes());
    k.push(0);
    k
}

impl Policy {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .str(&self.name)
            .str(&self.table)
            .u8(u8::from(self.permissive))
            .u8(self.command.tag())
            .u32(self.roles.len() as u32);
        for r in &self.roles {
            w.str(r);
        }
        w.opt_str(self.using.as_deref())
            .opt_str(self.check.as_deref());
        w.0
    }

    fn decode(b: &[u8]) -> Result<Policy> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown policy record version"));
        }
        let name = r.str()?;
        let table = r.str()?;
        let permissive = r.u8()? == 1;
        let command = Command::of_tag(r.u8()?)?;
        let n = r.u32()? as usize;
        let roles = (0..n).map(|_| r.str()).collect::<Result<_>>()?;
        Ok(Policy {
            name,
            table,
            permissive,
            command,
            roles,
            using: r.opt_str()?,
            check: r.opt_str()?,
        })
    }
}

/// The policies of `table`.
pub fn of(txn: &dyn KvTxn, table: &str) -> Result<Vec<Policy>> {
    let p = prefix(table);
    let end = nucleus_kv::prefix_end(&p).expect("policy prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&p[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Policy::decode(&e?.1)?);
    }
    Ok(out)
}

/// Every policy.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Policy>> {
    let end = nucleus_kv::prefix_end(&POLICIES).expect("policy prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&POLICIES[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Policy::decode(&e?.1)?);
    }
    Ok(out)
}

/// The policy `name` of `table`.
pub fn get(txn: &dyn KvTxn, table: &str, name: &str) -> Result<Option<Policy>> {
    txn.get(&key(table, name))?
        .map(|b| Policy::decode(&b))
        .transpose()
}

/// Stores a policy.
pub fn put(txn: &mut dyn KvTxn, p: &Policy) -> Result<()> {
    txn.put(&key(&p.table, &p.name), &p.encode())?;
    Ok(())
}

/// Deletes a policy.
pub fn delete(txn: &mut dyn KvTxn, table: &str, name: &str) -> Result<()> {
    txn.delete(&key(table, name))?;
    Ok(())
}

/// Deletes every policy of `table` (it was dropped).
pub fn forget(txn: &mut dyn KvTxn, table: &str) -> Result<()> {
    for p in of(&*txn, table)? {
        delete(txn, table, &p.name)?;
    }
    Ok(())
}

/// What a command on a table must pass: the SQL texts of its `USING`
/// (rows it may touch) and `WITH CHECK` (new rows it may write)
/// conditions — `None` when row-level security does not apply to the
/// current role.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    /// The condition on existing rows (`false` when no policy admits any).
    pub using: String,
    /// The condition on new rows.
    pub check: String,
}

/// The policies that apply to `command` on `table` for the current role,
/// combined (see the module documentation).
pub fn applied(
    txn: &dyn KvTxn,
    table: &crate::catalog::Table,
    command: Command,
) -> Result<Option<Applied>> {
    if !table.row_security {
        return Ok(None);
    }
    let who = crate::access::current();
    if who.superuser {
        return Ok(None);
    }
    let roles = crate::roles::effective_for(txn, &who)?;
    if !table.force_row_security && table.owner.as_ref().is_some_and(|o| roles.contains(o)) {
        return Ok(None);
    }
    let mine: Vec<Policy> = of(txn, &table.name)?
        .into_iter()
        .filter(|p| p.command == Command::All || p.command == command)
        .filter(|p| {
            p.roles
                .iter()
                .any(|r| r == crate::privileges::PUBLIC || roles.contains(r))
        })
        .collect();
    let paren = |s: &str| format!("({s})");
    let combine = |pick: &dyn Fn(&Policy) -> Option<String>| -> String {
        let permissive: Vec<String> = mine
            .iter()
            .filter(|p| p.permissive)
            .map(|p| pick(p).map_or("true".into(), |s| paren(&s)))
            .collect();
        let restrictive: Vec<String> = mine
            .iter()
            .filter(|p| !p.permissive)
            .filter_map(|p| pick(p).map(|s| paren(&s)))
            .collect();
        let mut parts = vec![if permissive.is_empty() {
            "false".to_string()
        } else {
            paren(&permissive.join(" OR "))
        }];
        parts.extend(restrictive);
        parts.join(" AND ")
    };
    Ok(Some(Applied {
        using: combine(&|p| p.using.clone()),
        check: combine(&|p| p.check.clone().or_else(|| p.using.clone())),
    }))
}

/// `42501`: a new row a policy does not admit.
pub fn violated(table: &str) -> Error {
    Error::insufficient_privilege(format!(
        "new row violates row-level security policy for table \"{table}\""
    ))
}
