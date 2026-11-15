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
