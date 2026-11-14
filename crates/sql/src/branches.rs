//! Branches of the database as SQL sees them (PLAN.md §M14.13 B3):
//! `CREATE BRANCH`, `DROP BRANCH`, `ALTER BRANCH … RESET` and
//! `nc_catalog.branches`.  A branch is a database of its own (a session
//! connects to one by its name); the storage below keeps it as a tree
//! sharing every page it has not changed.

use std::sync::Arc;

use nucleus_kv::{Branch, Kv, MAIN_BRANCH};

use crate::error::{Error, Result};

/// The branch named `name`, or `3D000`.
fn find(all: &[Branch], name: &str) -> Result<Branch> {
    all.iter()
        .find(|b| b.name == name)
        .cloned()
        .ok_or_else(|| Error::undefined_database(format!("branch \"{name}\" does not exist")))
}

/// `CREATE BRANCH name [FROM from]`, from the session's database `kv`.
pub fn create(kv: &Arc<dyn Kv>, name: &str, from: Option<&str>) -> Result<()> {
    // `nucleus` names the main database to a client, as `main` does.
    if !nucleus_kv::valid_branch_name(name) || name == "nucleus" {
        return Err(Error::invalid_name(format!(
            "\"{name}\" cannot name a branch: 1 to 63 letters, digits, '-', '_' or '.', \
             and not \"{MAIN_BRANCH}\" or \"nucleus\""
        )));
    }
    let all = kv.branches()?;
    if all.iter().any(|b| b.name == name) {
        return Err(Error::duplicate_database(format!(
            "branch \"{name}\" already exists"
        )));
    }
    let base = match from {
        Some(f) if f != kv.branch_name() => {
            find(&all, f)?;
            kv.branch(f)?
        }
        _ => Arc::clone(kv),
    };
    base.create_branch(name)?;
    Ok(())
}

/// Checks that `name`, a branch to drop or reset, may be: not the main
/// one, not the session's own, without branches of its own.
fn changeable(kv: &Arc<dyn Kv>, all: &[Branch], name: &str, what: &str) -> Result<()> {
    find(all, name)?;
    if name == MAIN_BRANCH {
        return Err(Error::feature(format!(
            "the main branch \"{MAIN_BRANCH}\" cannot be {what}"
        )));
    }
    if name == kv.branch_name() {
        return Err(Error::object_in_use(format!(
            "branch \"{name}\" is the one this session is connected to"
        )));
    }
    if let Some(child) = all.iter().find(|b| b.parent.as_deref() == Some(name)) {
        return Err(Error::dependent_objects(format!(
            "branch \"{name}\" has a branch of its own, \"{}\"",
            child.name
        )));
    }
    Ok(())
}

/// `DROP BRANCH [IF EXISTS] name`; whether there was one.
pub fn drop(kv: &Arc<dyn Kv>, name: &str, if_exists: bool) -> Result<bool> {
    let all = kv.branches()?;
    if if_exists && !all.iter().any(|b| b.name == name) {
        return Ok(false);
    }
    changeable(kv, &all, name, "dropped")?;
    kv.drop_branch(name)?;
    Ok(true)
}

/// `ALTER BRANCH name RESET`.
pub fn reset(kv: &Arc<dyn Kv>, name: &str) -> Result<()> {
    let all = kv.branches()?;
    changeable(kv, &all, name, "reset")?;
    kv.reset_branch(name)?;
    Ok(())
}

/// `nc_catalog.branches`' rows: name, parent, whether it is the
/// session's.
pub fn rows(kv: &dyn Kv) -> Result<Vec<(String, Option<String>, bool)>> {
    let here = kv.branch_name();
    Ok(kv
        .branches()?
        .into_iter()
        .map(|b| {
            let current = b.name == here;
            (b.name, b.parent, current)
        })
        .collect())
}
