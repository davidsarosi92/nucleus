//! The audit log (PLAN.md §M14.25): logins, DDL and every change of
//! access, always; statements as the database's settings ask; a hash
//! chain, so a gap or an edit shows.
//!
//! A record is in the catalog (`00 12 <seq>`, the chain's head at `00
//! 13`), written in the transaction of what it records — a statement rolled
//! back leaves none, a committed one always has one — and refused
//! statements, and failed logins, in a transaction of their own.  Each
//! record's hash is SHA-256 over its fields and the previous record's hash;
//! [`check`] walks the chain (`nc_catalog.audit_chain`): numbers one after
//! another, each hash the previous one's successor, the head the last.
//! Only an administrator reads it (`nc_catalog.audit`).
//!
//! What is recorded: `login` (success or refusal, with the method, the
//! address and the rule), `ddl` (every statement that changes the catalog:
//! tables, types, roles, privileges, policies, tokens, rules, settings),
//! and with `audit_statements = all`, `audit_roles` (the statements of
//! these roles' members) or `audit_tables` (those that touch these
//! tables), `statement`.

use std::cell::RefCell;
use std::collections::BTreeSet;

use nucleus_kv::KvTxn;

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const RECORDS: [u8; 2] = [0x00, 0x12];
const HEAD: [u8; 2] = [0x00, 0x13];

/// A record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Its number: 1, 2, …
    pub seq: u64,
    /// When (µs since 2000-01-01 UTC).
    pub at: i64,
    /// `login`, `ddl`, `statement`.
    pub kind: String,
    /// Whether it went through (a login admitted, a statement committed).
    pub ok: bool,
    /// The SQLSTATE of a refusal.
    pub code: String,
    /// Who logged in.
    pub session_user: String,
    /// Who acted.
    pub user: String,
    /// The connection.
    pub context: crate::access::Context,
    /// The access rule that admitted the session.
    pub rule: Option<String>,
    /// The statement's text, or what happened.
    pub detail: String,
    /// The previous record's hash.
    pub prev: Vec<u8>,
    /// Its hash.
    pub hash: Vec<u8>,
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

fn record_key(seq: u64) -> Vec<u8> {
    let mut k = RECORDS.to_vec();
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

impl Record {
    /// Its fields, as hashed (all but the hash).
    fn fields(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u64(self.seq)
            .u64(self.at as u64)
            .str(&self.kind)
            .u8(u8::from(self.ok))
            .str(&self.code)
            .str(&self.session_user)
            .str(&self.user)
            .opt_str(
                self.context
                    .client_addr
                    .map(|a| crate::rules::host(a).host_text())
                    .as_deref(),
            )
            .str(&self.context.front_end)
            .str(&self.context.auth_method)
            .u8(u8::from(self.context.tls))
            .opt_str(self.rule.as_deref())
            .str(&self.detail)
            .str(&hex(&self.prev));
        w.0
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = self.fields();
        b.extend_from_slice(&self.hash);
        b
    }

    fn decode(b: &[u8]) -> Result<Record> {
        if b.len() < 32 {
            return Err(Error::internal("a malformed audit record"));
        }
        let (fields, hash) = b.split_at(b.len() - 32);
        let mut r = Reader::new(fields);
        let seq = r.u64()?;
        let at = r.u64()? as i64;
        let kind = r.str()?;
        let ok = r.u8()? == 1;
        let code = r.str()?;
        let session_user = r.str()?;
        let user = r.str()?;
        let client_addr = r.opt_str()?.and_then(|a| a.parse().ok());
        let front_end = r.str()?;
        let auth_method = r.str()?;
        let tls = r.u8()? == 1;
        let rule = r.opt_str()?;
        let detail = r.str()?;
        let prev = unhex(&r.str()?);
        Ok(Record {
            seq,
            at,
            kind,
            ok,
            code,
            session_user,
            user,
            context: crate::access::Context {
                client_addr,
                front_end,
                auth_method,
                tls,
            },
            rule,
            detail,
            prev,
            hash: hash.to_vec(),
        })
    }
}

/// Appends `r` (its number, previous hash and hash filled in) in `txn`.
pub fn append(txn: &mut dyn KvTxn, mut r: Record) -> Result<()> {
    let (seq, prev) = match txn.get(&HEAD)? {
        Some(h) if h.len() == 40 => (
            u64::from_be_bytes(h[..8].try_into().expect("8")) + 1,
            h[8..].to_vec(),
        ),
        _ => (1, vec![0; 32]),
    };
    r.seq = seq;
    r.prev = prev;
    r.hash = nucleus_crypto::sha256(&r.fields()).to_vec();
    txn.put(&record_key(seq), &r.encode())?;
    let mut head = seq.to_be_bytes().to_vec();
    head.extend_from_slice(&r.hash);
    txn.put(&HEAD, &head)?;
    Ok(())
}

/// A record of what `who` did now.
pub fn record_of(
    who: &crate::access::Who,
    kind: &str,
    ok: bool,
    code: &str,
    detail: &str,
) -> Record {
    Record {
        at: crate::datetime::clock().unwrap_or_default(),
        kind: kind.into(),
        ok,
        code: code.into(),
        session_user: who.session_user.clone(),
        user: who.user.clone(),
        context: who.context.clone(),
        rule: who.rule.clone(),
        detail: detail.into(),
        ..Record::default()
    }
}

/// Every record, in order (an administrator's).
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Record>> {
    let end = nucleus_kv::prefix_end(&RECORDS).expect("audit prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&RECORDS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Record::decode(&e?.1)?);
    }
    Ok(out)
}

/// What [`check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    /// The records.
    pub records: u64,
    /// Whether the chain holds.
    pub intact: bool,
    /// The first record where it does not (or the head, `records + 1`).
    pub broken_at: Option<u64>,
    /// Why.
    pub problem: Option<String>,
}

