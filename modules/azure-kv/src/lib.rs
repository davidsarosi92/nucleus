//! nucleus's root key kept in Azure Key Vault (PLAN.md §M14.30 K3): the
//! data keys of the keyring (`<dir>.keyring`) encrypted by an RSA key of
//! a vault (`encrypt`, RSA-OAEP-256) and decrypted by it (`decrypt`) —
//! the private key never leaves Key Vault, every use is in its logs, and
//! disabling or deleting the key (or taking away the right to use it)
//! makes the database unreadable.
//!
//! A key is named after the scheme as `<vault>/<key>` (the vault at
//! `https://<vault>.vault.azure.net`) or by its URL,
//! `https://<host>/keys/<key>` (another cloud's vault; an emulator's):
//! `--root-kms azure-kv:myvault/nucleus`.  Opening reads the key: one not
//! RSA, not enabled, or not allowed both to encrypt and to decrypt is
//! refused by name.  RSA-OAEP takes no associated data, so the purpose is
//! encrypted with the key (`nucleus-purpose:<purpose>`, then the key's 32
//! bytes) and checked when it comes back: OAEP is non-malleable, so a
//! wrapped key offered for another purpose is refused.  What is kept is
//! the key's version, then the ciphertext: a key rotated in Key Vault
//! still decrypts what an older version encrypted.
//!
//! Credentials are found as Azure's SDKs find them: a service
//! principal's secret (`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
//! `AZURE_CLIENT_SECRET`), workload identity (`AZURE_FEDERATED_TOKEN_FILE`
//! with the tenant and client), a managed identity's endpoint
//! (`IDENTITY_ENDPOINT` and `IDENTITY_HEADER`: App Service, Container
//! Apps, Arc), else the machine's instance metadata service
//! (`169.254.169.254`; `AZURE_CLIENT_ID` picks a user-assigned
//! identity).  `AZURE_AUTHORITY_HOST` names another sign-in endpoint.
//! Tokens are kept until a minute before they expire.  Requests go over
//! `nucleus-http`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nucleus_server::keyring::{Key, KeyProvider, register_kms};
use nucleus_sql::ext::{base64_decode, base64_encode};
use serde_json::{Value, json};

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// The API version asked for.
pub const API_VERSION: &str = "7.4";
/// The resource tokens are asked for.
pub const RESOURCE: &str = "https://vault.azure.net";
/// What precedes the purpose in what is encrypted.
const BINDING: &str = "nucleus-purpose:";

/// Lets `--root-kms azure-kv:<key>` use this module.
pub fn register() {
    register_kms("azure-kv", open);
}

/// The Key Vault key `key` names (`<vault>/<key>` or its URL), with the
/// credentials of the environment.
pub fn open(key: &str) -> Result<Arc<dyn KeyProvider>, String> {
    AzureKeyVault::new(key, |k| std::env::var(k).ok()).map(|k| Arc::new(k) as Arc<dyn KeyProvider>)
}

/// Where tokens come from.
enum Source {
    /// A service principal's secret, or a federated token's file.
    Principal {
        authority: String,
        tenant: String,
        client: String,
        secret: Option<String>,
        assertion_file: Option<String>,
    },
    /// An identity endpoint (App Service and the like).
    Identity { endpoint: String, header: String },
    /// The instance metadata service, for this client (user-assigned).
    Imds(Option<String>),
}

struct Token {
    token: String,
    renew: Option<Instant>,
}

/// An RSA key in Azure Key Vault.
pub struct AzureKeyVault {
    /// `https://myvault.vault.azure.net`.
    vault: String,
    key: String,
    source: Source,
    token: Mutex<Option<Token>>,
}

impl std::fmt::Debug for AzureKeyVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureKeyVault")
            .field("vault", &self.vault)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Whether `s` is a vault's, a key's or a version's name: letters, digits
