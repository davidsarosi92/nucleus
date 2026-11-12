//! Secrets (PLAN.md §M14.22): credentials the database uses on its own
//! behalf — an embedding provider's API key — kept so that nothing shows
//! them.
//!
//! `CREATE SECRET name VALUE '…'` seals the value with ChaCha20-Poly1305
//! (RFC 8439) under the *keyring's* key, which lives outside the data
//! directory ([`Keyring`]: the server's `--secrets-key` file, or what an
//! application embedding nucleus gives), so a copied directory or a backup
//! does not reveal a secret; the record holds the key's id, a random
//! nonce and the sealed value, the secret's name authenticated beside it.
//! `CREATE SECRET name FROM ENV 'VAR'` names an environment variable of
//! the server's process instead, read when the secret is used — only an
//! administrator may make one, since the process's environment is theirs.
//!
//! A secret's value never reaches `nc_catalog` (`nc_catalog.secrets`
//! shows its name, owner, where it comes from and when it was made), the
//! statement statistics or the audit log (statements carrying one are
//! recorded with their strings masked, [`crate::statements::redacted`]),
//! nor an error.  Using one — naming it in a model's options (§M14.22
//! E2b) — needs `USAGE` on it; its owner (its maker) and administrators
//! have that, others when granted (`GRANT USAGE ON SECRET k TO r`).
//!
//! Records are in the catalog, `00 14 <name>`.

use std::sync::{Arc, RwLock};

use nucleus_crypto::aead;
use nucleus_kv::KvTxn;

use crate::access;
use crate::encoding::{Reader, Writer};
use crate::error::{Error, Result};

const SECRETS: [u8; 2] = [0x00, 0x14];
const VERSION: u8 = 1;

/// Where secret values are sealed: a key kept apart from the data.
pub trait Keyring: Send + Sync {
    /// The key; with `create`, made (and kept) if there is none yet.
    /// `Ok(None)`: there is none, and none was made.
    fn key(&self, create: bool) -> std::result::Result<Option<[u8; aead::KEY_LEN]>, String>;

    /// Where the key is, for messages (never the key).
    fn describe(&self) -> String;
}

/// A key given as it is (tests; an application holding its own key).
pub struct StaticKey(pub [u8; aead::KEY_LEN]);

impl Keyring for StaticKey {
    fn key(&self, _create: bool) -> std::result::Result<Option<[u8; aead::KEY_LEN]>, String> {
        Ok(Some(self.0))
    }

    fn describe(&self) -> String {
        "a key given by the application".into()
    }
}

static KEYRING: RwLock<Option<Arc<dyn Keyring>>> = RwLock::new(None);

/// Sets the process's keyring (`None`: none — secrets with a value can be
/// neither made nor read).
pub fn set_keyring(k: Option<Arc<dyn Keyring>>) {
    *KEYRING.write().unwrap_or_else(|e| e.into_inner()) = k;
}

fn keyring() -> Option<Arc<dyn Keyring>> {
    KEYRING.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn no_key() -> Error {
    Error::prerequisite("there is no key for secrets").with_hint(
        "start the server with --secrets-key FILE, or give an embedded database a keyring \
         (nucleus_sql::secrets::set_keyring); a secret FROM ENV needs none",
    )
}

fn key(create: bool) -> Result<[u8; aead::KEY_LEN]> {
    let ring = keyring().ok_or_else(no_key)?;
    match ring.key(create) {
        Ok(Some(k)) => Ok(k),
        Ok(None) => Err(no_key()),
        Err(e) => Err(Error::io(format!(
            "the key for secrets ({}): {e}",
            ring.describe()
        ))),
    }
}

/// A key's id: which key sealed a value, without revealing it.
fn key_id(key: &[u8; aead::KEY_LEN]) -> [u8; 8] {
    let mut m = b"nucleus secrets key id\0".to_vec();
    m.extend_from_slice(key);
    let h = nucleus_crypto::sha256(&m);
    h[..8].try_into().expect("8 bytes")
}

fn aad(name: &str) -> Vec<u8> {
    let mut a = b"nucleus secret\0".to_vec();
    a.extend_from_slice(name.as_bytes());
    a
}

/// Where a secret's value comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    /// Sealed in the record.
    Sealed {
        /// The id of the key that sealed it.
        key_id: [u8; 8],
        /// The nonce.
        nonce: [u8; aead::NONCE_LEN],
        /// The ciphertext and its tag.
        sealed: Vec<u8>,
    },
    /// The server process's environment variable of this name.
    Env(String),
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Sealed { .. } => f.write_str("Sealed(..)"),
            Source::Env(v) => write!(f, "Env({v:?})"),
        }
    }
}

