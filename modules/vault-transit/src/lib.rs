//! nucleus's root key kept in HashiCorp Vault's Transit secrets engine (or
//! OpenBao's, its open fork with the same API) (PLAN.md §M14.30 K3): the
//! data keys of the keyring (`<dir>.keyring`) wrapped by Transit's
//! `encrypt` and unwrapped by its `decrypt` — the root never leaves Vault,
//! every unwrap is in Vault's audit log, and taking away the token's
//! right to the key (or the key) makes the database unreadable.
//!
//! A key is named after the scheme as `<key>` (the engine mounted at
//! `transit`) or `<mount>/<key>`: `--root-kms vault-transit:nucleus`.
//! Opening reads the key (`GET <mount>/keys/<key>`): one that cannot
//! both encrypt and decrypt, or is not an AEAD key (`aes256-gcm96`,
//! `aes128-gcm96`, `chacha20-poly1305`), is refused by name.  A data key
//! is encrypted with its purpose as associated data, so Vault itself
//! refuses to decrypt it for another; a key made with derivation gets
//! the purpose as its context too.
//!
//! Configured as Vault's CLI is: `VAULT_ADDR` (default
//! `https://127.0.0.1:8200`), `VAULT_TOKEN` (else `~/.vault-token`, where
//! `vault login` leaves it), `VAULT_NAMESPACE` (Enterprise and HCP), and
//! `VAULT_CACERT` (a private authority's PEM, trusted beside Mozilla's
//! roots).  Requests go over `nucleus-http` (TLS but to a loopback
//! address).

use std::sync::Arc;
use std::time::Duration;

use nucleus_server::keyring::{Key, KeyProvider, register_kms};
use nucleus_sql::ext::{base64_decode, base64_encode};
use serde_json::{Value, json};

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// The mount the engine is at when the key names none.
pub const DEFAULT_MOUNT: &str = "transit";

/// The key types whose encryption takes associated data.
const AEAD_TYPES: [&str; 3] = ["aes256-gcm96", "aes128-gcm96", "chacha20-poly1305"];

/// Lets `--root-kms vault-transit:<key>` use this module.
pub fn register() {
    register_kms("vault-transit", open);
}

/// The Transit key `key` names (`<key>` or `<mount>/<key>`), with the
/// address and token of the environment.
pub fn open(key: &str) -> Result<Arc<dyn KeyProvider>, String> {
    VaultTransit::new(key, |k| std::env::var(k).ok()).map(|k| Arc::new(k) as Arc<dyn KeyProvider>)
}

/// A key of a Transit engine.
pub struct VaultTransit {
    /// `https://vault.example:8200`.
    addr: String,
    token: String,
    namespace: Option<String>,
    mount: String,
    key: String,
    /// Made with derivation: the purpose is its context.
    derived: bool,
}

impl std::fmt::Debug for VaultTransit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultTransit")
            .field("addr", &self.addr)
            .field("namespace", &self.namespace)
            .field("mount", &self.mount)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// `<key>` or `<mount>/<key>`: the mount and the key; path segments of
/// letters, digits, `-`, `_` and `.` only (no `..`).
fn split_name(name: &str) -> Result<(String, String), String> {
    let bad = || {
        format!(
            "vault-transit:{name}: name a key as <key> or <mount>/<key> (letters, digits, -, _ and .)"
        )
    };
    let segments: Vec<&str> = name.split('/').collect();
    if segments.iter().any(|s| {
        s.is_empty()
            || *s == "."
            || *s == ".."
            || !s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }) {
        return Err(bad());
    }
    let (key, mount) = segments.split_last().ok_or_else(bad)?;
    let mount = if mount.is_empty() {
        DEFAULT_MOUNT.to_string()
    } else {
        mount.join("/")
    };
    Ok((mount, key.to_string()))
}

