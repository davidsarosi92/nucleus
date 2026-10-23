//! Users and roles (PLAN.md §M14.25), in the catalog: `CREATE ROLE`,
//! `CREATE USER` (a role that may log in), `ALTER`, `DROP`, membership
//! with inheritance (`GRANT role TO role`), an administrator attribute
//! (`SUPERUSER`).
//!
//! | Key | Value |
//! |-----|-------|
//! | `00 0A <role>` | a role: its attributes and SCRAM-SHA-256 verifier |
//! | `00 0B <role> 00 <member>` | a membership: whether it carries the admin option |
//!
//! **Bootstrap.**  A database whose catalog has no superuser that can log
//! in with a password is *unclaimed*: the server's configured credentials
//! act as an administrator, so a first one can be made (`nucleus init`, or
//! `CREATE USER … SUPERUSER PASSWORD …`).  From then on the catalog
//! decides who logs in.

use std::collections::BTreeSet;
use std::ops::Bound;

use nucleus_kv::KvTxn;

use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const ROLES: [u8; 2] = [0x00, 0x0A];
const MEMBERS: [u8; 2] = [0x00, 0x0B];
const VERSION: u8 = 1;

/// A role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    /// Its name.
    pub name: String,
    /// An administrator: every privilege, and the role attributes.
    pub superuser: bool,
    /// May log in (a user).
    pub login: bool,
    /// Has the privileges of the roles it is a member of without `SET
    /// ROLE`.
    pub inherit: bool,
    /// May create, alter and drop roles other than administrators.
    pub createrole: bool,
    /// The most sessions it may have at once (`None`: no limit).
    pub connection_limit: Option<u32>,
    /// Its password's SCRAM-SHA-256 verifier (`None`: no password).
    pub password: Option<String>,
    /// When its password stops working (µs since 2000-01-01 UTC).
    pub valid_until: Option<i64>,
}

impl Role {
    /// A role with the defaults: no attributes, inheriting.
    pub fn new(name: &str) -> Role {
        Role {
            name: name.to_string(),
            superuser: false,
            login: false,
            inherit: true,
            createrole: false,
            connection_limit: None,
            password: None,
            valid_until: None,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        let flags = u8::from(self.superuser)
            | u8::from(self.login) << 1
            | u8::from(self.inherit) << 2
            | u8::from(self.createrole) << 3;
        w.u8(VERSION)
            .str(&self.name)
            .u8(flags)
            .u32(self.connection_limit.map_or(0, |n| n + 1))
            .opt_str(self.password.as_deref());
        match self.valid_until {
            Some(t) => w.u8(1).u64(t as u64),
            None => w.u8(0),
        };
        w.0
    }

    fn decode(b: &[u8]) -> Result<Role> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown role record version"));
        }
        let name = r.str()?;
        let flags = r.u8()?;
        let connection_limit = match r.u32()? {
            0 => None,
            n => Some(n - 1),
        };
        let password = r.opt_str()?;
        let valid_until = match r.u8()? {
            1 => Some(r.u64()? as i64),
            _ => None,
        };
        Ok(Role {
            name,
            superuser: flags & 1 != 0,
            login: flags & 2 != 0,
            inherit: flags & 4 != 0,
            createrole: flags & 8 != 0,
            connection_limit,
            password,
            valid_until,
        })
    }
}

fn role_key(name: &str) -> Vec<u8> {
    let mut k = ROLES.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn member_key(role: &str, member: &str) -> Vec<u8> {
    let mut k = MEMBERS.to_vec();
    k.extend_from_slice(role.as_bytes());
    k.push(0);
    k.extend_from_slice(member.as_bytes());
    k
}

fn scan(txn: &dyn KvTxn, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let end = nucleus_kv::prefix_end(prefix).expect("a catalog prefix");
    txn.scan(Bound::Included(prefix), Bound::Excluded(&end))?
        .map(|e| e.map_err(Into::into))
        .collect()
}

/// The role called `name`.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<Role>> {
    txn.get(&role_key(name))?
        .map(|b| Role::decode(&b))
        .transpose()
}

/// Every role, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Role>> {
    scan(txn, &ROLES)?
        .iter()
        .map(|(_, v)| Role::decode(v))
        .collect()
}

