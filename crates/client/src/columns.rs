//! Client-side encrypted columns (PLAN.md §M14.30 K5, DOCS.md §8.40): a
//! value the driver encrypts before it is sent and decrypts after it
//! arrives, so the server stores ciphertext and never holds the key.
//! Equality is queryable through a blind index — a keyed hash of the
//! value, stored beside it — and nothing else is.
//!
//! One format for every driver (format 1, `drivers/encrypted-columns.json`
//! its test vectors):
//!
//! - **Keys**: from a 32-byte master key the application keeps and a
//!   column's name it chooses (`"users.email"`): the encryption key
//!   `HMAC-SHA-256(master, "nucleus column 1 encrypt\0" ‖ name)`, the
//!   index key `HMAC-SHA-256(master, "nucleus column 1 index\0" ‖ name)`.
//! - **A value**: a type byte — 0 bytes, 1 text (UTF-8), 2 an integer
//!   (8 bytes, little-endian, two's complement) — and its bytes.
//! - **Ciphertext** (`bytea`): the byte 1, a 24-byte random nonce, and
//!   XChaCha20-Poly1305 of the value under the encryption key (the byte 1
//!   authenticated beside it): 41 bytes more than the value.
//! - **Blind index** (`bytea`): the first 16 bytes of
//!   `HMAC-SHA-256(index key, value)`.
//!
//! What it leaves visible: a value's length (to the byte), and through
//! the blind index which rows hold equal values and how often each
//! occurs — a column of few distinct values (a flag, a country) gives
//! them away by their frequencies; give such a column no index.

use nucleus_crypto::aead::{XNONCE_LEN, xopen, xseal};
use nucleus_crypto::hmac_sha256;

use crate::{Error, Value};

/// The format's version: the first byte of a ciphertext.
pub const FORMAT: u8 = 1;
/// A blind index's length.
pub const INDEX_LEN: usize = 16;

/// A value an encrypted column holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plain {
    /// Bytes as they are (type byte 0).
    Bytes(Vec<u8>),
    /// A text, UTF-8 (type byte 1).
    Text(String),
    /// An integer, 8 bytes little-endian (type byte 2).
    Int(i64),
}

impl Plain {
    /// Its type byte and bytes.
    fn encode(&self) -> Vec<u8> {
        match self {
            Plain::Bytes(b) => [&[0u8][..], b].concat(),
            Plain::Text(s) => [&[1u8][..], s.as_bytes()].concat(),
            Plain::Int(i) => [&[2u8][..], &i.to_le_bytes()].concat(),
        }
    }

    fn decode(b: &[u8]) -> Result<Plain, Error> {
        match b.split_first() {
            Some((0, rest)) => Ok(Plain::Bytes(rest.to_vec())),
            Some((1, rest)) => String::from_utf8(rest.to_vec())
                .map(Plain::Text)
                .map_err(|_| Error::Value("an encrypted text is not UTF-8".into())),
            Some((2, rest)) => rest
                .try_into()
                .map(|b| Plain::Int(i64::from_le_bytes(b)))
                .map_err(|_| Error::Value("an encrypted integer is not 8 bytes".into())),
            _ => Err(Error::Value("an encrypted value of no type known".into())),
        }
    }
}

impl From<&str> for Plain {
    fn from(s: &str) -> Self {
        Plain::Text(s.into())
    }
}

impl From<String> for Plain {
    fn from(s: String) -> Self {
        Plain::Text(s)
    }
}

impl From<i64> for Plain {
    fn from(i: i64) -> Self {
        Plain::Int(i)
    }
}

impl From<&[u8]> for Plain {
    fn from(b: &[u8]) -> Self {
        Plain::Bytes(b.to_vec())
    }
}

impl From<Vec<u8>> for Plain {
    fn from(b: Vec<u8>) -> Self {
        Plain::Bytes(b)
    }
}

/// A column's keys: encrypts its values, decrypts them, and makes their
/// blind index.
///
/// ```
/// use nucleus_client::columns::{ColumnKey, Plain};
///
/// let key = ColumnKey::new(&[7; 32], "users.email");
/// let sealed = key.encrypt("ann@example.com"); // a Value::Bytes to send
/// assert_eq!(key.decrypt_value(&sealed).unwrap(), Some(Plain::from("ann@example.com")));
/// assert_eq!(key.blind_index_bytes("ann@example.com").len(), 16);
/// ```
#[derive(Clone)]
pub struct ColumnKey {
    encrypt: [u8; 32],
    index: [u8; 32],
}

impl std::fmt::Debug for ColumnKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ColumnKey(..)")
    }
}

impl ColumnKey {
    /// The keys of column `name` under `master` (32 bytes the application
    /// keeps — in a KMS, a keychain; never in the database).
    pub fn new(master: &[u8; 32], name: &str) -> Self {
        let derive = |purpose: &str| {
            hmac_sha256(
                master,
                &[
                    format!("nucleus column 1 {purpose}\0").as_bytes(),
                    name.as_bytes(),
                ]
                .concat(),
            )
        };
        ColumnKey {
            encrypt: derive("encrypt"),
            index: derive("index"),
        }
    }

