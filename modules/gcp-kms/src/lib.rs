//! nucleus's root key kept in Google Cloud KMS (PLAN.md §M14.30 K3): the
//! data keys of the keyring (`<dir>.keyring`) wrapped by Cloud KMS's
//! `encrypt` and unwrapped by its `decrypt` under a symmetric key — the
//! root never leaves Cloud KMS, every use is in Cloud Audit Logs, and
//! disabling or destroying the key's version (or taking away the right to
//! use it) makes the database unreadable.
//!
//! A key is named by its resource name after the scheme:
//! `--root-kms gcp-kms:projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>`.
//! Opening reads the key: one not for `ENCRYPT_DECRYPT`, or whose primary
//! version is not enabled, is refused by name.  A data key is encrypted
//! with its purpose as additional authenticated data, so Cloud KMS
//! itself refuses to decrypt it for another.
//!
//! Credentials are found as Google's client libraries find them
//! (application default credentials): `GOOGLE_OAUTH_ACCESS_TOKEN` (a
//! token as it is), else the file `GOOGLE_APPLICATION_CREDENTIALS` names
//! or gcloud's `~/.config/gcloud/application_default_credentials.json` —
//! a service account's key (a JWT signed with it, RS256, exchanged for a
//! token) or a user's refresh token — else the metadata server of the
//! machine (Compute Engine, GKE's workload identity, Cloud Run;
//! `GCE_METADATA_HOST` to name another).  Tokens are kept until a minute
//! before they expire.  `CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDKMS` names
//! another endpoint (as for gcloud).  Requests go over `nucleus-http`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nucleus_server::keyring::{Key, KeyProvider, register_kms};
use nucleus_sql::ext::{base64_decode, base64_encode};
use serde_json::{Value, json};

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// The scope a token is asked for.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloudkms";

/// Lets `--root-kms gcp-kms:<key>` use this module.
pub fn register() {
    register_kms("gcp-kms", open);
}

/// The Cloud KMS key `key` names (its resource name), with the
/// credentials of the environment.
pub fn open(key: &str) -> Result<Arc<dyn KeyProvider>, String> {
    GcpKms::new(key, |k| std::env::var(k).ok()).map(|k| Arc::new(k) as Arc<dyn KeyProvider>)
}

/// Where tokens come from.
enum Source {
    /// Given as it is.
    Fixed(String),
    /// A service account's key: a JWT it signs, exchanged at `token_uri`.
    ServiceAccount {
        email: String,
        key: Box<ring::rsa::KeyPair>,
        key_id: String,
        token_uri: String,
    },
    /// A user's refresh token (gcloud's application default credentials).
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    /// The machine's metadata server, at this address.
    Metadata(String),
}

/// A token and when to fetch another.
struct Token {
    token: String,
    renew: Option<Instant>,
}

/// A symmetric key in Cloud KMS.
pub struct GcpKms {
    /// `https://cloudkms.googleapis.com`, or the one set.
    endpoint: String,
    /// `projects/…/cryptoKeys/…`.
    name: String,
    source: Source,
    token: Mutex<Option<Token>>,
}

impl std::fmt::Debug for GcpKms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpKms")
            .field("endpoint", &self.endpoint)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Whether `name` is a key's resource name:
/// `projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>`, each part of
/// letters, digits, `-`, `_` and `.`.
fn valid_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    parts.len() == 8
        && [0, 2, 4, 6]
            .iter()
            .zip(["projects", "locations", "keyRings", "cryptoKeys"])
            .all(|(&i, word)| parts[i] == word)
        && [1, 3, 5, 7].iter().all(|&i| {
            !parts[i].is_empty()
                && parts[i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// `b` in base64url without padding (a JWT's parts).
fn b64url(b: &[u8]) -> String {
    base64_encode(b)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// The DER of a PEM's first block.
fn pem_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    base64_decode(body.trim())
}

/// A form's body, its values percent-encoded.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            let enc: String = v
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("{k}={enc}")
        })
        .collect::<Vec<_>>()
        .join("&")
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
            agent: "nucleus-gcp-kms",
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

/// Google's error in `v` (`error.status` and `error.message`, or OAuth's
/// `error` and `error_description`).
fn google_error(status: u16, v: &Value) -> String {
    let e = &v["error"];
    let (kind, message) = if e.is_object() {
        (
            e["status"].as_str().unwrap_or(""),
            e["message"].as_str().unwrap_or(""),
        )
    } else {
        (
            e.as_str().unwrap_or(""),
            v["error_description"].as_str().unwrap_or(""),
        )
    };
    format!("HTTP {status} {kind} {message}").trim().to_string()
}

impl Source {
    /// From the environment, as application default credentials are
    /// found.
    fn find(env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        if let Some(t) = env("GOOGLE_OAUTH_ACCESS_TOKEN").filter(|t| !t.is_empty()) {
            return Ok(Source::Fixed(t));
        }
        let file = match env("GOOGLE_APPLICATION_CREDENTIALS").filter(|f| !f.is_empty()) {
            Some(f) => Some(f),
            None => env("HOME")
                .map(|h| format!("{h}/.config/gcloud/application_default_credentials.json"))
                .filter(|f| std::path::Path::new(f).exists()),
        };
        let Some(file) = file else {
            let host = env("GCE_METADATA_HOST")
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "169.254.169.254".into());
            return Ok(Source::Metadata(host));
        };
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
        let c: Value =
            serde_json::from_str(&text).map_err(|e| format!("{file}: not JSON ({e})"))?;
        let field = |k: &str| {
            c[k].as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{file}: no {k}"))
        };
        let token_uri = c["token_uri"]
            .as_str()
            .unwrap_or("https://oauth2.googleapis.com/token")
            .to_string();
        match c["type"].as_str() {
            Some("service_account") => {
                let der = pem_der(&field("private_key")?)
                    .ok_or_else(|| format!("{file}: the private key is not PEM"))?;
                let key = ring::rsa::KeyPair::from_pkcs8(&der)
                    .map_err(|e| format!("{file}: the private key: {e}"))?;
                Ok(Source::ServiceAccount {
                    email: field("client_email")?,
                    key: Box::new(key),
                    key_id: c["private_key_id"].as_str().unwrap_or("").to_string(),
                    token_uri,
                })
            }
            Some("authorized_user") => Ok(Source::User {
                client_id: field("client_id")?,
                client_secret: field("client_secret")?,
                refresh_token: field("refresh_token")?,
                token_uri,
            }),
            other => Err(format!(
                "{file}: credentials of type {} (service_account and authorized_user are known)",
                other.unwrap_or("none")
            )),
        }
    }

    /// A new token, and how long it lasts.
    fn fetch(&self) -> Result<(String, Option<Duration>), String> {
        let (status, v) = match self {
            Source::Fixed(t) => return Ok((t.clone(), None)),
            Source::ServiceAccount {
                email,
                key,
                key_id,
                token_uri,
            } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let header = json!({ "alg": "RS256", "typ": "JWT", "kid": key_id });
                let claims = json!({
                    "iss": email, "scope": SCOPE, "aud": token_uri,
                    "iat": now, "exp": now + 3600,
                });
                let signing = format!(
                    "{}.{}",
                    b64url(header.to_string().as_bytes()),
                    b64url(claims.to_string().as_bytes())
                );
                let mut sig = vec![0u8; key.public().modulus_len()];
                key.sign(
                    &ring::signature::RSA_PKCS1_SHA256,
                    &ring::rand::SystemRandom::new(),
                    signing.as_bytes(),
                    &mut sig,
                )
                .map_err(|_| "signing the token request failed".to_string())?;
                let jwt = format!("{signing}.{}", b64url(&sig));
                let body = form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", &jwt),
                ]);
                exchange(
                    "POST",
                    token_uri,
                    &[("Content-Type", "application/x-www-form-urlencoded")],
                    Some(body.as_bytes()),
                    "Google's token endpoint",
                )?
            }
            Source::User {
                client_id,
                client_secret,
                refresh_token,
                token_uri,
            } => {
                let body = form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id),
                    ("client_secret", client_secret),
                    ("refresh_token", refresh_token),
                ]);
                exchange(
                    "POST",
                    token_uri,
                    &[("Content-Type", "application/x-www-form-urlencoded")],
                    Some(body.as_bytes()),
                    "Google's token endpoint",
                )?
            }
            Source::Metadata(host) => exchange(
                "GET",
                &format!(
                    "http://{host}/computeMetadata/v1/instance/service-accounts/default/token?scopes={SCOPE}"
                ),
                &[("Metadata-Flavor", "Google")],
                None,
                "the metadata server (no GOOGLE_APPLICATION_CREDENTIALS, no gcloud credentials)",
            )?,
        };
        if status != 200 {
            return Err(format!("no token: {}", google_error(status, &v)));
        }
        let token = v["access_token"]
            .as_str()
            .ok_or("no token in the token endpoint's answer")?
            .to_string();
        let lasts = v["expires_in"].as_u64().map(Duration::from_secs);
        Ok((token, lasts))
    }
}

