//! Keys and their wrapping (PLAN.md §M14.30 K1): a root key (a KEK)
//! wraps data keys (DEKs) — envelope encryption — so one key kept outside
//! the database guards all the others, which are stored only wrapped.
//!
//! A wrapped key is deterministic authenticated encryption, as the key
//! wrap of RFC 3394 and SIV's synthetic IV (RFC 5297): the nonce is a
//! PRF of the purpose and the key wrapped, under a MAC key derived from
//! the root; ChaCha20-Poly1305 then seals the key under an encryption key
//! derived apart, with the purpose as associated data; unwrapping opens
//! it and checks the nonce again.  No randomness is needed, a nonce is
//! never used for two different keys, and the same key wrapped twice
//! gives the same bytes (harmless: nothing but the key is in them).
//!
//! The form: `1` (the version), the nonce (12 bytes), the key sealed (32
//! bytes and the 16-byte tag) — 61 bytes.

use crate::aead;
use crate::{constant_time_eq, hmac_sha256, pbkdf2_sha256, sha256};

/// A key's length.
pub const KEY_LEN: usize = 32;
/// A wrapped key's length.
pub const WRAPPED_LEN: usize = 1 + aead::NONCE_LEN + KEY_LEN + aead::TAG_LEN;
/// PBKDF2's iterations for a root derived from a passphrase (OWASP's
/// figure for PBKDF2-HMAC-SHA-256, 2023).
pub const PASSPHRASE_ITERATIONS: u32 = 600_000;

const WRAP_VERSION: u8 = 1;

/// A 256-bit key: its bytes overwritten with zeros when it is dropped, and
/// never printed.
///
/// The zeros are written, then passed to [`std::hint::black_box`] so the
/// write is not optimized away — a best effort without `unsafe` (copies a
/// move left behind are not reached, and the memory is not locked
/// against swapping).
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// The key of these bytes.
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The key's bytes.
    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The key written as 64 hex digits (for the files a key is kept in).
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// The key from 64 hex digits (surrounding whitespace ignored).
    pub fn from_hex(text: &str) -> Option<Self> {
        let b = from_hex(text.trim())?;
        Some(Self(b.try_into().ok()?))
    }

    /// An identifier for the key, safe to show: the first 8 bytes of
    /// SHA-256 of `label`, a zero byte and the key, in hex.  The label
    /// keeps the ids of a key in two roles apart.
    pub fn id(&self, label: &str) -> String {
        let mut b = label.as_bytes().to_vec();
        b.push(0);
        b.extend_from_slice(&self.0);
        let h = sha256(&b);
        b.fill(0);
        std::hint::black_box(&b);
        to_hex(&h[..8])
    }
}

impl Clone for Key {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(…)")
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(&self.0, &other.0)
    }
}

impl Eq for Key {}

/// Who holds the root key, and wraps and unwraps data keys with it: the
/// three custody models differ only here (PLAN.md §M14.30 K1).  A file
/// or a passphrase's key is a [`LocalRoot`]; a KMS is a module whose root
/// never leaves it.
pub trait KeyProvider: Send + Sync {
    /// The root's identifier, safe to show and to record beside what it
    /// wrapped (a wrapped key names the root that opens it).
    fn id(&self) -> String;

    /// The custody: `database` (a file beside the data), `user` (a file,
    /// an environment variable or a passphrase the user gives) or `kms:…`.
    fn kind(&self) -> String;

    /// Where the root is, for messages — never the key.
    fn describe(&self) -> String;

    /// `dek` wrapped for `purpose` (the purpose is bound in: a key wrapped
    /// for one does not unwrap for another).
    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String>;

    /// The key `blob` wrapped for `purpose`; an error when this root did not
    /// wrap it, or for another purpose, or a byte of it changed.
    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String>;
}

/// A root key held in this process: read from a file or an environment
/// variable, or derived from a passphrase.
pub struct LocalRoot {
    key: Key,
    kind: String,
    describe: String,
}

impl LocalRoot {
    /// A root of `kind` (`database`, `user`), `describe` saying where it
    /// came from.
    pub fn new(key: Key, kind: &str, describe: &str) -> Self {
        Self {
            key,
            kind: kind.to_string(),
            describe: describe.to_string(),
        }
    }
}

/// The identifier label of root keys.
pub const ROOT_ID_LABEL: &str = "nucleus root key";

impl KeyProvider for LocalRoot {
    fn id(&self) -> String {
        self.key.id(ROOT_ID_LABEL)
    }

    fn kind(&self) -> String {
        self.kind.clone()
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        Ok(wrap(&self.key, purpose, dek))
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        unwrap(&self.key, purpose, blob).ok_or_else(|| {
            format!(
                "a key for {purpose} does not open with the root key {} ({})",
                self.id(),
                self.describe
            )
        })
    }
}

/// The two keys a root wraps with: the nonces' MAC key and the encryption
/// key, derived apart.
fn wrap_keys(root: &Key) -> (Key, Key) {
    (
        Key(hmac_sha256(root.bytes(), b"nucleus key wrap: nonce")),
        Key(hmac_sha256(root.bytes(), b"nucleus key wrap: seal")),
    )
}