/// and `-`.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `<vault>/<key>` or `https://<host>/keys/<key>`: the vault's URL and
/// the key.
fn split_name(name: &str) -> Result<(String, String), String> {
    let bad = || {
        format!("azure-kv:{name}: name a key as <vault>/<key> or https://<vault host>/keys/<key>")
    };
    if let Some(rest) = name
        .strip_prefix("https://")
        .or_else(|| name.strip_prefix("http://"))
    {
        let scheme = &name[..name.len() - rest.len()];
        let (host, path) = rest.split_once('/').ok_or_else(bad)?;
        let key = path.strip_prefix("keys/").ok_or_else(bad)?;
        let host_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':'));
        if !host_ok || !plain(key) {
            return Err(bad());
        }
        return Ok((format!("{scheme}{host}"), key.to_string()));
    }
    let (vault, key) = name.split_once('/').ok_or_else(bad)?;
    if !plain(vault) || !plain(key) {
        return Err(bad());
    }
    Ok((format!("https://{vault}.vault.azure.net"), key.to_string()))
}

/// `b` in base64url without padding (as Key Vault's JSON has bytes).
fn b64url(b: &[u8]) -> String {
    base64_encode(b)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// base64url, padded or not.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut t = s.replace('-', "+").replace('_', "/");
    while !t.len().is_multiple_of(4) {
        t.push('=');
    }
    base64_decode(&t)
}

/// A form's body, its values percent-encoded.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", percent(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// One HTTP exchange, retried a few times when the server or the network
/// says to try again: the status and the body as JSON.
fn exchange(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    what: &str,
) -> Result<(u16, Value), String> {
    let mut wait = Duration::from_millis(250);
    let mut tries = 0;
    loop {
        tries += 1;
        let r = nucleus_http::send(&nucleus_http::Request {
            method,
            url,
            headers,
            body,
            timeout: Duration::from_secs(10),
            total: Some(Duration::from_secs(30)),
            limit: 1 << 20,
            agent: "nucleus-azure-kv",
        });
        let again = match &r {
            Err(e) => e.transient,
            Ok(r) => matches!(r.status, 429 | 500 | 502 | 503 | 504),
        };
        if again && tries < 4 {
            std::thread::sleep(wait);
            wait *= 2;
            continue;
        }
        let r = r.map_err(|e| format!("{what}: {}", e.message))?;
        let v = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        return Ok((r.status, v));
    }
}

/// Azure's error in `v`: Key Vault's `error.code` and `error.message`, or
/// Entra ID's `error` and `error_description`.
fn azure_error(status: u16, v: &Value) -> String {
    let e = &v["error"];
    let (code, message) = if e.is_object() {
        (
            e["code"].as_str().unwrap_or(""),
            e["message"].as_str().unwrap_or(""),
        )
    } else {
        (
            e.as_str().unwrap_or(""),
            v["error_description"].as_str().unwrap_or(""),
        )
    };
    let message = message.lines().next().unwrap_or("");
    format!("HTTP {status} {code} {message}").trim().to_string()
}

impl Source {
    fn find(env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |k: &str| env(k).filter(|v| !v.is_empty());
        let authority = var("AZURE_AUTHORITY_HOST")
            .unwrap_or_else(|| "https://login.microsoftonline.com".into())
            .trim_end_matches('/')
            .to_string();
        if let (Some(tenant), Some(client)) = (var("AZURE_TENANT_ID"), var("AZURE_CLIENT_ID")) {
            let secret = var("AZURE_CLIENT_SECRET");
            let assertion_file = var("AZURE_FEDERATED_TOKEN_FILE");
            if secret.is_some() || assertion_file.is_some() {
                return Ok(Source::Principal {
                    authority,
                    tenant,
                    client,
                    secret,
                    assertion_file,
                });
            }
        }
        if let (Some(endpoint), Some(header)) = (var("IDENTITY_ENDPOINT"), var("IDENTITY_HEADER")) {
            return Ok(Source::Identity { endpoint, header });
        }
        Ok(Source::Imds(var("AZURE_CLIENT_ID")))
    }

