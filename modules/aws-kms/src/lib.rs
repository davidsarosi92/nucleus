//! nucleus's root key kept in AWS KMS (PLAN.md §M14.30 K3): the data
//! keys of the keyring (`<dir>.keyring`) wrapped by KMS's `Encrypt` and
//! unwrapped by its `Decrypt` under a symmetric KMS key — the root never
//! leaves KMS, every unwrap is in its audit log (CloudTrail), and
//! disabling or deleting the key there makes the database unreadable.
//!
//! A key is named as the AWS CLI names one — its id, its ARN, an alias
//! (`alias/db`) or an alias's ARN — after the scheme: `--root-kms
//! aws-kms:alias/db`.  Opening asks KMS for the key (`DescribeKey`): its
//! ARN is the root's id in the keyring, so the alias or the ARN given
//! later is the same root; a key not enabled, or not for encryption, is
//! refused by name.  A data key is wrapped with its purpose as the
//! encryption context (`nucleus-purpose`), so KMS itself refuses to
//! unwrap it for another.
//!
//! Configured as the AWS SDKs and CLI are: `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` (or
//! `AWS_DEFAULT_REGION`), and `AWS_ENDPOINT_URL_KMS` or
//! `AWS_ENDPOINT_URL` for an endpoint other than AWS's (LocalStack, moto,
//! a VPC endpoint).  Requests go over `nucleus-http` (TLS but to a
//! loopback address), signed by Signature Version 4.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use nucleus_aws::Credentials;
use nucleus_server::keyring::{Key, KeyProvider, register_kms};
use nucleus_sql::ext::{base64_decode, base64_encode};
use serde_json::{Value, json};

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// The encryption context's name for a data key's purpose.
pub const PURPOSE: &str = "nucleus-purpose";

/// Lets `--root-kms aws-kms:<key>` use this module.
pub fn register() {
    register_kms("aws-kms", open);
}

/// The KMS key `key` names (an id, an ARN or an alias), with the
/// credentials and endpoint of the environment.
pub fn open(key: &str) -> Result<Arc<dyn KeyProvider>, String> {
    AwsKms::new(key, |k| std::env::var(k).ok()).map(|k| Arc::new(k) as Arc<dyn KeyProvider>)
}

/// A symmetric key in AWS KMS.
pub struct AwsKms {
    /// `https://kms.<region>.amazonaws.com`, or the one set.
    endpoint: String,
    region: String,
    creds: Credentials,
    /// The key as named.
    named: String,
    /// Its ARN, as KMS said.
    arn: String,
}

impl std::fmt::Debug for AwsKms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsKms")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("arn", &self.arn)
            .finish_non_exhaustive()
    }
}

impl AwsKms {
    /// The key `key` names, its settings read by `env` (the process's
    /// environment, or a test's), asked of KMS (`DescribeKey`).
    pub fn new(key: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        if key.is_empty() {
            return Err("no key named (aws-kms:<key id, ARN or alias/…>)".into());
        }
        let what = format!("aws-kms:{key}");
        let creds = Credentials::from_env(&env, &what)?;
        let region = nucleus_aws::region(&env);
        let endpoint = nucleus_aws::endpoint(&env, "KMS")?
            .unwrap_or_else(|| format!("https://kms.{region}.amazonaws.com"));
        let mut kms = Self {
            endpoint,
            region,
            creds,
            named: key.to_string(),
            arn: String::new(),
        };
        let r = kms.call("DescribeKey", &json!({ "KeyId": key }))?;
        let meta = &r["KeyMetadata"];
        let arn = meta["Arn"]
            .as_str()
            .ok_or("KMS's DescribeKey answered without the key's ARN")?;
        let state = meta["KeyState"].as_str().unwrap_or("");
        if state != "Enabled" {
            return Err(format!("the key {arn} is {state}, not Enabled"));
        }
        let usage = meta["KeyUsage"].as_str().unwrap_or("ENCRYPT_DECRYPT");
        if usage != "ENCRYPT_DECRYPT" {
            return Err(format!("the key {arn} is for {usage}, not ENCRYPT_DECRYPT"));
        }
        let spec = meta["KeySpec"]
            .as_str()
            .or_else(|| meta["CustomerMasterKeySpec"].as_str())
            .unwrap_or("SYMMETRIC_DEFAULT");
        if spec != "SYMMETRIC_DEFAULT" {
            return Err(format!("the key {arn} is {spec}, not a symmetric key"));
        }
        kms.arn = arn.to_string();
        Ok(kms)
    }

