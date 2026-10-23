//! Tokens (PLAN.md §M14.25): API keys for programs and AI agents.
//!
//! `CREATE TOKEN name [FOR role] [READ ONLY] [ON TABLE t, …] [VALID UNTIL
//! '…']` makes one and returns, once, its login name (`nct_…`) and its
//! secret; a client logs in with them as user and password — by
//! SCRAM-SHA-256, as any password, so every driver and front end takes a
//! token as it is.  A token session acts as the token's role, under the
//! token's *ceiling*: `READ ONLY` (no statement may write: `25006`), `ON
//! TABLE …` (no other table may be touched, nor one created: `42501`).
//! The ceiling narrows the role's privileges, never widens them, and holds
//! through `SET ROLE`.  `DROP TOKEN` revokes one; dropping its role drops
//! it; a token past `VALID UNTIL` no longer logs in.
//!
//! Tokens are in the catalog, `00 0F <login>`, with the secret's SCRAM
//! verifier only — the secret itself is never stored.  A token session
//! cannot make tokens nor set passwords, so a leaked token leaves no
//! credential behind once it is revoked.

use std::collections::BTreeSet;

use nucleus_kv::KvTxn;

use crate::access::{self, denied};
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const TOKENS: [u8; 2] = [0x00, 0x0F];
/// 2: budgets (rows, scan, timeout, rate).
const VERSION: u8 = 2;

/// The prefix of every token's login name (role names may not take it).
pub const PREFIX: &str = "nct_";

/// A token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Its login name: [`PREFIX`] and 16 hex digits.
    pub login: String,
    /// Its name, unique among tokens.
    pub name: String,
    /// The role it acts as.
    pub role: String,
    /// `READ ONLY`.
    pub read_only: bool,
    /// `ON TABLE …`: the only tables it may touch (`None`: any its role
    /// may).
    pub tables: Option<Vec<String>>,
    /// When it stops working (µs since 2000-01-01 UTC).
    pub valid_until: Option<i64>,
    /// When it was made.
    pub created: i64,
    /// Who made it.
    pub created_by: String,
    /// Its secret's SCRAM-SHA-256 verifier.
    pub verifier: String,
    /// Its budgets, per statement and a minute.
    pub budget: Budgets,
}

/// A token's budgets (`None`: no limit).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Budgets {
    /// `ROWS n`: the most rows a statement may return.
    pub rows: Option<u64>,
    /// `SCAN n`: the most rows a statement may read.
    pub scan: Option<u64>,
    /// `TIMEOUT '…'`: the longest a statement may run (µs).
    pub timeout: Option<i64>,
    /// `RATE n PER MINUTE`: the most statements a minute.
    pub rate: Option<u32>,
}

fn opt_u64(w: &mut Writer, v: Option<u64>) {
    match v {
        Some(n) => w.u8(1).u64(n),
        None => w.u8(0),
    };
}

fn read_opt_u64(r: &mut Reader) -> Result<Option<u64>> {
    Ok(if r.u8()? == 1 { Some(r.u64()?) } else { None })
}

fn key(login: &str) -> Vec<u8> {
    let mut k = TOKENS.to_vec();
    k.extend_from_slice(login.as_bytes());
    k
}