impl VaultTransit {
    /// The key `name` names, its settings read by `env` (the process's
    /// environment, or a test's), read from Vault.
    pub fn new(name: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let (mount, key) = split_name(name)?;
        let addr = env("VAULT_ADDR")
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "https://127.0.0.1:8200".into())
            .trim_end_matches('/')
            .to_string();
        let token = match env("VAULT_TOKEN").filter(|t| !t.is_empty()) {
            Some(t) => t,
            None => env("HOME")
                .and_then(|h| std::fs::read_to_string(format!("{h}/.vault-token")).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    format!(
                        "vault-transit:{name}: no token (VAULT_TOKEN, or ~/.vault-token from vault login)"
                    )
                })?,
        };
        if let Some(ca) = env("VAULT_CACERT").filter(|c| !c.is_empty()) {
            let pem = std::fs::read(&ca).map_err(|e| format!("VAULT_CACERT {ca}: {e}"))?;
            nucleus_http::trust_pem(&pem).map_err(|e| format!("VAULT_CACERT {ca}: {e}"))?;
        }
        let mut v = Self {
            addr,
            token,
            namespace: env("VAULT_NAMESPACE").filter(|n| !n.is_empty()),
            mount,
            key,
            derived: false,
        };
        let r = v.call("GET", &format!("keys/{}", v.key), None)?;
        let d = &r["data"];
        let ty = d["type"].as_str().unwrap_or("");
        if !AEAD_TYPES.contains(&ty) {
            return Err(format!(
                "{} is {ty}, not an AEAD key ({})",
                v.describe(),
                AEAD_TYPES.join(", ")
            ));
        }
        if d["supports_encryption"] != json!(true) || d["supports_decryption"] != json!(true) {
            return Err(format!(
                "{} does not both encrypt and decrypt",
                v.describe()
            ));
        }
        v.derived = d["derived"] == json!(true);
        Ok(v)
    }

    /// One call of Transit's API (`path` under the mount); retried a few
    /// times when Vault or the network says to try again.  Vault's errors
    /// by its words.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}/v1/{}/{path}", self.addr, self.mount);
        let body = body.map(Value::to_string);
        let mut headers: Vec<(&str, &str)> = vec![("X-Vault-Token", &self.token)];
        if let Some(n) = &self.namespace {
            headers.push(("X-Vault-Namespace", n));
        }
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let mut wait = Duration::from_millis(250);
        let mut tries = 0;
        loop {
            tries += 1;
            let r = nucleus_http::send(&nucleus_http::Request {
                method,
                url: &url,
                headers: &headers,
                body: body.as_deref().map(str::as_bytes),
                timeout: Duration::from_secs(10),
                total: Some(Duration::from_secs(30)),
                limit: 1 << 20,
                agent: "nucleus-vault-transit",
            });
            // (A standby's 429 and a sealed Vault's 503 pass.)
            let again = match &r {
                Err(e) => e.transient,
                Ok(r) => matches!(r.status, 429 | 500 | 502 | 503),
            };
            if again && tries < 4 {
                std::thread::sleep(wait);
                wait *= 2;
                continue;
            }
            let r = r.map_err(|e| format!("Vault {path}: {}", e.message))?;
            let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            if r.status != 200 {
                let errors = v["errors"]
                    .as_array()
                    .map(|e| {
                        e.iter()
                            .filter_map(Value::as_str)
                            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                let errors = match (r.status, errors.is_empty()) {
                    (404, true) => "no such key or mount".to_string(),
                    (_, true) => r.body_text(200),
                    _ => errors,
                };
                return Err(format!(
                    "Vault {} of {}: HTTP {} {errors}",
                    path.split('/').next().unwrap_or(path),
                    self.describe(),
                    r.status
                ));
            }
            return Ok(v);
        }
    }

    /// The request's body: `field` = `value`, bound to `purpose`.
    fn bound(&self, field: &str, value: String, purpose: &str) -> Value {
        let mut body = json!({
            field: value,
            "associated_data": base64_encode(purpose.as_bytes()),
        });
        if self.derived {
            body["context"] = json!(base64_encode(purpose.as_bytes()));
        }
        body
    }
}

impl KeyProvider for VaultTransit {
    fn id(&self) -> String {
        match &self.namespace {
            Some(n) => format!(
                "vault-transit:{}/{}/{}",
                n.trim_matches('/'),
                self.mount,
                self.key
            ),
            None => format!("vault-transit:{}/{}", self.mount, self.key),
        }
    }

    fn kind(&self) -> String {
        "kms:vault-transit".into()
    }