/// Stores `role` (new or changed).
pub fn put(txn: &mut dyn KvTxn, role: &Role) -> Result<()> {
    txn.put(&role_key(&role.name), &role.encode())?;
    Ok(())
}

/// A membership: `member` is a member of `role`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    /// The role.
    pub role: String,
    /// Its member.
    pub member: String,
    /// The member may grant `role` to others, and revoke it.
    pub admin_option: bool,
    /// `WHEN (…)`: the membership holds only in a session whose connection
    /// meets it (SQL over `source`, `auth`, `front_end`, `tls`).
    pub condition: Option<String>,
}

fn condition_of(v: &[u8]) -> Option<String> {
    (v.len() > 1).then(|| String::from_utf8_lossy(&v[1..]).into_owned())
}

/// Every membership.
pub fn memberships(txn: &dyn KvTxn) -> Result<Vec<Membership>> {
    let mut out = Vec::new();
    for (k, v) in scan(txn, &MEMBERS)? {
        let rest = &k[MEMBERS.len()..];
        let at = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::internal("malformed membership key"))?;
        out.push(Membership {
            role: String::from_utf8_lossy(&rest[..at]).into_owned(),
            member: String::from_utf8_lossy(&rest[at + 1..]).into_owned(),
            admin_option: v.first() == Some(&1),
            condition: condition_of(&v),
        });
    }
    Ok(out)
}

/// The membership of `member` in `role`, if any.
pub fn membership(txn: &dyn KvTxn, role: &str, member: &str) -> Result<Option<Membership>> {
    Ok(txn.get(&member_key(role, member))?.map(|v| Membership {
        role: role.to_string(),
        member: member.to_string(),
        admin_option: v.first() == Some(&1),
        condition: condition_of(&v),
    }))
}

/// Makes `member` a member of `role` (or changes its admin option).
pub fn add_member(txn: &mut dyn KvTxn, role: &str, member: &str, admin_option: bool) -> Result<()> {
    put_member(txn, role, member, admin_option, None)
}

/// The same, holding only under `condition` (`None`: always).
pub fn put_member(
    txn: &mut dyn KvTxn,
    role: &str,
    member: &str,
    admin_option: bool,
    condition: Option<&str>,
) -> Result<()> {
    let mut v = vec![u8::from(admin_option)];
    v.extend_from_slice(condition.unwrap_or_default().as_bytes());
    txn.put(&member_key(role, member), &v)?;
    Ok(())
}

/// Ends `member`'s membership of `role`.
pub fn remove_member(txn: &mut dyn KvTxn, role: &str, member: &str) -> Result<()> {
    txn.delete(&member_key(role, member))?;
    Ok(())
}

/// The roles `user` has the privileges of: itself, and — through members
/// that inherit — the roles it belongs to, directly or not.
pub fn effective(txn: &dyn KvTxn, user: &str) -> Result<BTreeSet<String>> {
    effective_except(txn, user, &BTreeSet::new())
}

/// The same, without the memberships `(role, member)` in `suspended`.
pub fn effective_except(
    txn: &dyn KvTxn,
    user: &str,
    suspended: &BTreeSet<(String, String)>,
) -> Result<BTreeSet<String>> {
    let all: Vec<Membership> = memberships(txn)?
        .into_iter()
        .filter(|m| !suspended.contains(&(m.role.clone(), m.member.clone())))
        .collect();
    let mut out = BTreeSet::from([user.to_string()]);
    let mut todo = vec![user.to_string()];
    while let Some(r) = todo.pop() {
        let inherits = get(txn, &r)?.is_none_or(|role| role.inherit);
        if !inherits {
            continue;
        }
        for m in all.iter().filter(|m| m.member == r) {
            if out.insert(m.role.clone()) {
                todo.push(m.role.clone());
            }
        }
    }
    Ok(out)
}

/// The roles whose privileges `who` has: its role's, and while it acts as
/// itself, those its login granted for the session.
pub fn effective_for(txn: &dyn KvTxn, who: &access::Who) -> Result<BTreeSet<String>> {
    let mut out = effective_except(txn, &who.user, &who.suspended)?;
    if who.user == who.session_user {
        for r in &who.session_roles {
            out.extend(effective_except(txn, r, &who.suspended)?);
        }
    }
    Ok(out)
}