    /// `value` encrypted, as the `bytea` to store: a fresh random nonce
    /// each time, so equal values make different ciphertexts.
    pub fn encrypt(&self, value: impl Into<Plain>) -> Value {
        Value::Bytes(self.encrypt_with_nonce(value, &random_nonce()))
    }

    /// [`Self::encrypt`] with a nonce of one's own — for test vectors: a
    /// nonce used twice under one key gives both values away.
    pub fn encrypt_with_nonce(&self, value: impl Into<Plain>, nonce: &[u8; XNONCE_LEN]) -> Vec<u8> {
        let mut out = vec![FORMAT];
        out.extend_from_slice(nonce);
        out.extend(xseal(
            &self.encrypt,
            nonce,
            &[FORMAT],
            &value.into().encode(),
        ));
        out
    }

    /// The value in `sealed` (what [`Self::encrypt`] made); an error when
    /// it was not made under this key, or was changed.
    pub fn decrypt(&self, sealed: &[u8]) -> Result<Plain, Error> {
        let fail = || Error::Value("an encrypted value this column's key does not open".into());
        if sealed.len() < 1 + XNONCE_LEN || sealed[0] != FORMAT {
            return Err(fail());
        }
        let nonce: [u8; XNONCE_LEN] = sealed[1..1 + XNONCE_LEN].try_into().map_err(|_| fail())?;
        let plain =
            xopen(&self.encrypt, &nonce, &[FORMAT], &sealed[1 + XNONCE_LEN..]).ok_or_else(fail)?;
        Plain::decode(&plain)
    }

    /// [`Self::decrypt`] of a column's value as it arrived: `NULL` stays
    /// `None`.
    pub fn decrypt_value(&self, v: &Value) -> Result<Option<Plain>, Error> {
        match v {
            Value::Null => Ok(None),
            Value::Bytes(b) => self.decrypt(b).map(Some),
            other => Err(Error::Value(format!(
                "an encrypted column holds bytea, not {other}"
            ))),
        }
    }

    /// `value`'s blind index: equal values have equal ones, so `WHERE
    /// email_index = $1` with this as `$1` finds a value's rows.
    pub fn blind_index(&self, value: impl Into<Plain>) -> Value {
        Value::Bytes(self.blind_index_bytes(value))
    }

    /// [`Self::blind_index`]'s bytes.
    pub fn blind_index_bytes(&self, value: impl Into<Plain>) -> Vec<u8> {
        hmac_sha256(&self.index, &value.into().encode())[..INDEX_LEN].to_vec()
    }
}

/// 24 bytes from the operating system's random source; where there is
/// none to read, unique ones — a process-wide counter and the time
/// hashed under the process's random hasher keys (XChaCha20's nonce
/// needs to be unique, not secret).
fn random_nonce() -> [u8; XNONCE_LEN] {
    let mut n = [0u8; XNONCE_LEN];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut n))
            .is_ok()
        {
            return n;
        }
    }
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut seed = Vec::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        seed.extend_from_slice(&h.finish().to_le_bytes());
    }
    seed.extend_from_slice(&count.to_le_bytes());
    seed.extend_from_slice(&time.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    n.copy_from_slice(&nucleus_crypto::sha256(&seed)[..XNONCE_LEN]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses() {
        let k = ColumnKey::new(&[7; 32], "users.email");
        for v in [
            Plain::from("ann@example.com"),
            Plain::Text(String::new()),
            Plain::Int(-42),
            Plain::Bytes(vec![0, 255]),
        ] {
            let Value::Bytes(a) = k.encrypt(v.clone()) else {
                unreachable!()
            };
            let Value::Bytes(b) = k.encrypt(v.clone()) else {
                unreachable!()
            };
            assert_ne!(a, b, "a fresh nonce each time");
            assert_eq!(a.len(), 41 + v.encode().len());
            assert_eq!(k.decrypt(&a).unwrap(), v);
            assert_eq!(k.blind_index_bytes(v.clone()), k.blind_index_bytes(v));
        }
        // The text "1" and the integer 1 are different values.
        assert_ne!(k.blind_index_bytes("1"), k.blind_index_bytes(1));
        // Another column's key, or a byte changed: refused.
        let other = ColumnKey::new(&[7; 32], "users.phone");
        let Value::Bytes(mut s) = k.encrypt("x") else {
            unreachable!()
        };
        assert!(other.decrypt(&s).is_err());
        assert_ne!(other.blind_index_bytes("x"), k.blind_index_bytes("x"));
        s[30] ^= 1;
        assert!(k.decrypt(&s).is_err());
        assert!(k.decrypt(&[]).is_err());
    }
}