fn wrap_nonce(mac: &Key, purpose: &str, dek: &Key) -> [u8; aead::NONCE_LEN] {
    let mut m = purpose.as_bytes().to_vec();
    m.push(0);
    m.extend_from_slice(dek.bytes());
    let h = hmac_sha256(mac.bytes(), &m);
    m.fill(0);
    std::hint::black_box(&m);
    h[..aead::NONCE_LEN].try_into().expect("12 bytes")
}

/// `dek` wrapped under `root` for `purpose` ([`WRAPPED_LEN`] bytes).
pub fn wrap(root: &Key, purpose: &str, dek: &Key) -> Vec<u8> {
    let (mac, enc) = wrap_keys(root);
    let nonce = wrap_nonce(&mac, purpose, dek);
    let mut out = Vec::with_capacity(WRAPPED_LEN);
    out.push(WRAP_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&aead::seal(
        enc.bytes(),
        &nonce,
        purpose.as_bytes(),
        dek.bytes(),
    ));
    out
}

/// The key [`wrap`] wrapped under `root` for `purpose`; `None` when the
/// root, the purpose or a byte differs.
pub fn unwrap(root: &Key, purpose: &str, blob: &[u8]) -> Option<Key> {
    if blob.len() != WRAPPED_LEN || blob[0] != WRAP_VERSION {
        return None;
    }
    let nonce: [u8; aead::NONCE_LEN] = blob[1..1 + aead::NONCE_LEN].try_into().ok()?;
    let (mac, enc) = wrap_keys(root);
    let mut plain = aead::open(
        enc.bytes(),
        &nonce,
        purpose.as_bytes(),
        &blob[1 + aead::NONCE_LEN..],
    )?;
    let key = Key(plain.as_slice().try_into().ok()?);
    plain.fill(0);
    std::hint::black_box(&plain);
    // The synthetic nonce checked: the key and the purpose are the ones
    // it was made from.
    constant_time_eq(&wrap_nonce(&mac, purpose, &key), &nonce).then_some(key)
}

/// A root key derived from a passphrase: PBKDF2-HMAC-SHA-256 over it and
/// `salt` (random, kept beside the wrapped keys), `iterations` rounds.
pub fn root_from_passphrase(passphrase: &[u8], salt: &[u8], iterations: u32) -> Key {
    Key(pbkdf2_sha256(passphrase, salt, iterations))
}

/// Bytes as lower-case hex.
pub fn to_hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[usize::from(x >> 4)] as char);
        s.push(D[usize::from(x & 15)] as char);
    }
    s
}

/// Bytes from hex (either case); `None` for an odd length or a character
/// that is not a hex digit.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| {
            let d = |c: u8| (c as char).to_digit(16);
            Some((d(p[0])? * 16 + d(p[1])?) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Key {
        Key::new([b; KEY_LEN])
    }

    #[test]
    fn wrapped_keys_open_with_their_root_and_purpose_only() {
        let (root, dek) = (key(1), key(2));
        let w = wrap(&root, "secrets", &dek);
        assert_eq!(w.len(), WRAPPED_LEN);
        assert_eq!(unwrap(&root, "secrets", &w), Some(dek.clone()));
        // Deterministic.
        assert_eq!(wrap(&root, "secrets", &dek), w);
        // Another root, another purpose: refused.
        assert_eq!(unwrap(&key(3), "secrets", &w), None);
        assert_eq!(unwrap(&root, "backup", &w), None);
        // Every byte flipped: refused.
        for i in 0..w.len() {
            let mut t = w.clone();
            t[i] ^= 0x40;
            assert_eq!(unwrap(&root, "secrets", &t), None, "byte {i}");
        }
        // Cut short, or longer: refused.
        assert_eq!(unwrap(&root, "secrets", &w[..w.len() - 1]), None);
        let mut long = w.clone();
        long.push(0);
        assert_eq!(unwrap(&root, "secrets", &long), None);
        // Two keys, two nonces.
        let w2 = wrap(&root, "secrets", &key(4));
        assert_ne!(w[1..13], w2[1..13]);
    }

    #[test]
    fn a_local_root_names_itself_and_says_why_it_refuses() {
        let r = LocalRoot::new(key(7), "user", "the file /k");
        let other = LocalRoot::new(key(8), "user", "the file /j");
        assert_ne!(r.id(), other.id());
        assert_eq!(r.id().len(), 16);
        let w = r.wrap("backup", &key(9)).unwrap();
        assert_eq!(r.unwrap("backup", &w).unwrap(), key(9));
        let e = other.unwrap("backup", &w).unwrap_err();
        assert!(e.contains("backup") && e.contains(&other.id()), "{e}");
        assert_eq!(format!("{:?}", key(9)), "Key(…)");
    }

    #[test]
    fn hex_and_passphrases() {
        let k = key(0xab);
        assert_eq!(Key::from_hex(&format!(" {}\n", k.to_hex())), Some(k));
        assert_eq!(Key::from_hex("abc"), None);
        assert_eq!(Key::from_hex(&"zz".repeat(32)), None);
        assert_eq!(from_hex("0aFf"), Some(vec![0x0a, 0xff]));
        let a = root_from_passphrase(b"correct horse", b"salt", 1000);
        assert_eq!(a, root_from_passphrase(b"correct horse", b"salt", 1000));
        assert_ne!(a, root_from_passphrase(b"correct horse", b"pepper", 1000));
        assert_ne!(a, root_from_passphrase(b"correct horsf", b"salt", 1000));
    }
}