/// Whether `member` is in `role`, directly or not, inheriting or not (for
/// `SET ROLE` and cycle checks).
pub fn is_member(txn: &dyn KvTxn, member: &str, role: &str) -> Result<bool> {
    is_member_except(txn, member, role, &BTreeSet::new())
}

/// The same, without the memberships `(role, member)` in `suspended`.
pub fn is_member_except(
    txn: &dyn KvTxn,
    member: &str,
    role: &str,
    suspended: &BTreeSet<(String, String)>,
) -> Result<bool> {
    if member == role {
        return Ok(true);
    }
    let all: Vec<Membership> = memberships(txn)?
        .into_iter()
        .filter(|m| !suspended.contains(&(m.role.clone(), m.member.clone())))
        .collect();
    let mut seen = BTreeSet::from([member.to_string()]);
    let mut todo = vec![member.to_string()];
    while let Some(r) = todo.pop() {
        for m in all.iter().filter(|m| m.member == r) {
            if m.role == role {
                return Ok(true);
            }
            if seen.insert(m.role.clone()) {
                todo.push(m.role.clone());
            }
        }
    }
    Ok(false)
}

/// Drops role `name` and its memberships, both ways.
pub fn drop(txn: &mut dyn KvTxn, name: &str) -> Result<()> {
    for m in memberships(&*txn)? {
        if m.role == name || m.member == name {
            remove_member(txn, &m.role, &m.member)?;
        }
    }
    txn.delete(&role_key(name))?;
    Ok(())
}

/// Whether the database is unclaimed: no superuser that can log in with a
/// password (see the module documentation).
pub fn unclaimed(txn: &dyn KvTxn) -> Result<bool> {
    Ok(!all(txn)?
        .iter()
        .any(|r| r.superuser && r.login && r.password.is_some()))
}

/// Why a login was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No such role, it may not log in, or the password is wrong (not told
    /// apart, so a client cannot probe for names).
    BadCredentials,
    /// The password has expired.
    Expired,
}

/// Checks a login against the catalog: the role exists, may log in, has a
/// password that `password` matches and that has not expired (`now`: µs
/// since 2000-01-01 UTC).
pub fn authenticate(
    txn: &dyn KvTxn,
    user: &str,
    password: &str,
    now: i64,
) -> Result<std::result::Result<Role, Refusal>> {
    let Some(role) = get(txn, user)? else {
        return Ok(Err(Refusal::BadCredentials));
    };
    let verifier = role
        .password
        .as_deref()
        .and_then(nucleus_crypto::ScramVerifier::parse);
    let good = role.login && verifier.is_some_and(|v| v.matches(password));
    Ok(if !good {
        Err(Refusal::BadCredentials)
    } else if role.valid_until.is_some_and(|t| t < now) {
        Err(Refusal::Expired)
    } else {
        Ok(role)
    })
}

/// A password's verifier, salted with `salt` (16 random bytes); a value
/// that already is a verifier (`SCRAM-SHA-256$…`) is kept, as PostgreSQL
/// does, so dumps restore.
pub fn verifier(password: &str, salt: &[u8]) -> String {
    if nucleus_crypto::ScramVerifier::parse(password).is_some() {
        return password.to_string();
    }
    nucleus_crypto::ScramVerifier::new(password, salt, nucleus_crypto::SCRAM_ITERATIONS).to_text()
}

// ------------------------------------------------------------- statements

use crate::access::{self, denied};
use crate::ast::{RoleOption, Statement};

/// What the current role may do to roles: everything (an administrator),
/// or non-administrator roles (`CREATEROLE`).
fn may_manage(txn: &dyn KvTxn, target_superuser: bool) -> Result<()> {
    let who = access::current();
    if who.superuser {
        return Ok(());
    }
    let createrole = get(txn, &who.user)?.is_some_and(|r| r.createrole);
    if createrole && !target_superuser {
        Ok(())
    } else if createrole {
        Err(denied(
            "only a superuser may create, alter or drop a superuser",
        ))
    } else {
        Err(denied("managing roles needs CREATEROLE or a superuser"))
    }
}