/// A secret, as the catalog keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Secret {
    /// Its name.
    pub name: String,
    /// The role that made it.
    pub owner: String,
    /// When it was made (µs since 2000-01-01 UTC).
    pub created: i64,
    /// When its value last changed.
    pub changed: i64,
    /// Where its value comes from.
    pub source: Source,
}

fn key_of(name: &str) -> Vec<u8> {
    let mut k = SECRETS.to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

impl Secret {
    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(VERSION)
            .str(&self.name)
            .str(&self.owner)
            .u64(self.created as u64)
            .u64(self.changed as u64);
        match &self.source {
            Source::Sealed {
                key_id,
                nonce,
                sealed,
            } => {
                w.u8(1);
                w.0.extend_from_slice(key_id);
                w.0.extend_from_slice(nonce);
                w.u32(sealed.len() as u32);
                w.0.extend_from_slice(sealed);
            }
            Source::Env(var) => {
                w.u8(2).str(var);
            }
        }
        w.0
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(Error::internal("unknown secret record version"));
        }
        let name = r.str()?;
        let owner = r.str()?;
        let created = r.u64()? as i64;
        let changed = r.u64()? as i64;
        let source = match r.u8()? {
            1 => {
                let key_id = r.take(8)?.try_into().expect("8 bytes");
                let nonce = r.take(aead::NONCE_LEN)?.try_into().expect("12 bytes");
                let n = r.u32()? as usize;
                Source::Sealed {
                    key_id,
                    nonce,
                    sealed: r.take(n)?.to_vec(),
                }
            }
            2 => Source::Env(r.str()?),
            _ => return Err(Error::internal("unknown secret source")),
        };
        Ok(Secret {
            name,
            owner,
            created,
            changed,
            source,
        })
    }

    /// `value` or `env`.
    pub fn kind(&self) -> &'static str {
        match self.source {
            Source::Sealed { .. } => "value",
            Source::Env(_) => "env",
        }
    }
}

/// The secret called `name`.
pub fn get(txn: &dyn KvTxn, name: &str) -> Result<Option<Secret>> {
    txn.get(&key_of(name))?
        .map(|b| Secret::decode(&b))
        .transpose()
}

/// Every secret, by name.
pub fn all(txn: &dyn KvTxn) -> Result<Vec<Secret>> {
    let end = nucleus_kv::prefix_end(&SECRETS).expect("secret prefix");
    let mut out = Vec::new();
    for e in txn.scan(
        std::ops::Bound::Included(&SECRETS[..]),
        std::ops::Bound::Excluded(&end[..]),
    )? {
        out.push(Secret::decode(&e?.1)?);
    }
    Ok(out)
}

fn put(txn: &mut dyn KvTxn, s: &Secret) -> Result<()> {
    txn.put(&key_of(&s.name), &s.encode())?;
    Ok(())
}

/// What a `CREATE SECRET` or `ALTER SECRET` gives.
#[derive(Clone, PartialEq, Eq)]
pub enum Given {
    /// `VALUE '…'`.
    Value(String),
    /// `FROM ENV 'VAR'`.
    Env(String),
}

impl std::fmt::Debug for Given {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Given::Value(_) => f.write_str("Value(..)"),
            Given::Env(v) => write!(f, "Env({v:?})"),
        }
    }
}

fn source_of(name: &str, given: &Given) -> Result<Source> {
    match given {
        Given::Value(v) => {
            let key = key(true)?;
            let mut nonce = [0u8; aead::NONCE_LEN];
            crate::datetime::random_bytes(&mut nonce)?;
            Ok(Source::Sealed {
                key_id: key_id(&key),
                nonce,
                sealed: aead::seal(&key, &nonce, &aad(name), v.as_bytes()),
            })
        }
        Given::Env(var) => {
            if !access::current().superuser {
                return Err(Error::insufficient_privilege(
                    "only an administrator may make a secret FROM ENV",
                ));
            }
            if var.is_empty() || var.contains(['=', '\0']) {
                return Err(Error::invalid_parameter(format!(
                    "\"{var}\" is not an environment variable's name"
                )));
            }
            Ok(Source::Env(var.clone()))
        }
    }
}