    /// The key's ARN.
    pub fn arn(&self) -> &str {
        &self.arn
    }

    /// One call of KMS's JSON API, signed; retried a few times when KMS or
    /// the network says to try again.  KMS's error by its type and
    /// message.
    fn call(&self, action: &str, body: &Value) -> Result<Value, String> {
        let body = body.to_string();
        let payload = nucleus_aws::payload_hash(body.as_bytes());
        let host = nucleus_aws::host_of(&self.endpoint).to_string();
        let target = format!("TrentService.{action}");
        let url = format!("{}/", self.endpoint);
        let signer = self.creds.signer("kms", &self.region);
        let mut wait = Duration::from_millis(250);
        let mut tries = 0;
        loop {
            tries += 1;
            let date = nucleus_aws::amz_date(SystemTime::now());
            let mut headers: Vec<(&str, &str)> = vec![
                ("content-type", "application/x-amz-json-1.1"),
                ("x-amz-date", &date),
                ("x-amz-target", &target),
            ];
            if let Some(t) = &self.creds.token {
                headers.push(("x-amz-security-token", t));
            }
            let auth = signer.authorization("POST", &host, "/", &[], &headers, &payload);
            headers.push(("Authorization", &auth));
            let r = nucleus_http::send(&nucleus_http::Request {
                method: "POST",
                url: &url,
                headers: &headers,
                body: Some(body.as_bytes()),
                timeout: Duration::from_secs(10),
                total: Some(Duration::from_secs(30)),
                limit: 1 << 20,
                agent: "nucleus-aws-kms",
            });
            let again = match &r {
                Err(e) => e.transient,
                Ok(r) => r.status == 500 || r.status == 503 || throttled(r),
            };
            if again && tries < 4 {
                std::thread::sleep(wait);
                wait *= 2;
                continue;
            }
            let r = r.map_err(|e| format!("KMS {action}: {}", e.message))?;
            let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            if r.status != 200 {
                let kind = v["__type"].as_str().unwrap_or("");
                let kind = kind.rsplit('#').next().unwrap_or(kind);
                let message = v["message"]
                    .as_str()
                    .or_else(|| v["Message"].as_str())
                    .unwrap_or("");
                return Err(format!(
                    "KMS {action} of {}: HTTP {} {kind} {message}",
                    if self.arn.is_empty() {
                        &self.named
                    } else {
                        &self.arn
                    },
                    r.status
                ));
            }
            return Ok(v);
        }
    }

    fn context(purpose: &str) -> Value {
        json!({ PURPOSE: purpose })
    }
}

fn throttled(r: &nucleus_http::Response) -> bool {
    r.status == 400 && r.body_text(4096).contains("ThrottlingException")
}

impl KeyProvider for AwsKms {
    fn id(&self) -> String {
        format!("aws-kms:{}", self.arn)
    }

    fn kind(&self) -> String {
        "kms:aws".into()
    }