fn apply(txn: &mut dyn KvTxn, role: &mut Role, options: &[RoleOption]) -> Result<()> {
    // A credential made in a token session would outlive the token.
    if access::current().token.is_some()
        && options.iter().any(|o| matches!(o, RoleOption::Password(_)))
    {
        return Err(denied("a token session cannot set passwords"));
    }
    for o in options {
        match o {
            RoleOption::Superuser(b) => role.superuser = *b,
            RoleOption::Login(b) => role.login = *b,
            RoleOption::Inherit(b) => role.inherit = *b,
            RoleOption::CreateRole(b) => role.createrole = *b,
            RoleOption::ConnectionLimit(n) => role.connection_limit = *n,
            RoleOption::Password(None) => role.password = None,
            RoleOption::Password(Some(p)) => {
                if p.is_empty() {
                    return Err(Error::invalid_parameter("a password must not be empty"));
                }
                // A verifier given as such (a dump) is not checked again.
                if nucleus_crypto::ScramVerifier::parse(p).is_none() {
                    let min: usize = crate::dbsettings::get(&*txn, "password_min_length")?
                        .parse()
                        .unwrap_or(1);
                    if p.chars().count() < min {
                        return Err(Error::invalid_parameter(format!(
                            "a password must have at least {min} characters (password_min_length)"
                        )));
                    }
                    if p.eq_ignore_ascii_case(&role.name) {
                        return Err(Error::invalid_parameter(
                            "a password must not be the role's name",
                        ));
                    }
                }
                // Without VALID UNTIL, the database's password lifetime.
                let lifetime = crate::dbsettings::get(&*txn, "password_lifetime")?;
                if lifetime != "infinity"
                    && !options
                        .iter()
                        .any(|o| matches!(o, RoleOption::ValidUntil(_)))
                {
                    let us = crate::dbsettings::duration(&lifetime)?;
                    role.valid_until = Some(crate::datetime::now()? + us);
                }
                let mut salt = [0u8; 16];
                crate::datetime::random_bytes(&mut salt)?;
                role.password = Some(verifier(p, &salt));
            }
            RoleOption::ValidUntil(t) => {
                role.valid_until = if t.eq_ignore_ascii_case("infinity") {
                    None
                } else {
                    Some(crate::datetime::parse_timestamp(t, true)?)
                };
            }
            RoleOption::InRole(roles) => {
                for r in roles {
                    if get(&*txn, r)?.is_none() {
                        return Err(no_role(r));
                    }
                    add_member(txn, r, &role.name, false)?;
                }
            }
        }
    }
    Ok(())
}

fn no_role(name: &str) -> Error {
    Error::undefined_object(format!("role \"{name}\" does not exist"))
}

/// Names the catalog keeps for itself.
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "public" | "none" | "current_user" | "session_user" | "current_role"
    ) || name.starts_with("pg_")
        || name.starts_with("nc_")
        || name.starts_with(crate::tokens::PREFIX)
}