/// `CREATE SECRET [IF NOT EXISTS] name VALUE '…' | FROM ENV 'VAR'`.
pub fn create(txn: &mut dyn KvTxn, name: &str, given: &Given, if_not_exists: bool) -> Result<()> {
    if get(&*txn, name)?.is_some() {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::duplicate_object(format!(
            "secret \"{name}\" already exists"
        )));
    }
    let now = crate::datetime::now()?;
    let s = Secret {
        name: name.to_string(),
        owner: access::current().user.clone(),
        created: now,
        changed: now,
        source: source_of(name, given)?,
    };
    put(txn, &s)
}

fn existing(txn: &dyn KvTxn, name: &str) -> Result<Secret> {
    get(txn, name)?
        .ok_or_else(|| Error::undefined_object(format!("secret \"{name}\" does not exist")))
}

/// `ALTER SECRET name VALUE '…' | FROM ENV 'VAR'`: a new value (the old
/// one is gone with the record it was sealed in).
pub fn alter(txn: &mut dyn KvTxn, name: &str, given: &Given) -> Result<()> {
    let mut s = existing(&*txn, name)?;
    s.source = source_of(name, given)?;
    s.changed = crate::datetime::now()?;
    put(txn, &s)
}

/// `DROP SECRET [IF EXISTS] name`; its grants go with it.
pub fn drop(txn: &mut dyn KvTxn, name: &str, if_exists: bool) -> Result<()> {
    if get(&*txn, name)?.is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::undefined_object(format!(
            "secret \"{name}\" does not exist"
        )));
    }
    let users = crate::models::using_secret(&*txn, name)?;
    if !users.is_empty() {
        return Err(Error::dependent_objects(format!(
            "secret \"{name}\" is used by model {}",
            users.join(", ")
        )));
    }
    txn.delete(&key_of(name))?;
    crate::privileges::forget(txn, crate::privileges::Kind::Secret, name)
}

/// A role renamed: the secrets it owns follow.
pub fn rename_owner(txn: &mut dyn KvTxn, from: &str, to: &str) -> Result<()> {
    for mut s in all(&*txn)? {
        if s.owner == from {
            s.owner = to.to_string();
            put(txn, &s)?;
        }
    }
    Ok(())
}

/// A secret's value, for the code that uses it on the database's behalf
/// (a model provider) — never for a client.  The caller checks `USAGE`
/// where the secret is named (a model's definition).
pub fn reveal(txn: &dyn KvTxn, name: &str) -> Result<Revealed> {
    let s = existing(txn, name)?;
    match &s.source {
        Source::Sealed {
            key_id: id,
            nonce,
            sealed,
        } => {
            let key = key(false)?;
            if key_id(&key) != *id {
                return Err(Error::prerequisite(format!(
                    "secret \"{name}\" was sealed with another key than this server's"
                ))
                .with_hint("start the server with the --secrets-key it was made with, or set the secret again"));
            }
            let v = aead::open(&key, nonce, &aad(name), sealed).ok_or_else(|| {
                Error::data_exception(format!("secret \"{name}\" does not open: its record is damaged"))
            })?;
            String::from_utf8(v)
                .map(Revealed)
                .map_err(|_| Error::data_exception(format!("secret \"{name}\" is not UTF-8")))
        }
        Source::Env(var) => std::env::var(var).map(Revealed).map_err(|_| {
            Error::prerequisite(format!(
                "secret \"{name}\": the environment variable {var} is not set in the server's process"
            ))
        }),
    }
}

/// A secret's value: printed as `***`, so a log or a debug print of
/// what holds it shows nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct Revealed(String);

impl Revealed {
    /// The value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Revealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl std::fmt::Display for Revealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let s = Secret {
            name: "k".into(),
            owner: "alice".into(),
            created: 5,
            changed: 7,
            source: Source::Sealed {
                key_id: [1; 8],
                nonce: [2; 12],
                sealed: vec![3; 40],
            },
        };
        assert_eq!(Secret::decode(&s.encode()).unwrap(), s);
        let e = Secret {
            source: Source::Env("OPENAI_API_KEY".into()),
            ..s
        };
        assert_eq!(Secret::decode(&e.encode()).unwrap(), e);
    }

    #[test]
    fn values_never_print() {
        let r = Revealed("sk-very-secret".into());
        assert_eq!(format!("{r} {r:?}"), "*** ***");
        assert!(!format!("{:?}", Given::Value("sk-very-secret".into())).contains("sk-"));
    }
}