impl GcpKms {
    /// The key `name` names, its credentials found by `env` (the
    /// process's environment, or a test's), read from Cloud KMS.
    pub fn new(name: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        if !valid_name(name) {
            return Err(format!(
                "gcp-kms:{name}: name a key as projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>"
            ));
        }
        let endpoint = env("CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDKMS")
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "https://cloudkms.googleapis.com".into())
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string();
        let kms = Self {
            endpoint,
            name: name.to_string(),
            source: Source::find(&env)?,
            token: Mutex::new(None),
        };
        let key = kms.call("GET", "", None)?;
        let purpose = key["purpose"].as_str().unwrap_or("");
        if purpose != "ENCRYPT_DECRYPT" {
            return Err(format!(
                "{} is for {purpose}, not ENCRYPT_DECRYPT",
                kms.describe()
            ));
        }
        let state = key["primary"]["state"].as_str().unwrap_or("none");
        if state != "ENABLED" {
            return Err(format!(
                "{}'s primary version is {state}, not ENABLED",
                kms.describe()
            ));
        }
        Ok(kms)
    }

    /// A token, fetched when there is none or it is about to expire.
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

    /// One call of Cloud KMS's REST API on the key (`suffix`: `""`,
    /// `:encrypt`, `:decrypt`).
    fn call(&self, method: &str, suffix: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}/v1/{}{suffix}", self.endpoint, self.name);
        let auth = format!("Bearer {}", self.token()?);
        let body = body.map(Value::to_string);
        let mut headers = vec![("Authorization", auth.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let what = if suffix.is_empty() {
            "get"
        } else {
            &suffix[1..]
        };
        let (status, v) = exchange(
            method,
            &url,
            &headers,
            body.as_deref().map(str::as_bytes),
            &format!("Cloud KMS {what}"),
        )?;
        if status != 200 {
            if status == 401 {
                // A token revoked or expired early: the next call fetches another.
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            return Err(format!(
                "Cloud KMS {what} of {}: {}",
                self.name,
                google_error(status, &v)
            ));
        }
        Ok(v)
    }
}

impl KeyProvider for GcpKms {
    fn id(&self) -> String {
        format!("gcp-kms:{}", self.name)
    }

    fn kind(&self) -> String {
        "kms:gcp".into()
    }

    fn describe(&self) -> String {
        format!("the Cloud KMS key {}", self.name)
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        let r = self.call(
            "POST",
            ":encrypt",
            Some(&json!({
                "plaintext": base64_encode(dek.bytes()),
                "additionalAuthenticatedData": base64_encode(purpose.as_bytes()),
            })),
        )?;
        r["ciphertext"]
            .as_str()
            .and_then(base64_decode)
            .ok_or_else(|| "Cloud KMS's encrypt answered without a ciphertext".into())
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        let r = self.call(
            "POST",
            ":decrypt",
            Some(&json!({
                "ciphertext": base64_encode(blob),
                "additionalAuthenticatedData": base64_encode(purpose.as_bytes()),
            })),
        )?;
        let mut plain = r["plaintext"]
            .as_str()
            .and_then(base64_decode)
            .ok_or("Cloud KMS's decrypt answered without a plaintext")?;
        let bytes: Result<[u8; 32], _> = plain.as_slice().try_into();
        plain.fill(0);
        let mut bytes = bytes
            .map_err(|_| format!("Cloud KMS decrypted {} bytes, not a key's 32", plain.len()))?;
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

    const KEY: &str = "projects/p1/locations/europe-west3/keyRings/ring/cryptoKeys/db";

    #[test]
    fn names_and_encodings() {
        assert!(valid_name(KEY));
        for bad in [
            "",
            "projects/p1",
            "projects/p1/locations/l/keyRings/r/cryptoKeys/",
            "projects/p1/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1",
            "projects/p 1/locations/l/keyRings/r/cryptoKeys/k",
            "keys/p1/locations/l/keyRings/r/cryptoKeys/k",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(form(&[("a", "x y/z"), ("b", "~.")]), "a=x%20y%2Fz&b=~.");
    }

    /// Through each kind of credentials: a data key wrapped and
    /// unwrapped, for its purpose only; a token refused, a key not there,
    /// a key for signing, a version disabled — each refused by Cloud
    /// KMS's own words.
    #[test]
    fn data_keys_wrapped_by_cloud_kms() {
        let m = Mock::start();
        m.add_key(KEY, "ENCRYPT_DECRYPT");
        let signing = "projects/p1/locations/global/keyRings/ring/cryptoKeys/sign";
        m.add_key(signing, "ASYMMETRIC_SIGN");
        let dir = std::env::temp_dir().join(format!("nucleus-gcp-kms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let dek = Key::new([0x42; 32]);
        let mut blob = Vec::new();
        for (i, env) in [
            m.env_token(),
            m.env_service_account(&dir),
            m.env_user(&dir),
            m.env_metadata(),
        ]
        .into_iter()
        .enumerate()
        {
            let kms = GcpKms::new(KEY, env).unwrap_or_else(|e| panic!("credentials {i}: {e}"));
            assert_eq!(kms.id(), format!("gcp-kms:{KEY}"));
            assert_eq!(kms.kind(), "kms:gcp");
            if blob.is_empty() {
                blob = kms.wrap("data", &dek).unwrap();
            }
            assert_eq!(kms.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
            let e = kms.unwrap("backup", &blob).unwrap_err();
            assert!(e.contains("INVALID_ARGUMENT"), "{e}");
        }
        assert!(m.tokens_issued() >= 3, "{}", m.tokens_issued());

        let e = GcpKms::new(KEY, m.env_bad_token()).unwrap_err();
        assert!(e.contains("401") && e.contains("UNAUTHENTICATED"), "{e}");
        let missing = KEY.replace("/db", "/none");
        let e = GcpKms::new(&missing, m.env_token()).unwrap_err();
        assert!(e.contains("404"), "{e}");
        let e = GcpKms::new(signing, m.env_token()).unwrap_err();
        assert!(e.contains("ASYMMETRIC_SIGN"), "{e}");
        assert!(GcpKms::new("projects/x", m.env_token()).is_err());

        let kms = GcpKms::new(KEY, m.env_token()).unwrap();
        m.set_enabled(KEY, false);
        let e = kms.unwrap("data", &blob).unwrap_err();
        assert!(e.contains("FAILED_PRECONDITION"), "{e}");
        let e = GcpKms::new(KEY, m.env_token()).unwrap_err();
        assert!(e.contains("DISABLED"), "{e}");
        m.set_enabled(KEY, true);
        assert!(kms.unwrap("data", &blob).is_ok());
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

    /// A keyring under a Cloud KMS root: its data keys only wrapped in the
    /// file, read back by another keyring, none once the version is
    /// disabled.
    #[test]
    fn a_keyring_under_a_cloud_kms_root() {
        let m = Mock::start();
        m.add_key(KEY, "ENCRYPT_DECRYPT");
        let d = std::env::temp_dir().join(format!("nucleus-gcp-kms-ring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let kv = || -> Arc<dyn nucleus_kv::Kv> {
            Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
        };
        let settings = || Settings {
            root: Root::Provider(Arc::new(Handle(Arc::new(
                GcpKms::new(KEY, m.env_token()).unwrap(),
            )))),
            ..Settings::default_for(db)
        };
        let ring = Keyring::new(db, settings(), kv());
        let dek = ring.dek("data", true).unwrap().unwrap();
        let file = std::fs::read_to_string(format!("{db}.keyring")).unwrap();
        assert!(file.contains(&format!("gcp-kms:{KEY}")), "{file}");
        assert!(!file.contains(&dek.to_hex()), "the data key in the clear");

        let held = Keyring::new(db, settings(), kv());
        assert_eq!(
            held.dek("data", false).unwrap().unwrap().bytes(),
            dek.bytes()
        );
        let later = Keyring::new(db, settings(), kv());
        m.set_enabled(KEY, false);
        let e = later.dek("data", false).unwrap_err();
        assert!(e.contains("FAILED_PRECONDITION"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
