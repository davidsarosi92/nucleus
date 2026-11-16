//! A stand-in for AWS KMS, for tests (the `mock` feature): `DescribeKey`,
//! `Encrypt` and `Decrypt` of its JSON API on a loopback port, every
//! request's Signature Version 4 checked as KMS checks it, the encryption
//! context bound into the ciphertext as KMS binds it, keys that can be
//! disabled.  Not a cipher: its "ciphertext" masks the plaintext with a
//! pad of the key's id — enough for a test to see no key in the clear,
//! nothing more.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use nucleus_sql::ext::{base64_decode, base64_encode, sha256};
use serde_json::{Value, json};

/// The stand-in's access key id.
pub const KEY_ID: &str = "mock-key-id";
/// The stand-in's secret access key.
pub const SECRET: &str = "mock-secret";
/// The stand-in's region.
pub const REGION: &str = "eu-central-1";
const ACCOUNT: &str = "111122223333";

#[derive(Clone, Debug)]
struct MockKey {
    id: String,
    alias: String,
    enabled: bool,
}

impl MockKey {
    fn arn(&self) -> String {
        format!("arn:aws:kms:{REGION}:{ACCOUNT}:key/{}", self.id)
    }
}

#[derive(Default)]
struct State {
    keys: Vec<MockKey>,
    /// Every call answered: its action and the key's id.
    calls: Vec<String>,
}

/// A stand-in KMS, serving until dropped with the process.
#[derive(Clone)]
pub struct Mock {
    /// `http://127.0.0.1:<port>`.
    pub endpoint: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    /// A stand-in on a free loopback port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let endpoint = format!("http://{}", listener.local_addr().expect("bound"));
        let state = Arc::new(Mutex::new(State::default()));
        let s = Arc::clone(&state);
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || serve(c, &s));
            }
        });
        Self { endpoint, state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new key known also as `alias` (`alias/…`); its ARN.
    pub fn add_key(&self, alias: &str) -> String {
        let mut st = self.lock();
        let k = MockKey {
            id: format!(
                "{:08x}-0000-4000-8000-{:012x}",
                st.keys.len() + 1,
                st.keys.len() + 1
            ),
            alias: alias.to_string(),
            enabled: true,
        };
        let arn = k.arn();
        st.keys.push(k);
        arn
    }

    /// Enables or disables the key `arn`.
    pub fn set_enabled(&self, arn: &str, enabled: bool) {
        for k in &mut self.lock().keys {
            if k.arn() == arn {
                k.enabled = enabled;
            }
        }
    }

    /// The calls answered so far (`Action key-id`).
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    /// The environment a client of it needs, as variables.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        [
            ("AWS_ACCESS_KEY_ID", KEY_ID),
            ("AWS_SECRET_ACCESS_KEY", SECRET),
            ("AWS_REGION", REGION),
            ("AWS_ENDPOINT_URL_KMS", self.endpoint.as_str()),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    /// [`env_vars`](Self::env_vars) as a lookup, for
    /// [`AwsKms::new`](crate::AwsKms::new).
    pub fn env(&self) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: BTreeMap<String, String> = self.env_vars().into_iter().collect();
        move |k| vars.get(k).cloned()
    }
}

fn find<'a>(st: &'a mut State, key: &str) -> Option<&'a mut MockKey> {
    let key = key
        .strip_prefix(&format!("arn:aws:kms:{REGION}:{ACCOUNT}:"))
        .unwrap_or(key);
    st.keys
        .iter_mut()
        .find(|k| key == k.id || key == format!("key/{}", k.id) || key == k.alias)
}

/// The mock's "ciphertext": the key, a check binding key and context, the
/// plaintext masked by [`pad`].
fn tag(id: &str, context: &Value) -> [u8; 32] {
    sha256(format!("{id}\0{context}").as_bytes())
}

/// `b` masked (or unmasked) by a pad of the key's id.
fn pad(id: &str, b: &[u8]) -> Vec<u8> {
    let p = sha256(format!("{id}\0pad").as_bytes());
    b.iter().zip(p.iter().cycle()).map(|(x, y)| x ^ y).collect()
}