impl Token {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .str(&self.login)
            .str(&self.name)
            .str(&self.role)
            .u8(u8::from(self.read_only));
        match &self.tables {
            Some(t) => {
                w.u8(1).u32(t.len() as u32);
                for name in t {
                    w.str(name);
                }
            }
            None => {
                w.u8(0);
            }
        }
        match self.valid_until {
            Some(t) => w.u8(1).u64(t as u64),
            None => w.u8(0),
        };
        w.u64(self.created as u64)
            .str(&self.created_by)
            .str(&self.verifier);
        opt_u64(&mut w, self.budget.rows);
        opt_u64(&mut w, self.budget.scan);
        opt_u64(&mut w, self.budget.timeout.map(|t| t as u64));
        opt_u64(&mut w, self.budget.rate.map(u64::from));
        w.0
    }

    fn decode(b: &[u8]) -> Result<Token> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if !(1..=VERSION).contains(&version) {
            return Err(Error::internal("unknown token record version"));
        }
        let login = r.str()?;
        let name = r.str()?;
        let role = r.str()?;
        let read_only = r.u8()? == 1;
        let tables = if r.u8()? == 1 {
            let n = r.u32()? as usize;
            Some((0..n).map(|_| r.str()).collect::<Result<_>>()?)
        } else {
            None
        };
        let valid_until = if r.u8()? == 1 {
            Some(r.u64()? as i64)
        } else {
            None
        };
        Ok(Token {
            login,
            name,
            role,
            read_only,
            tables,
            valid_until,
            created: r.u64()? as i64,
            created_by: r.str()?,
            verifier: r.str()?,
            budget: if version >= 2 {
                Budgets {
                    rows: read_opt_u64(&mut r)?,
                    scan: read_opt_u64(&mut r)?,
                    timeout: read_opt_u64(&mut r)?.map(|t| t as i64),
                    rate: read_opt_u64(&mut r)?.map(|n| n as u32),
                }
            } else {
                Budgets::default()
            },
        })
    }

    /// The ceiling a session of this token runs under.
    pub fn ceiling(&self) -> access::Ceiling {
        access::Ceiling {
            read_only: self.read_only,
            tables: self
                .tables
                .as_ref()
                .map(|t| t.iter().cloned().collect::<BTreeSet<_>>()),
            as_role: None,
            timeout: self.budget.timeout,
            max_scan: self.budget.scan,
            max_rows: self.budget.rows,
            rate: self.budget.rate,
        }
    }
}

/// Every token.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Token>> {
    let end = nucleus_kv::prefix_end(&TOKENS).expect("token prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&TOKENS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Token::decode(&e?.1)?);
    }
    Ok(out)
}

/// The token that logs in as `login`.
pub fn get(txn: &dyn KvTxn, login: &str) -> Result<Option<Token>> {
    if !login.starts_with(PREFIX) {
        return Ok(None);
    }
    txn.get(&key(login))?.map(|b| Token::decode(&b)).transpose()
}

fn by_name(txn: &dyn KvTxn, name: &str) -> Result<Option<Token>> {
    Ok(all(txn)?.into_iter().find(|t| t.name == name))
}

fn put(txn: &mut dyn KvTxn, t: &Token) -> Result<()> {
    txn.put(&key(&t.login), &t.encode())?;
    Ok(())
}

/// The token `login` logs in with, if it may now (it exists, its role
/// does, and it has not expired at `now`).
pub fn usable(txn: &dyn KvTxn, login: &str, now: i64) -> Result<Option<Token>> {
    let Some(t) = get(txn, login)? else {
        return Ok(None);
    };
    if t.valid_until.is_some_and(|v| v < now) || crate::roles::get(txn, &t.role)?.is_none() {
        return Ok(None);
    }
    Ok(Some(t))
}

/// Deletes the tokens of `role` (it was dropped).
pub fn forget_role(txn: &mut dyn KvTxn, role: &str) -> Result<()> {
    for t in all(&*txn)? {
        if t.role == role {
            txn.delete(&key(&t.login))?;
        }
    }
    Ok(())
}

/// Points the tokens of `from` at `to` (the role was renamed).
pub fn rename_role(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for mut t in all(&*txn)? {
        if t.role == from {
            t.role = to.to_string();
            put(txn, &t)?;
        }
    }
    Ok(())
}

/// Forgets `table` in the tokens scoped to it (it was dropped): a token
/// keeps its other tables, and one left with none touches no table.
pub fn forget_table(txn: &mut dyn KvTxn, table: &str) -> Result<()> {
    for mut t in all(&*txn)? {
        if let Some(tables) = &mut t.tables
            && tables.iter().any(|n| n == table)
        {
            tables.retain(|n| n != table);
            put(txn, &t)?;
        }
    }
    Ok(())
}

