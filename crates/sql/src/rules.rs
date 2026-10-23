//! Access rules (PLAN.md §M14.25): who may connect, from where, and how.
//!
//! `CREATE ACCESS RULE name [FOR role, … | FOR ALL] [FROM 'cidr', …] [VIA
//! front_end, …] [AUTH method, …] [WITH TLS | WITHOUT TLS] {ALLOW [READ
//! ONLY] [AS ROLE r] | REJECT} [BEFORE | AFTER rule]` — ordered rules in the
//! catalog (`00 10 <name>`), the first that matches a login deciding: its
//! user (the role, or a member of it), the client's address, the front end
//! (`ncp`, `pg`), the authentication method (`password`, `token`,
//! `certificate`, `oidc`, `ldap`, `trust`) and TLS.  `REJECT`, or no rule
//! matching, refuses the login (`28000`); `ALLOW` admits it, under the
//! rule's ceiling — `READ ONLY`, `AS ROLE r` (only what `r` may do as well)
//! — which narrows the user's privileges, never widens them.  With no rules
//! at all every login is admitted, as before.
//!
//! Rules bind connections through a front end; a local session (an
//! embedded database, `ncsql <dir>`) is under none — the way back in for
//! an administrator a rule has locked out.

use nucleus_kv::KvTxn;

use crate::access::{self, Context};
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const RULES: [u8; 2] = [0x00, 0x10];
const VERSION: u8 = 1;

/// The authentication methods a rule can name.
pub const METHODS: &[&str] = &["password", "token", "certificate", "oidc", "ldap", "trust"];

/// What a matching rule does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Admits the login, under a ceiling.
    Allow {
        /// No statement may write.
        read_only: bool,
        /// Only what this role may do as well.
        as_role: Option<String>,
    },
    /// Refuses it.
    Reject,
}

/// An access rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// Its name.
    pub name: String,
    /// Its place in the order (the lowest first).
    pub position: u32,
    /// The roles it is for, a member of any of them included (empty: all).
    pub roles: Vec<String>,
    /// The client addresses it is for, as CIDR text (empty: any).
    pub from: Vec<String>,
    /// The front ends (empty: any).
    pub via: Vec<String>,
    /// The authentication methods (empty: any).
    pub auth: Vec<String>,
    /// TLS required (`Some(true)`), refused (`Some(false)`), or either.
    pub tls: Option<bool>,
    /// What it does.
    pub action: Action,
}

fn key(name: &str) -> Vec<u8> {
    let mut k = RULES.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn strings(w: &mut Writer, v: &[String]) {
    w.u32(v.len() as u32);
    for s in v {
        w.str(s);
    }
}

fn read_strings(r: &mut Reader) -> Result<Vec<String>> {
    let n = r.u32()? as usize;
    (0..n).map(|_| r.str()).collect()
}

impl Rule {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION).str(&self.name).u32(self.position);
        strings(&mut w, &self.roles);
        strings(&mut w, &self.from);
        strings(&mut w, &self.via);
        strings(&mut w, &self.auth);
        w.u8(match self.tls {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        });
        match &self.action {
            Action::Reject => {
                w.u8(0);
            }
            Action::Allow { read_only, as_role } => {
                w.u8(1).u8(u8::from(*read_only)).opt_str(as_role.as_deref());
            }
        }
        w.0
    }

    fn decode(b: &[u8]) -> Result<Rule> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown access rule record version"));
        }
        let name = r.str()?;
        let position = r.u32()?;
        let roles = read_strings(&mut r)?;
        let from = read_strings(&mut r)?;
        let via = read_strings(&mut r)?;
        let auth = read_strings(&mut r)?;
        let tls = match r.u8()? {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        };
        let action = if r.u8()? == 0 {
            Action::Reject
        } else {
            Action::Allow {
                read_only: r.u8()? == 1,
                as_role: r.opt_str()?,
            }
        };
        Ok(Rule {
            name,
            position,
            roles,
            from,
            via,
            auth,
            tls,
            action,
        })
    }

    /// Whether it is for `user` logging in in `context`.
    fn matches(
        &self,
        txn: &dyn KvTxn,
        user: &str,
        context: &Context,
        suspended: &std::collections::BTreeSet<(String, String)>,
    ) -> Result<bool> {
        if !self.via.is_empty() && !self.via.contains(&context.front_end) {
            return Ok(false);
        }
        if !self.auth.is_empty() && !self.auth.contains(&context.auth_method) {
            return Ok(false);
        }
        if self.tls.is_some_and(|t| t != context.tls) {
            return Ok(false);
        }
        if !self.from.is_empty() {
            let Some(addr) = context.client_addr else {
                return Ok(false);
            };
            let host = host(addr);
            let mut inside = false;
            for cidr in &self.from {
                inside |= host.within(crate::inet::parse_cidr(cidr)?);
            }
            if !inside {
                return Ok(false);
            }
        }
        if !self.roles.is_empty() {
            let mut any = false;
            for r in &self.roles {
                any |= r == user || crate::roles::is_member_except(txn, user, r, suspended)?;
            }
            if !any {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// An address as a host `inet` (an IPv4 address mapped into IPv6, as a
/// dual-stack listener sees one, as IPv4).
pub fn host(addr: std::net::IpAddr) -> crate::inet::Net {
    let addr = match addr {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        v4 => v4,
    };
    match addr {
        std::net::IpAddr::V4(a) => crate::inet::Net {
            v6: false,
            addr: u128::from(u32::from(a)),
            bits: 32,
        },
        std::net::IpAddr::V6(a) => crate::inet::Net {
            v6: true,
            addr: u128::from(a),
            bits: 128,
        },
    }
}

/// Every rule, in order.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Rule>> {
    let end = nucleus_kv::prefix_end(&RULES).expect("rule prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&RULES[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Rule::decode(&e?.1)?);
    }
    out.sort_by_key(|r| r.position);
    Ok(out)
}

fn put(txn: &mut dyn KvTxn, r: &Rule) -> Result<()> {
    txn.put(&key(&r.name), &r.encode())?;
    Ok(())
}

/// Where a new rule goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// After every rule.
    Last,
    /// Just before this one.
    Before(String),
    /// Just after this one.
    After(String),
}

/// `CREATE ACCESS RULE`.
pub fn create(txn: &mut dyn KvTxn, mut rule: Rule, place: &Place) -> Result<()> {
    access::require_superuser("CREATE ACCESS RULE")?;
    let rules = all(&*txn)?;
    if rules.iter().any(|r| r.name == rule.name) {
        return Err(Error::duplicate_object(format!(
            "access rule \"{}\" already exists",
            rule.name
        )));
    }
    for cidr in &rule.from {
        crate::inet::parse_cidr(cidr)?;
    }
    for m in &rule.auth {
        if !METHODS.contains(&m.as_str()) {
            return Err(Error::invalid_parameter(format!(
                "unknown authentication method \"{m}\" (there are: {})",
                METHODS.join(", ")
            )));
        }
    }
    let mut roles = rule.roles.clone();
    if let Action::Allow {
        as_role: Some(r), ..
    } = &rule.action
    {
        roles.push(r.clone());
    }
    for r in &roles {
        if crate::roles::get(&*txn, r)?.is_none() {
            return Err(Error::undefined_object(format!(
                "role \"{r}\" does not exist"
            )));
        }
    }
    let at = |name: &str| {
        rules
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.position)
            .ok_or_else(|| {
                Error::undefined_object(format!("access rule \"{name}\" does not exist"))
            })
    };
    // Positions are renumbered 1, 2, … with the new rule in its place.
    let index = match place {
        Place::Last => rules.len(),
        Place::Before(n) => {
            let p = at(n)?;
            rules.iter().position(|r| r.position == p).expect("found")
        }
        Place::After(n) => {
            let p = at(n)?;
            rules.iter().position(|r| r.position == p).expect("found") + 1
        }
    };
    let mut ordered = rules;
    ordered.insert(index, rule.clone());
    for (i, mut r) in ordered.into_iter().enumerate() {
        r.position = i as u32 + 1;
        if r.name == rule.name {
            rule.position = r.position;
        }
        put(txn, &r)?;
    }
    Ok(())
}