    fn fetch(&self) -> Result<(String, Option<Duration>), String> {
        let (status, v) = match self {
            Source::Principal {
                authority,
                tenant,
                client,
                secret,
                assertion_file,
            } => {
                let scope = format!("{RESOURCE}/.default");
                let mut pairs = vec![
                    ("grant_type", "client_credentials".to_string()),
                    ("client_id", client.clone()),
                    ("scope", scope),
                ];
                match (secret, assertion_file) {
                    (Some(s), _) => pairs.push(("client_secret", s.clone())),
                    (None, Some(f)) => {
                        let t = std::fs::read_to_string(f)
                            .map_err(|e| format!("AZURE_FEDERATED_TOKEN_FILE {f}: {e}"))?;
                        pairs.push((
                            "client_assertion_type",
                            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".into(),
                        ));
                        pairs.push(("client_assertion", t.trim().to_string()));
                    }
                    (None, None) => unreachable!("a principal has a secret or an assertion"),
                }
                let pairs: Vec<(&str, &str)> =
                    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                exchange(
                    "POST",
                    &format!("{authority}/{tenant}/oauth2/v2.0/token"),
                    &[("Content-Type", "application/x-www-form-urlencoded")],
                    Some(form(&pairs).as_bytes()),
                    "Entra ID's token endpoint",
                )?
            }
            Source::Identity { endpoint, header } => exchange(
                "GET",
                &format!(
                    "{endpoint}?api-version=2019-08-01&resource={}",
                    percent(RESOURCE)
                ),
                &[("X-IDENTITY-HEADER", header)],
                None,
                "the managed identity's endpoint",
            )?,
            Source::Imds(client) => exchange(
                "GET",
                &format!(
                    "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}{}",
                    percent(RESOURCE),
                    client
                        .as_ref()
                        .map_or(String::new(), |c| format!("&client_id={}", percent(c)))
                ),
                &[("Metadata", "true")],
                None,
                "the instance metadata service (no AZURE_CLIENT_SECRET, AZURE_FEDERATED_TOKEN_FILE or IDENTITY_ENDPOINT)",
            )?,
        };
        if status != 200 {
            return Err(format!("no token: {}", azure_error(status, &v)));
        }
        let token = v["access_token"]
            .as_str()
            .ok_or("no token in the token endpoint's answer")?
            .to_string();
        // A number, or (the identity endpoints) a number in a string.
        let lasts = v["expires_in"]
            .as_u64()
            .or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .map(Duration::from_secs);
        Ok((token, lasts))
    }
}