    fn describe(&self) -> String {
        format!(
            "the Vault Transit key {}/{} at {}",
            self.mount, self.key, self.addr
        )
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        let body = self.bound("plaintext", base64_encode(dek.bytes()), purpose);
        let r = self.call("POST", &format!("encrypt/{}", self.key), Some(&body))?;
        r["data"]["ciphertext"]
            .as_str()
            .map(|c| c.as_bytes().to_vec())
            .ok_or_else(|| "Vault's encrypt answered without a ciphertext".into())
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        let ciphertext = std::str::from_utf8(blob)
            .ok()
            .filter(|c| c.starts_with("vault:v"))
            .ok_or("not a Vault Transit ciphertext")?;
        let body = self.bound("ciphertext", ciphertext.to_string(), purpose);
        let r = self.call("POST", &format!("decrypt/{}", self.key), Some(&body))?;
        let mut plain = r["data"]["plaintext"]
            .as_str()
            .and_then(base64_decode)
            .ok_or("Vault's decrypt answered without a plaintext")?;
        let bytes: Result<[u8; 32], _> = plain.as_slice().try_into();
        plain.fill(0);
        let mut bytes =
            bytes.map_err(|_| format!("Vault decrypted {} bytes, not a key's 32", plain.len()))?;
        let key = Key::new(bytes);
        bytes.fill(0);
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mock::Mock;
    use nucleus_server::keyring::{Keyring, ProviderHandle, Root, Settings};

    #[test]
    fn names() {
        assert_eq!(
            split_name("db").unwrap(),
            ("transit".to_string(), "db".to_string())
        );
        assert_eq!(
            split_name("team/kms/db").unwrap(),
            ("team/kms".to_string(), "db".to_string())
        );
        // (A parent segment spelled apart: the boundary check reads
        // literal paths.)
        let parent = format!("{}/db", "..");
        for bad in ["", "/db", "a//b", &parent, "a/..", "d b", "a?b"] {
            assert!(split_name(bad).is_err(), "{bad}");
        }
    }

    /// A data key wrapped and unwrapped, for its purpose only; a wrong
    /// token, a key not there, a key that cannot encrypt, a token
    /// revoked — each refused by Vault's own words.
    #[test]
    fn data_keys_wrapped_by_transit() {
        let m = Mock::start();
        m.add_key("transit", "db", "aes256-gcm96");
        m.add_key("kms", "signing", "ed25519");
        let v = VaultTransit::new("db", m.env()).unwrap();
        assert_eq!(v.id(), "vault-transit:transit/db");
        assert_eq!(
            VaultTransit::new("transit/db", m.env()).unwrap().id(),
            v.id()
        );
        assert_eq!(v.kind(), "kms:vault-transit");

        let dek = Key::new([0x42; 32]);
        let blob = v.wrap("data", &dek).unwrap();
        assert!(blob.starts_with(b"vault:v1:"));
        assert_eq!(v.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
        let e = v.unwrap("backup", &blob).unwrap_err();
        assert!(e.contains("message authentication failed"), "{e}");
        assert!(v.unwrap("data", b"garbage").is_err());

        let wrong = |k: &str| match k {
            "VAULT_TOKEN" => Some("not-the-token".to_string()),
            _ => m.env()(k),
        };
        let e = VaultTransit::new("db", wrong).unwrap_err();
        assert!(e.contains("403") && e.contains("permission denied"), "{e}");
        let e = VaultTransit::new("none", m.env()).unwrap_err();
        assert!(e.contains("404"), "{e}");
        let e = VaultTransit::new("kms/signing", m.env()).unwrap_err();
        assert!(e.contains("not an AEAD key"), "{e}");
        let e = VaultTransit::new("db", |k: &str| match k {
            "VAULT_ADDR" => Some(m.addr.clone()),
            _ => None,
        })
        .unwrap_err();
        assert!(e.contains("VAULT_TOKEN"), "{e}");

        m.revoke();
        let e = v.unwrap("data", &blob).unwrap_err();
        assert!(e.contains("403"), "{e}");
    }

    struct Handle(Arc<dyn KeyProvider>);

    impl std::fmt::Debug for Handle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0.describe())
        }
    }

    impl ProviderHandle for Handle {
        fn provider(&self) -> Arc<dyn KeyProvider> {
            Arc::clone(&self.0)
        }
    }

    /// A keyring under a Transit root: its data keys only wrapped in the
    /// file, read back by another keyring, none once the token is revoked.
    #[test]
    fn a_keyring_under_a_transit_root() {
        let m = Mock::start();
        m.add_key("transit", "ring", "aes256-gcm96");
        let d = std::env::temp_dir().join(format!("nucleus-vault-transit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let kv = || -> Arc<dyn nucleus_kv::Kv> {
            Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
        };
        let settings = || Settings {
            root: Root::Provider(Arc::new(Handle(Arc::new(
                VaultTransit::new("ring", m.env()).unwrap(),
            )))),
            ..Settings::default_for(db)
        };
        let ring = Keyring::new(db, settings(), kv());
        let dek = ring.dek("data", true).unwrap().unwrap();
        let file = std::fs::read_to_string(format!("{db}.keyring")).unwrap();
        assert!(file.contains("vault-transit:transit/ring"), "{file}");
        assert!(!file.contains(&dek.to_hex()), "the data key in the clear");
        assert!(!std::path::Path::new(&format!("{db}.root-key")).exists());

        let held = Keyring::new(db, settings(), kv());
        assert_eq!(
            held.dek("data", false).unwrap().unwrap().bytes(),
            dek.bytes()
        );
        let later = Keyring::new(db, settings(), kv());
        m.revoke();
        let e = later.dek("data", false).unwrap_err();
        assert!(e.contains("403"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