/// Runs a role statement; returns its command tag.
pub fn run(txn: &mut dyn KvTxn, stmt: &Statement) -> Result<String> {
    match stmt {
        Statement::CreateRole { name, options } => {
            let superuser = options.contains(&RoleOption::Superuser(true));
            may_manage(&*txn, superuser)?;
            if reserved(name) {
                return Err(Error::invalid_parameter(format!(
                    "role name \"{name}\" is reserved"
                )));
            }
            if get(&*txn, name)?.is_some() {
                return Err(Error::duplicate_object(format!(
                    "role \"{name}\" already exists"
                )));
            }
            let mut role = Role::new(name);
            apply(txn, &mut role, options)?;
            put(txn, &role)?;
            // A role made by a non-administrator is administered by its
            // maker (as PostgreSQL 16 does).
            let who = access::current();
            if !who.superuser {
                add_member(txn, name, &who.user, true)?;
            }
            Ok("CREATE ROLE".into())
        }
        Statement::AlterRole { name, options } => {
            let mut role = get(&*txn, name)?.ok_or_else(|| no_role(name))?;
            let who = access::current();
            let own_password =
                who.user == *name && options.iter().all(|o| matches!(o, RoleOption::Password(_)));
            if !own_password {
                let to_superuser = options.contains(&RoleOption::Superuser(true));
                may_manage(&*txn, role.superuser || to_superuser)?;
            }
            apply(txn, &mut role, options)?;
            put(txn, &role)?;
            Ok("ALTER ROLE".into())
        }
        Statement::RenameRole { name, to } => {
            let mut role = get(&*txn, name)?.ok_or_else(|| no_role(name))?;
            may_manage(&*txn, role.superuser)?;
            let who = access::current();
            if who.session_user == *name || who.user == *name {
                return Err(Error::feature("the session's own role cannot be renamed"));
            }
            if get(&*txn, to)?.is_some() {
                return Err(Error::duplicate_object(format!(
                    "role \"{to}\" already exists"
                )));
            }
            if reserved(to) {
                return Err(Error::invalid_parameter(format!(
                    "role name \"{to}\" is reserved"
                )));
            }
            let members = memberships(&*txn)?;
            drop(txn, name)?;
            role.name = to.clone();
            put(txn, &role)?;
            for m in members {
                let rename = |n: &String| if n == name { to.clone() } else { n.clone() };
                if m.role == *name || m.member == *name {
                    add_member(txn, &rename(&m.role), &rename(&m.member), m.admin_option)?;
                }
            }
            for mut t in crate::catalog::tables(&*txn)? {
                if t.owner.as_deref() == Some(name.as_str()) {
                    t.owner = Some(to.clone());
                    crate::catalog::update_table(txn, &t)?;
                }
            }
            crate::enums::rename_owner(txn, name, to)?;
            crate::privileges::rename_grantee(txn, name, to)?;
            crate::tokens::rename_role(txn, name, to)?;
            crate::rules::rename_role(txn, name, to)?;
            Ok("ALTER ROLE".into())
        }
        Statement::DropRole { names, if_exists } => {
            let who = access::current();
            for name in names {
                let Some(role) = get(&*txn, name)? else {
                    if *if_exists {
                        continue;
                    }
                    return Err(no_role(name));
                };
                may_manage(&*txn, role.superuser)?;
                if who.session_user == *name || who.user == *name {
                    return Err(Error::object_in_use(format!(
                        "current user cannot be dropped: \"{name}\""
                    )));
                }
                let mut owned: Vec<String> = crate::catalog::tables(&*txn)?
                    .into_iter()
                    .filter(|t| t.owner.as_deref() == Some(name.as_str()))
                    .map(|t| format!("table {}", t.name))
                    .collect();
                owned.extend(
                    crate::enums::all(&*txn)?
                        .into_iter()
                        .filter(|e| e.owner.as_deref() == Some(name.as_str()))
                        .map(|e| format!("type {}", e.name)),
                );
                owned.extend(crate::privileges::held_by(&*txn, name)?);
                if !owned.is_empty() {
                    return Err(Error::dependent_objects(format!(
                        "role \"{name}\" cannot be dropped because it owns {}",
                        owned.join(", ")
                    )));
                }
                crate::tokens::forget_role(txn, name)?;
                crate::rules::forget_role(txn, name)?;
                drop(txn, name)?;
            }
            Ok("DROP ROLE".into())
        }
        Statement::GrantRole {
            roles,
            members,
            admin_option,
            condition,
        } => {
            for r in roles {
                let role = get(&*txn, r)?.ok_or_else(|| no_role(r))?;
                may_grant(&*txn, &role)?;
                for m in members {
                    if get(&*txn, m)?.is_none() {
                        return Err(no_role(m));
                    }
                    if is_member(&*txn, r, m)? {
                        return Err(Error::invalid_grant(format!(
                            "role \"{m}\" is a member of role \"{r}\""
                        )));
                    }
                    let keep_admin = membership(&*txn, r, m)?.is_some_and(|x| x.admin_option);
                    // The statement says the membership's condition: none
                    // without WHEN.
                    put_member(txn, r, m, *admin_option || keep_admin, condition.as_deref())?;
                }
            }
            Ok("GRANT ROLE".into())
        }
        Statement::RevokeRole {
            roles,
            members,
            admin_option_only,
        } => {
            for r in roles {
                let role = get(&*txn, r)?.ok_or_else(|| no_role(r))?;
                may_grant(&*txn, &role)?;
                for m in members {
                    if membership(&*txn, r, m)?.is_none() {
                        continue; // PostgreSQL warns; nothing to do
                    }
                    if *admin_option_only {
                        add_member(txn, r, m, false)?;
                    } else {
                        remove_member(txn, r, m)?;
                    }
                }
            }
            Ok("REVOKE ROLE".into())
        }
        _ => Err(Error::internal("not a role statement")),
    }
}