impl AzureKeyVault {
    /// The key `name` names, its credentials found by `env` (the
    /// process's environment, or a test's), read from Key Vault.
    pub fn new(name: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let (vault, key) = split_name(name)?;
        let kv = Self {
            vault,
            key,
            source: Source::find(&env)?,
            token: Mutex::new(None),
        };
        let r = kv.call("GET", "", None)?;
        let kty = r["key"]["kty"].as_str().unwrap_or("");
        if kty != "RSA" && kty != "RSA-HSM" {
            return Err(format!("{} is {kty}, not an RSA key", kv.describe()));
        }
        if r["attributes"]["enabled"] == json!(false) {
            return Err(format!("{} is disabled", kv.describe()));
        }
        let ops: Vec<&str> = r["key"]["key_ops"]
            .as_array()
            .map(|o| o.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !ops.contains(&"encrypt") || !ops.contains(&"decrypt") {
            return Err(format!(
                "{} may not both encrypt and decrypt (its operations: {})",
                kv.describe(),
                ops.join(", ")
            ));
        }
        Ok(kv)
    }

    fn token(&self) -> Result<String, String> {
        let mut t = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = t.as_ref()
            && t.renew.is_none_or(|r| Instant::now() < r)
        {
            return Ok(t.token.clone());
        }
        let (token, lasts) = self.source.fetch()?;
        *t = Some(Token {
            token: token.clone(),
            renew: lasts.map(|d| Instant::now() + d.saturating_sub(Duration::from_secs(60))),
        });
        Ok(token)
    }

    /// One call of Key Vault's REST API on the key (`suffix`: `""`,
    /// `/encrypt`, `/<version>/decrypt`).
    fn call(&self, method: &str, suffix: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!(
            "{}/keys/{}{suffix}?api-version={API_VERSION}",
            self.vault, self.key
        );
        let auth = format!("Bearer {}", self.token()?);
        let body = body.map(Value::to_string);
        let mut headers = vec![("Authorization", auth.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let what = suffix.rsplit('/').next().filter(|w| !w.is_empty());
        let what = what.unwrap_or("get");
        let (status, v) = exchange(
            method,
            &url,
            &headers,
            body.as_deref().map(str::as_bytes),
            &format!("Key Vault {what}"),
        )?;
        if status != 200 {
            if status == 401 {
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            return Err(format!(
                "Key Vault {what} of {}: {}",
                self.describe(),
                azure_error(status, &v)
            ));
        }
        Ok(v)
    }
}

impl KeyProvider for AzureKeyVault {
    fn id(&self) -> String {
        format!("azure-kv:{}/keys/{}", self.vault, self.key)
    }

    fn kind(&self) -> String {
        "kms:azure".into()
    }

    fn describe(&self) -> String {
        format!("the Key Vault key {}/keys/{}", self.vault, self.key)
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        let mut plain = format!("{BINDING}{purpose}\0").into_bytes();
        plain.extend_from_slice(dek.bytes());
        let body = json!({ "alg": "RSA-OAEP-256", "value": b64url(&plain) });
        plain.fill(0);
        let r = self.call("POST", "/encrypt", Some(&body))?;
        let version = r["kid"]
            .as_str()
            .and_then(|k| k.rsplit('/').next())
            .filter(|v| plain_version(v))
            .ok_or("Key Vault's encrypt answered without the key's version")?;
        let cipher = r["value"]
            .as_str()
            .and_then(b64url_decode)
            .ok_or("Key Vault's encrypt answered without a ciphertext")?;
        let mut blob = version.as_bytes().to_vec();
        blob.push(0);
        blob.extend_from_slice(&cipher);
        Ok(blob)
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        let at = blob
            .iter()
            .position(|&b| b == 0)
            .ok_or("not a Key Vault ciphertext")?;
        let version = std::str::from_utf8(&blob[..at])
            .ok()
            .filter(|v| plain_version(v))
            .ok_or("not a Key Vault ciphertext")?;
        let body = json!({ "alg": "RSA-OAEP-256", "value": b64url(&blob[at + 1..]) });
        let r = self.call("POST", &format!("/{version}/decrypt"), Some(&body))?;
        let mut plain = r["value"]
            .as_str()
            .and_then(b64url_decode)
            .ok_or("Key Vault's decrypt answered without a plaintext")?;
        let bound = format!("{BINDING}{purpose}\0");
        let ok = plain.len() == bound.len() + 32 && plain.starts_with(bound.as_bytes());
        if !ok {
            plain.fill(0);
            return Err(format!(
                "{} decrypted a key that was not wrapped for {purpose}",
                self.describe()
            ));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&plain[bound.len()..]);
        plain.fill(0);
        let key = Key::new(bytes);
        bytes.fill(0);
        Ok(key)
    }
}

/// A key version's name: hexadecimal (32 digits in Azure), or letters,
/// digits and `-` (an emulator's).
fn plain_version(v: &str) -> bool {
    plain(v) && v.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;
    use mock::Mock;
    use nucleus_server::keyring::{Keyring, ProviderHandle, Root, Settings};

    #[test]
    fn names() {
        assert_eq!(
            split_name("myvault/db").unwrap(),
            (
                "https://myvault.vault.azure.net".to_string(),
                "db".to_string()
            )
        );
        assert_eq!(
            split_name("https://v.vault.azure.cn/keys/db").unwrap(),
            ("https://v.vault.azure.cn".to_string(), "db".to_string())
        );
        assert_eq!(
            split_name("http://127.0.0.1:8443/keys/db").unwrap(),
            ("http://127.0.0.1:8443".to_string(), "db".to_string())
        );
        for bad in [
            "",
            "db",
            "my vault/db",
            "v/d/b",
            "https://v.vault.azure.net/secrets/db",
            "https://v.vault.azure.net/keys/db/1",
            "https:///keys/db",
        ] {
            assert!(split_name(bad).is_err(), "{bad}");
        }
        assert_eq!(
            b64url_decode(&b64url(&[0xfb, 0xff, 1])).unwrap(),
            [0xfb, 0xff, 1]
        );
    }

    /// Through each kind of credentials: a data key wrapped and
    /// unwrapped, for its purpose only, still after the key is rotated; a
    /// secret refused, a key not there, an EC key, a key disabled — each
    /// refused by Key Vault's own words.
    #[test]
    fn data_keys_wrapped_by_key_vault() {
        let m = Mock::start();
        m.add_key("db", "RSA", &["encrypt", "decrypt"]);
        m.add_key("signing", "EC", &["sign", "verify"]);
        m.add_key("wraponly", "RSA", &["wrapKey", "unwrapKey"]);
        let dir = std::env::temp_dir().join(format!("nucleus-azure-kv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = m.key_url("db");

        let dek = Key::new([0x42; 32]);
        let mut blob = Vec::new();
        for (i, env) in [m.env_secret(), m.env_federated(&dir), m.env_identity()]
            .into_iter()
            .enumerate()
        {
            let kv =
                AzureKeyVault::new(&key, env).unwrap_or_else(|e| panic!("credentials {i}: {e}"));
            assert_eq!(kv.id(), format!("azure-kv:{key}"));
            assert_eq!(kv.kind(), "kms:azure");
            if blob.is_empty() {
                blob = kv.wrap("data", &dek).unwrap();
            }
            assert_eq!(kv.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
            let e = kv.unwrap("backup", &blob).unwrap_err();
            assert!(e.contains("not wrapped for backup"), "{e}");
        }
        assert!(m.tokens_issued() >= 3, "{}", m.tokens_issued());

        // Rotated: the old version still decrypts what it encrypted.
        m.rotate("db");
        let kv = AzureKeyVault::new(&key, m.env_secret()).unwrap();
        assert_eq!(kv.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
        let newer = kv.wrap("data", &dek).unwrap();
        assert_ne!(
            newer[..newer.iter().position(|&b| b == 0).unwrap()],
            blob[..blob.iter().position(|&b| b == 0).unwrap()]
        );
        assert!(kv.unwrap("data", b"no version").is_err());

        let e = AzureKeyVault::new(&key, m.env_bad_secret()).unwrap_err();
        assert!(e.contains("invalid_client"), "{e}");
        let e = AzureKeyVault::new(&m.key_url("none"), m.env_secret()).unwrap_err();
        assert!(e.contains("KeyNotFound"), "{e}");
        let e = AzureKeyVault::new(&m.key_url("signing"), m.env_secret()).unwrap_err();
        assert!(e.contains("not an RSA key"), "{e}");
        let e = AzureKeyVault::new(&m.key_url("wraponly"), m.env_secret()).unwrap_err();
        assert!(e.contains("may not both encrypt and decrypt"), "{e}");

        m.set_enabled("db", false);
        let e = kv.unwrap("data", &blob).unwrap_err();
        assert!(e.contains("Forbidden"), "{e}");
        let e = AzureKeyVault::new(&key, m.env_secret()).unwrap_err();
        assert!(e.contains("disabled"), "{e}");
        m.set_enabled("db", true);
        assert!(kv.unwrap("data", &blob).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A keyring under a Key Vault root: its data keys only wrapped in the
    /// file, read back by another keyring, none once the key is disabled.
    #[test]
    fn a_keyring_under_a_key_vault_root() {
        let m = Mock::start();
        m.add_key("ring", "RSA-HSM", &["encrypt", "decrypt"]);
        let key = m.key_url("ring");
        let d = std::env::temp_dir().join(format!("nucleus-azure-kv-ring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let kv = || -> Arc<dyn nucleus_kv::Kv> {
            Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
        };
        let settings = || Settings {
            root: Root::Provider(Arc::new(Handle(Arc::new(
                AzureKeyVault::new(&key, m.env_secret()).unwrap(),
            )))),
            ..Settings::default_for(db)
        };
        let ring = Keyring::new(db, settings(), kv());
        let dek = ring.dek("data", true).unwrap().unwrap();
        let file = std::fs::read_to_string(format!("{db}.keyring")).unwrap();
        assert!(file.contains(&format!("azure-kv:{key}")), "{file}");
        assert!(!file.contains(&dek.to_hex()), "the data key in the clear");

        let held = Keyring::new(db, settings(), kv());
        assert_eq!(
            held.dek("data", false).unwrap().unwrap().bytes(),
            dek.bytes()
        );
        let later = Keyring::new(db, settings(), kv());
        m.set_enabled("ring", false);
        let e = later.dek("data", false).unwrap_err();
        assert!(e.contains("Forbidden"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