    fn describe(&self) -> String {
        if self.named == self.arn {
            format!("the AWS KMS key {}", self.arn)
        } else {
            format!("the AWS KMS key {} ({})", self.named, self.arn)
        }
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        let r = self.call(
            "Encrypt",
            &json!({
                "KeyId": self.arn,
                "Plaintext": base64_encode(dek.bytes()),
                "EncryptionContext": Self::context(purpose),
            }),
        )?;
        r["CiphertextBlob"]
            .as_str()
            .and_then(base64_decode)
            .ok_or_else(|| "KMS's Encrypt answered without a ciphertext".into())
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        let r = self.call(
            "Decrypt",
            &json!({
                "KeyId": self.arn,
                "CiphertextBlob": base64_encode(blob),
                "EncryptionContext": Self::context(purpose),
            }),
        )?;
        let mut plain = r["Plaintext"]
            .as_str()
            .and_then(base64_decode)
            .ok_or("KMS's Decrypt answered without a plaintext")?;
        let bytes: Result<[u8; 32], _> = plain.as_slice().try_into();
        plain.fill(0);
        let mut bytes =
            bytes.map_err(|_| format!("KMS unwrapped {} bytes, not a key's 32", plain.len()))?;
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

    fn kms(m: &Mock, key: &str) -> Result<AwsKms, String> {
        AwsKms::new(key, m.env())
    }

    /// Named by alias or by ARN, the same root; a data key wrapped and
    /// unwrapped, for its purpose only; a wrong secret, a key not there,
    /// a key disabled — each refused by KMS's own words.
    #[test]
    fn data_keys_wrapped_by_kms() {
        let m = Mock::start();
        let arn = m.add_key("alias/db");
        let by_alias = kms(&m, "alias/db").unwrap();
        let by_arn = kms(&m, &arn).unwrap();
        assert_eq!(by_alias.id(), format!("aws-kms:{arn}"));
        assert_eq!(by_alias.id(), by_arn.id());
        assert_eq!(by_alias.kind(), "kms:aws");
        assert!(
            by_alias.describe().contains("alias/db"),
            "{}",
            by_alias.describe()
        );

        let dek = Key::new([0x42; 32]);
        let blob = by_alias.wrap("data", &dek).unwrap();
        assert_eq!(by_arn.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
        let e = by_arn.unwrap("backup", &blob).unwrap_err();
        assert!(e.contains("InvalidCiphertextException"), "{e}");
        let calls = m.calls();
        assert!(calls.iter().any(|c| c.starts_with("Encrypt ")), "{calls:?}");

        let wrong = |k: &str| match k {
            "AWS_SECRET_ACCESS_KEY" => Some("not-the-secret".to_string()),
            _ => m.env()(k),
        };
        let e = AwsKms::new("alias/db", wrong).unwrap_err();
        assert!(e.contains("InvalidSignatureException"), "{e}");
        let e = kms(&m, "alias/none").unwrap_err();
        assert!(e.contains("NotFoundException"), "{e}");
        assert!(kms(&m, "").is_err());
        let e = AwsKms::new("alias/db", |_: &str| None).unwrap_err();
        assert!(e.contains("AWS_ACCESS_KEY_ID"), "{e}");

        m.set_enabled(&arn, false);
        let e = by_arn.unwrap("data", &blob).unwrap_err();
        assert!(e.contains("DisabledException"), "{e}");
        let e = kms(&m, "alias/db").unwrap_err();
        assert!(e.contains("Disabled"), "{e}");
        m.set_enabled(&arn, true);
        assert!(by_arn.unwrap("data", &blob).is_ok());
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

    /// A keyring under a KMS root: its data keys only wrapped in the file,
    /// read back by another keyring naming the key by its ARN, and none
    /// once the key is disabled.
    #[test]
    fn a_keyring_under_a_kms_root() {
        let m = Mock::start();
        let arn = m.add_key("alias/ring");
        let d = std::env::temp_dir().join(format!("nucleus-aws-kms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let kv = || -> Arc<dyn nucleus_kv::Kv> {
            Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
        };
        let settings = |key: &str| Settings {
            root: Root::Provider(Arc::new(Handle(Arc::new(kms(&m, key).unwrap())))),
            ..Settings::default_for(db)
        };
        let ring = Keyring::new(db, settings("alias/ring"), kv());
        let dek = ring.dek("data", true).unwrap().unwrap();
        let file = std::fs::read_to_string(format!("{db}.keyring")).unwrap();
        assert!(file.contains(&format!("aws-kms:{arn}")), "{file}");
        assert!(!file.contains(&dek.to_hex()), "the data key in the clear");
        assert!(!std::path::Path::new(&format!("{db}.root-key")).exists());

        let again = Keyring::new(db, settings(&arn), kv());
        assert_eq!(
            again.dek("data", false).unwrap().unwrap().bytes(),
            dek.bytes()
        );

        let held = Keyring::new(db, settings(&arn), kv());
        m.set_enabled(&arn, false);
        let e = held.dek("data", false).unwrap_err();
        assert!(e.contains("DisabledException"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
