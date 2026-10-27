//! The database's settings (PLAN.md §M14.25): `ALTER DATABASE nucleus SET
//! name = value | RESET name`, kept in the catalog (`00 11 <name>`), shown
//! in `nc_catalog.database_settings`.  Each is checked when set, and has a
//! default.

use nucleus_kv::KvTxn;

use crate::error::{Error, Result};

const SETTINGS: [u8; 2] = [0x00, 0x11];

/// A setting the database knows.
pub struct Known {
    /// Its name.
    pub name: &'static str,
    /// Its value when not set.
    pub default: &'static str,
    /// What it does.
    pub description: &'static str,
    check: fn(&str) -> Result<String>,
}

fn whole(v: &str) -> Result<String> {
    v.trim()
        .parse::<u32>()
        .map(|n| n.to_string())
        .map_err(|_| Error::invalid_parameter(format!("\"{v}\" is not a whole number")))
}

fn positive(v: &str) -> Result<String> {
    match whole(v)?.as_str() {
        "0" => Err(Error::invalid_parameter("it must be at least 1")),
        n => Ok(n.to_string()),
    }
}

fn none_or_all(v: &str) -> Result<String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "none" => Ok("none".into()),
        "all" => Ok("all".into()),
        _ => Err(Error::invalid_parameter("it is none or all")),
    }
}

fn list(v: &str) -> Result<String> {
    Ok(v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(","))
}

fn memory_text(v: &str) -> Result<String> {
    memory(v)?;
    Ok(v.trim().trim_matches('\'').to_string())
}

/// A memory size: a number of kB (as PostgreSQL's `work_mem`), or with a
/// unit (`kB`, `MB`, `GB`, `TB`; `B`).  At least 64 kB.
pub fn memory(v: &str) -> Result<usize> {
    let v = v.trim().trim_matches('\'');
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = (&v[..split], v[split..].trim());
    let n: u64 = n
        .parse()
        .map_err(|_| Error::invalid_parameter(format!("invalid memory size: \"{v}\"")))?;
    let scale: u64 = match unit {
        "" | "kB" => 1 << 10,
        "B" => 1,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => {
            return Err(Error::invalid_parameter(format!(
                "invalid memory size: \"{v}\" (valid units are B, kB, MB, GB and TB)"
            )));
        }
    };
    let bytes = n.saturating_mul(scale);
    if bytes < 64 << 10 {
        return Err(Error::invalid_parameter(format!(
            "{v} is below the minimum of 64kB"
        )));
    }
    Ok(usize::try_from(bytes).unwrap_or(usize::MAX))
}

fn duration_text(v: &str) -> Result<String> {
    duration(v)?;
    Ok(v.trim().to_string())
}

fn lifetime(v: &str) -> Result<String> {
    if v.trim().eq_ignore_ascii_case("infinity") {
        return Ok("infinity".into());
    }
    duration_text(v)
}

/// The settings, and their defaults.
pub const KNOWN: &[Known] = &[
    Known {
        name: "work_mem",
        default: "64MB",
        description: "the memory each sort, hash table and materialised result of a statement holds before the rest goes to scratch space",
        check: memory_text,
    },
    Known {
        name: "password_min_length",
        default: "1",
        description: "the fewest characters a password set by CREATE / ALTER ROLE may have",
        check: positive,
    },
    Known {
        name: "password_lifetime",
        default: "infinity",
        description: "how long a password set without VALID UNTIL works (an interval)",
        check: lifetime,
    },
    Known {
        name: "statement_timeout",
        default: "0",
        description: "the longest a statement may run (0: no limit); SET statement_timeout overrides",
        check: duration_text,
    },
    Known {
        name: "audit_statements",
        default: "none",
        description: "all: every statement is recorded in the audit log; none: only logins and DDL",
        check: none_or_all,
    },
    Known {
        name: "audit_roles",
        default: "",
        description: "the statements of these roles (and their members) are recorded, a comma-separated list",
        check: list,
    },
    Known {
        name: "audit_tables",
        default: "",
        description: "the statements that touch these tables are recorded, a comma-separated list",
        check: list,
    },
    Known {
        name: "login_lock_failures",
        default: "10",
        description: "failed logins in a row after which a role is locked for login_lock_time",
        check: positive,
    },
    Known {
        name: "login_lock_time",
        default: "15 minutes",
        description: "how long a role stays locked after too many failed logins",
        check: duration_text,
    },
];

fn known(name: &str) -> Result<&'static Known> {
    KNOWN.iter().find(|k| k.name == name).ok_or_else(|| {
        Error::undefined_object(format!(
            "unrecognized database setting \"{name}\" (there are: {})",
            KNOWN.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")
        ))
    })
}

fn key(name: &str) -> Vec<u8> {
    let mut k = SETTINGS.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

/// A setting's value (its default when not set).
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<String> {
    let k = known(name)?;
    Ok(match txn.get(&key(name))? {
        Some(v) => String::from_utf8_lossy(&v).into_owned(),
        None => k.default.to_string(),
    })
}

/// Every setting: name, value, whether set, default, description.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<(&'static Known, String, bool)>> {
    KNOWN
        .iter()
        .map(|k| {
            let set = txn.get(&key(k.name))?;
            Ok((
                k,
                set.as_ref().map_or(k.default.to_string(), |v| {
                    String::from_utf8_lossy(v).into_owned()
                }),
                set.is_some(),
            ))
        })
        .collect()
}

/// `ALTER DATABASE … SET name = value` (`None`: `RESET`).
pub fn set(txn: &mut dyn KvTxn, database: &str, name: &str, value: Option<&str>) -> Result<()> {
    crate::access::require_superuser("ALTER DATABASE … SET")?;
    if database != crate::privileges::DATABASE {
        return Err(Error::undefined_database(format!(
            "database \"{database}\" does not exist"
        )));
    }
    let k = known(name)?;
    match value {
        Some(v) => {
            let v = (k.check)(v).map_err(|e| {
                Error::invalid_parameter(format!("invalid value for \"{name}\": {}", e.message))
            })?;
            txn.put(&key(name), v.as_bytes())?;
        }
        None => txn.delete(&key(name))?,
    }
    Ok(())
}

/// A duration: whole milliseconds (`5000`), a number and a unit (`5s`,
/// `100ms`, `2min`, `1h`, `1d`), or an interval (`'5 seconds'`); in µs.
pub fn duration(v: &str) -> Result<i64> {
    let v = v.trim().trim_matches('\'');
    if let Ok(ms) = v.parse::<i64>() {
        return if ms < 0 {
            Err(Error::invalid_parameter("a duration cannot be negative"))
        } else {
            Ok(ms * 1000)
        };
    }
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (n, unit) = (&v[..split], v[split..].trim());
    if let Ok(n) = n.parse::<i64>() {
        let scale = match unit {
            "us" => Some(1),
            "ms" => Some(1_000),
            "s" => Some(1_000_000),
            "min" => Some(60_000_000),
            "h" => Some(3_600_000_000),
            "d" => Some(86_400_000_000),
            _ => None,
        };
        if let Some(s) = scale {
            return Ok(n * s);
        }
    }
    let iv = crate::datetime::parse_interval(v)?;
    let us = (i64::from(iv.months) * 30 + i64::from(iv.days)) * 86_400_000_000 + iv.micros;
    if us < 0 {
        return Err(Error::invalid_parameter("a duration cannot be negative"));
    }
    Ok(us)
}