/// Whether the current role may grant `role`: an administrator, a role with
/// `CREATEROLE` (for a non-administrator role), or a member with the admin
/// option.
fn may_grant(txn: &dyn KvTxn, role: &Role) -> Result<()> {
    let who = access::current();
    if who.superuser {
        return Ok(());
    }
    if membership(txn, &role.name, &who.user)?.is_some_and(|m| m.admin_option) {
        return Ok(());
    }
    if !role.superuser && get(txn, &who.user)?.is_some_and(|r| r.createrole) {
        return Ok(());
    }
    Err(denied(&format!(
        "granting role \"{}\" needs its admin option",
        role.name
    )))
}

/// The identity a session that logged in as `user` has, or `28000` if
/// the database does not know it (an unclaimed one admits any name as its
/// administrator).
pub fn login_identity(txn: &dyn KvTxn, user: &str) -> Result<access::Who> {
    if user.starts_with(crate::tokens::PREFIX) {
        // A token: its role, under its ceiling.
        let t = crate::tokens::get(txn, user)?.ok_or_else(|| {
            Error::invalid_authorization(format!("token \"{user}\" does not exist"))
        })?;
        let r = get(txn, &t.role)?.ok_or_else(|| no_role(&t.role))?;
        return Ok(access::Who {
            session_user: r.name.clone(),
            user: r.name,
            superuser: r.superuser,
            settings: Default::default(),
            ceiling: t.ceiling(),
            token: Some(t.name),
            session_roles: Default::default(),
            claims: Default::default(),
            context: Default::default(),
            rule: None,
            suspended: Default::default(),
        });
    }
    match get(txn, user)? {
        Some(r) => Ok(access::Who {
            session_user: user.into(),
            user: user.into(),
            superuser: r.superuser,
            settings: Default::default(),
            token: None,
            ceiling: Default::default(),
            session_roles: Default::default(),
            claims: Default::default(),
            context: Default::default(),
            rule: None,
            suspended: Default::default(),
        }),
        None if unclaimed(txn)? => Ok(access::Who {
            session_user: user.into(),
            user: user.into(),
            superuser: true,
            settings: Default::default(),
            token: None,
            ceiling: Default::default(),
            session_roles: Default::default(),
            claims: Default::default(),
            context: Default::default(),
            rule: None,
            suspended: Default::default(),
        }),
        None => Err(Error::invalid_authorization(format!(
            "role \"{user}\" does not exist"
        ))),
    }
}

/// The identity after `SET ROLE role` for a session that logged in as
/// `login` (`None`: back to it).
pub fn set_role(txn: &dyn KvTxn, login: &access::Who, role: Option<&str>) -> Result<access::Who> {
    let Some(role) = role else {
        return Ok(login.clone());
    };
    let r = get(txn, role)?.ok_or_else(|| no_role(role))?;
    let through_session = login
        .session_roles
        .iter()
        .map(|r| Ok(r == role || is_member_except(txn, r, role, &login.suspended)?))
        .collect::<Result<Vec<bool>>>()?
        .contains(&true);
    if !login.superuser
        && !through_session
        && !is_member_except(txn, &login.session_user, role, &login.suspended)?
    {
        return Err(denied(&format!(
            "role \"{}\" is not a member of role \"{role}\"",
            login.session_user
        )));
    }
    Ok(access::Who {
        session_user: login.session_user.clone(),
        user: role.into(),
        superuser: r.superuser,
        settings: login.settings.clone(),
        token: login.token.clone(),
        ceiling: login.ceiling.clone(),
        session_roles: login.session_roles.clone(),
        claims: login.claims.clone(),
        context: login.context.clone(),
        rule: login.rule.clone(),
        suspended: login.suspended.clone(),
    })
}
