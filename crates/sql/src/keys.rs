//! The key hierarchy as the SQL layer sees it (PLAN.md §M14.30 K1): one
//! root key, held by whoever has its custody, wrapping the data keys —
//! the secrets', the backups', the subject keys' — which are stored only
//! wrapped.  The custody itself (the root's file, variable, passphrase or
//! KMS, and the keyring file of wrapped keys) is the edition's: a server
//! installs it with [`set_custody`]; this module holds it for the parts
//! that need a key, and `nc_catalog.keys` lists it — ids, kinds, where,
//! how old; never a key.

use std::sync::{Arc, RwLock};

pub use nucleus_crypto::keys::{Key, KeyProvider};

/// A key of the hierarchy, as `nc_catalog.keys` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyInfo {
    /// `root`, or the purpose of a data key (`secrets`, `backup`, …).
    pub name: String,
    /// `root` or `data`.
    pub role: String,
    /// The key's identifier (safe to show).
    pub id: String,
    /// Its custody: `database`, `user` or `kms:…` for the root; the root
    /// that wraps it for a data key.
    pub custody: String,
    /// Where the root is (a file, a variable, a KMS key) — never the key.
    pub location: String,
    /// When it was made (microseconds since 1970), when known.
    pub created: Option<i64>,
}

/// The custody of the root and the data keys it wraps.
pub trait Custody: Send + Sync {
    /// The data key for `purpose`; with `create`, made (and kept, wrapped)
    /// when there is none — the root too, when its custody allows.
    fn dek(&self, purpose: &str, create: bool) -> Result<Option<Key>, String>;

    /// The root, for wrapping a key into something that leaves the
    /// database (a backup's manifest); with `create`, made when its
    /// custody allows and there is none.
    fn root(&self, create: bool) -> Result<Option<Arc<dyn KeyProvider>>, String>;

    /// Whether backups are encrypted without being asked (the owner's
    /// setting; by default when the root was given explicitly).
    fn encrypt_backups(&self) -> bool;

    /// The keys, for `nc_catalog.keys`.
    fn list(&self) -> Result<Vec<KeyInfo>, String>;

    /// Begins a new data key for `purpose` (PLAN.md §M14.30 K3f), the one
    /// there kept as the previous until [`retire`](Self::retire); the new
    /// key.  An error when a rotation of it has not ended, or there is no
    /// key for `purpose`.  The default: not supported.
    fn rotate(&self, purpose: &str) -> Result<Key, String> {
        Err(format!(
            "this key custody cannot rotate the key \"{purpose}\""
        ))
    }

    /// Whether a rotation of `purpose`'s key has not ended.  The default:
    /// no.
    fn rotating(&self, purpose: &str) -> Result<bool, String> {
        let _ = purpose;
        Ok(false)
    }

    /// Ends a rotation of `purpose`'s key: what the previous key sealed
    /// is all sealed again, and it is dropped.  The default: nothing to
    /// do.
    fn retire(&self, purpose: &str) -> Result<(), String> {
        let _ = purpose;
        Ok(())
    }
}

/// `ROTATE KEY data` (PLAN.md §M14.30 K3f): a new key for the database's
/// files, the database open — the custody's new data key given to the
/// database at once, so everything written from now on is sealed under
/// it, while what the old key sealed is sealed again in the background
/// (the server's maintenance), after which the old key is dropped.  The
/// other data keys are rotated with the server stopped (`nucleus keys
/// <dir> rotate secrets|subjects|backup`).
pub fn rotate(kv: &Arc<dyn nucleus_kv::Kv>, purpose: &str) -> Result<(), crate::Error> {
    if purpose != "data" {
        return Err(crate::Error::feature(format!(
            "ROTATE KEY {purpose}: only the data key rotates online; the server stopped, \
             `nucleus keys <dir> rotate {purpose}` rotates it"
        )));
    }
    let c = custody().ok_or_else(|| {
        crate::Error::feature("ROTATE KEY: this database has no key custody installed")
    })?;
    let running = |m: &str| crate::Error::object_in_use(format!("ROTATE KEY data: {m}"));
    if kv.rekey_state() != nucleus_kv::Rekey::Idle || c.rotating("data").map_err(io)? {
        return Err(running(
            "the rotation begun before is still running (its pages are being sealed again)",
        ));
    }
    if c.dek("data", false).map_err(io)?.is_none() {
        return Err(crate::Error::feature(
            "ROTATE KEY data: the database is not encrypted (`nucleus keys <dir> encrypt`)",
        ));
    }
    let key = c.rotate("data").map_err(io)?;
    kv.rekey(nucleus_kv::PageCipher::new(*key.bytes()))?;
    Ok(())
}

fn io(m: String) -> crate::Error {
    crate::Error::io(m)
}

static CUSTODY: RwLock<Option<Arc<dyn Custody>>> = RwLock::new(None);

/// Installs the custody of this process's database (`None` removes it).
pub fn set_custody(c: Option<Arc<dyn Custody>>) {
    *CUSTODY.write().unwrap_or_else(|e| e.into_inner()) = c;
}

/// The custody installed, if any.
pub fn custody() -> Option<Arc<dyn Custody>> {
    CUSTODY.read().unwrap_or_else(|e| e.into_inner()).clone()
}