/// Whether the current role may make and revoke tokens for `role`: an
/// administrator; for a role that is not one, the role itself or a member
/// with its admin option — never a token session.
pub fn may_manage(txn: &dyn KvTxn, role: &crate::roles::Role) -> Result<()> {
    let who = access::current();
    if who.token.is_some() {
        return Err(denied("a token session cannot manage tokens"));
    }
    if who.superuser {
        return Ok(());
    }
    if role.superuser {
        return Err(denied("only a superuser may manage a superuser's tokens"));
    }
    if who.user == role.name
        || crate::roles::membership(txn, &role.name, &who.user)?.is_some_and(|m| m.admin_option)
    {
        return Ok(());
    }
    Err(denied(&format!(
        "managing tokens for role \"{}\" needs its admin option",
        role.name
    )))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `CREATE TOKEN`: the token made, and its secret (shown this once).
pub fn create(
    txn: &mut dyn KvTxn,
    name: &str,
    role: Option<&str>,
    read_only: bool,
    tables: Option<&[String]>,
    valid_until: Option<&str>,
    budget: Budgets,
) -> Result<(Token, String)> {
    let who = access::current();
    let role_name = role.unwrap_or(&who.user);
    let role = crate::roles::get(&*txn, role_name)?
        .ok_or_else(|| Error::undefined_object(format!("role \"{role_name}\" does not exist")))?;
    may_manage(&*txn, &role)?;
    if by_name(&*txn, name)?.is_some() {
        return Err(Error::duplicate_object(format!(
            "token \"{name}\" already exists"
        )));
    }
    if let Some(tables) = tables {
        for t in tables {
            crate::catalog::table(&*txn, t)?;
        }
    }
    let valid_until = match valid_until {
        Some(t) if t.eq_ignore_ascii_case("infinity") => None,
        Some(t) => Some(crate::datetime::parse_timestamp(t, true)?),
        None => None,
    };
    let login = loop {
        let mut id = [0u8; 8];
        crate::datetime::random_bytes(&mut id)?;
        let login = format!("{PREFIX}{}", hex(&id));
        if get(&*txn, &login)?.is_none() {
            break login;
        }
    };
    let mut secret = [0u8; 32];
    crate::datetime::random_bytes(&mut secret)?;
    let secret = hex(&secret);
    let mut salt = [0u8; 16];
    crate::datetime::random_bytes(&mut salt)?;
    let token = Token {
        login,
        name: name.to_string(),
        role: role.name,
        read_only,
        tables: tables.map(<[String]>::to_vec),
        valid_until,
        created: crate::datetime::now()?,
        created_by: who.user.clone(),
        verifier: crate::roles::verifier(&secret, &salt),
        budget,
    };
    put(txn, &token)?;
    Ok((token, secret))
}

/// `DROP TOKEN [IF EXISTS] name`.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    let Some(t) = by_name(&*txn, name)? else {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "token \"{name}\" does not exist"
        )));
    };
    if let Some(role) = crate::roles::get(&*txn, &t.role)? {
        may_manage(&*txn, &role)?;
    } else {
        access::require_superuser("dropping a token without a role")?;
    }
    txn.delete(&key(&t.login))?;
    Ok(())
}

/// The tokens the current role may see: all for an administrator, else
/// those it may manage.
pub fn visible(txn: &dyn KvTxn) -> Result<Vec<Token>> {
    let who = access::current();
    let mut out = Vec::new();
    for t in all(txn)? {
        let shown = who.superuser
            || crate::roles::get(txn, &t.role)?.is_some_and(|r| {
                !r.superuser
                    && (who.user == r.name
                        || crate::roles::membership(txn, &r.name, &who.user)
                            .ok()
                            .flatten()
                            .is_some_and(|m| m.admin_option))
            });
        if shown {
            out.push(t);
        }
    }
    Ok(out)
}