/// Walks the chain.
pub fn check(txn: &dyn KvTxn) -> Result<Chain> {
    let records = all(txn)?;
    let broken = |at: u64, why: &str| Chain {
        records: records.len() as u64,
        intact: false,
        broken_at: Some(at),
        problem: Some(why.into()),
    };
    let mut prev = vec![0u8; 32];
    for (i, r) in records.iter().enumerate() {
        let want = i as u64 + 1;
        if r.seq != want {
            return Ok(broken(want, "a record is missing"));
        }
        if r.prev != prev {
            return Ok(broken(r.seq, "it does not follow the record before it"));
        }
        if nucleus_crypto::sha256(&r.fields()).to_vec() != r.hash {
            return Ok(broken(r.seq, "it was changed"));
        }
        prev = r.hash.clone();
    }
    let head = txn.get(&HEAD)?;
    let ends = match &head {
        Some(h) if h.len() == 40 => {
            u64::from_be_bytes(h[..8].try_into().expect("8")) == records.len() as u64
                && h[8..] == prev[..]
        }
        None => records.is_empty(),
        _ => false,
    };
    if !ends {
        return Ok(broken(
            records.len() as u64 + 1,
            "the last records are missing (the head names more)",
        ));
    }
    Ok(Chain {
        records: records.len() as u64,
        intact: true,
        broken_at: None,
        problem: None,
    })
}

// ------------------------------------------------- what a statement touched

thread_local! {
    static TOUCHED: RefCell<Option<BTreeSet<String>>> = const { RefCell::new(None) };
}

/// Starts noting the tables the next statement's checks look at.
pub fn begin_statement() {
    TOUCHED.with(|t| *t.borrow_mut() = Some(BTreeSet::new()));
}

/// A table a privilege check looked at.
pub fn touched(table: &str) {
    TOUCHED.with(|t| {
        if let Some(set) = t.borrow_mut().as_mut() {
            set.insert(table.to_string());
        }
    });
}

/// The tables noted since [`begin_statement`].
pub fn end_statement() -> BTreeSet<String> {
    TOUCHED.with(|t| t.borrow_mut().take().unwrap_or_default())
}

/// Whether the database's settings ask for statements like this one to be
/// recorded: `audit_statements = all`, a role of `audit_roles` acting (or
/// one the actor is a member of), a table of `audit_tables` touched.
pub fn wanted(
    txn: &dyn KvTxn,
    who: &crate::access::Who,
    tables: &BTreeSet<String>,
) -> Result<bool> {
    if crate::dbsettings::get(txn, "audit_statements")? == "all" {
        return Ok(true);
    }
    let list = |name: &str| -> Result<Vec<String>> {
        Ok(crate::dbsettings::get(txn, name)?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    };
    for r in list("audit_roles")? {
        if crate::roles::is_member(txn, &who.user, &r)? {
            return Ok(true);
        }
    }
    Ok(list("audit_tables")?.iter().any(|t| tables.contains(t)))
}