fn answer(c: &mut TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let _ = write!(
        c,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/x-amz-json-1.1\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn error(c: &mut TcpStream, status: u16, kind: &str, message: &str) {
    answer(c, status, &json!({ "__type": kind, "message": message }));
}

fn serve(mut c: TcpStream, state: &Mutex<State>) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    let head_end = loop {
        let Ok(n) = c.read(&mut buf) else { return };
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let header = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    let len: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < len {
        let Ok(n) = c.read(&mut buf) else { return };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let method = first.split(' ').next().unwrap_or("");

    // The signature, as KMS checks it.
    let auth = header("authorization").unwrap_or("");
    let field = |name: &str| {
        auth.split([',', ' '])
            .find_map(|p| p.trim().strip_prefix(name))
            .unwrap_or("")
            .to_string()
    };
    let credential = field("Credential=");
    let signed = field("SignedHeaders=");
    let scope: Vec<&str> = credential.split('/').collect();
    if scope.len() != 5 || scope[0] != KEY_ID || scope[2] != REGION || scope[3] != "kms" {
        return error(
            &mut c,
            400,
            "UnrecognizedClientException",
            "bad credential scope",
        );
    }
    let host = header("host").unwrap_or("").to_string();
    let others: Vec<(&str, &str)> = signed
        .split(';')
        .filter(|h| *h != "host")
        .map(|h| (h, header(h).unwrap_or("")))
        .collect();
    let want = nucleus_aws::Signer {
        service: "kms",
        region: REGION,
        key_id: KEY_ID,
        secret: SECRET,
    }
    .authorization(
        method,
        &host,
        "/",
        &[],
        &others,
        &nucleus_aws::payload_hash(&body),
    );
    if want != auth {
        return error(
            &mut c,
            400,
            "InvalidSignatureException",
            "the signature does not match",
        );
    }

    let action = header("x-amz-target")
        .and_then(|t| t.strip_prefix("TrentService."))
        .unwrap_or("")
        .to_string();
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let named = req["KeyId"].as_str().unwrap_or("").to_string();
    let Some(key) = find(&mut st, &named).cloned() else {
        drop(st);
        return error(
            &mut c,
            400,
            "NotFoundException",
            &format!("key {named} not found"),
        );
    };
    st.calls.push(format!("{action} {}", key.id));
    drop(st);
    let context = req
        .get("EncryptionContext")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match action.as_str() {
        "DescribeKey" => answer(
            &mut c,
            200,
            &json!({ "KeyMetadata": {
                "KeyId": key.id,
                "Arn": key.arn(),
                "KeyState": if key.enabled { "Enabled" } else { "Disabled" },
                "KeyUsage": "ENCRYPT_DECRYPT",
                "KeySpec": "SYMMETRIC_DEFAULT",
            }}),
        ),
        _ if !key.enabled => error(
            &mut c,
            400,
            "DisabledException",
            &format!("{} is disabled.", key.arn()),
        ),
        "Encrypt" => {
            let plain = req["Plaintext"]
                .as_str()
                .and_then(base64_decode)
                .unwrap_or_default();
            let mut blob = key.id.as_bytes().to_vec();
            blob.push(0);
            blob.extend_from_slice(&tag(&key.id, &context));
            blob.extend_from_slice(&pad(&key.id, &plain));
            answer(
                &mut c,
                200,
                &json!({ "CiphertextBlob": base64_encode(&blob), "KeyId": key.arn() }),
            );
        }
        "Decrypt" => {
            let blob = req["CiphertextBlob"]
                .as_str()
                .and_then(base64_decode)
                .unwrap_or_default();
            let at = blob.iter().position(|&b| b == 0).unwrap_or(blob.len());
            let ok = blob.len() >= at + 33
                && blob[..at] == *key.id.as_bytes()
                && blob[at + 1..at + 33] == tag(&key.id, &context);
            if !ok {
                return error(&mut c, 400, "InvalidCiphertextException", "");
            }
            answer(
                &mut c,
                200,
                &json!({
                    "Plaintext": base64_encode(&pad(&key.id, &blob[at + 33..])),
                    "KeyId": key.arn(),
                }),
            );
        }
        _ => error(&mut c, 400, "UnknownOperationException", &action),
    }
}