/// `DROP ACCESS RULE [IF EXISTS] name`.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    access::require_superuser("DROP ACCESS RULE")?;
    if txn.get(&key(name))?.is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "access rule \"{name}\" does not exist"
        )));
    }
    txn.delete(&key(name))?;
    Ok(())
}

/// Takes `role` out of the rules naming it (it was dropped); a rule left
/// for no role, or whose ceiling role went, is dropped — it would
/// otherwise be for everyone.
pub fn forget_role(txn: &mut dyn KvTxn, role: &str) -> Result<()> {
    for mut r in all(&*txn)? {
        let named = r.roles.iter().any(|x| x == role);
        let ceiling = matches!(&r.action, Action::Allow { as_role: Some(x), .. } if x == role);
        if !named && !ceiling {
            continue;
        }
        r.roles.retain(|x| x != role);
        if ceiling || r.roles.is_empty() {
            txn.delete(&key(&r.name))?;
        } else {
            put(txn, &r)?;
        }
    }
    Ok(())
}

/// Renames `from` to `to` in the rules.
pub fn rename_role(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for mut r in all(&*txn)? {
        let mut changed = false;
        for x in r.roles.iter_mut() {
            if x == from {
                *x = to.to_string();
                changed = true;
            }
        }
        if let Action::Allow {
            as_role: Some(x), ..
        } = &mut r.action
            && x == from
        {
            *x = to.to_string();
            changed = true;
        }
        if changed {
            put(txn, &r)?;
        }
    }
    Ok(())
}

/// The rule that admits `user` logging in in `context` (`None`: there are
/// no rules), or `28000`.
pub fn admit(
    txn: &dyn KvTxn,
    user: &str,
    context: &Context,
    suspended: &std::collections::BTreeSet<(String, String)>,
) -> Result<Option<Rule>> {
    let rules = all(txn)?;
    if rules.is_empty() {
        return Ok(None);
    }
    for r in rules {
        if r.matches(txn, user, context, suspended)? {
            return match r.action {
                Action::Reject => Err(Error::invalid_authorization(format!(
                    "access rule \"{}\" rejects the login of \"{user}\"",
                    r.name
                ))),
                Action::Allow { .. } => Ok(Some(r)),
            };
        }
    }
    Err(Error::invalid_authorization(format!(
        "no access rule admits the login of \"{user}\" from {} through {} by {}{}",
        context
            .client_addr
            .map_or("an unknown address".to_string(), |a| a.to_string()),
        context.front_end,
        context.auth_method,
        if context.tls { " over TLS" } else { "" }
    )))
}
