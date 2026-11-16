//! A stand-in for Google Cloud KMS, for tests (the `mock` feature): a
//! key's `get`, `encrypt` and `decrypt` on a loopback port, the bearer
//! token checked; a token endpoint that checks a service account's
//! signed JWT (RS256) and a user's refresh token; a metadata server.
//! The additional authenticated data is bound into the ciphertext as an
//! AEAD binds it; versions can be disabled.  Not a cipher: its
//! "ciphertext" masks the plaintext with a pad of the key's name — enough
//! for a test to see no key in the clear, nothing more.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_sql::ext::{base64_decode, base64_encode, sha256};
use serde_json::{Value, json};

/// The token the stand-in hands out and takes.
pub const ACCESS_TOKEN: &str = "mock-access-token";
/// The stand-in service account.
pub const SA_EMAIL: &str = "nucleus-test@mock.invalid";
const CLIENT_ID: &str = "mock-client";
const CLIENT_SECRET: &str = "mock-client-secret";
const REFRESH_TOKEN: &str = "mock-refresh-token";

/// The stand-in service account's private key (PKCS#8): made for these
/// tests, guarding nothing.
pub const SA_PRIVATE_KEY: &str = "\
-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDcMPkEFpgrDyhb\n\
yZSX7EzJIhA7VbDQPeAOoIgqMgB/Wp8jfq+3SXeFkwlCBB+mPuq0EHcFhojUe1dK\n\
GYQ/9nSLp+DsCPWtf8m429zWfh3vvfD2aAj4Znh8J1RspUYliAC1G8XwL95y6HX5\n\
ohMSLEw77a/rlnDy3LaBuE56vgjJDkJV0/yZU13Se+QavijRmmVVzZOAzLI8Jr59\n\
DrYf+NYUEUQU66Z62N0PtCJervpvtzCanvauu/y81e7jAa2/lnCxCJ4L8NGufmt4\n\
2KINJzcZMR1+3RtkAJ3phHKQRt6t3pdewOLClwKLnaY+iqoC17B9DUV1iDBu5ERr\n\
PZHh5sr5AgMBAAECggEBANVA2OuMtnN9iqja4n8A7J0f0Him1UfZpVaFQ1sackDl\n\
k5rc53GsRYWokh7Nv5lS9hzOlXdUhNDFZBTwAZWxK87WGJCBXO9cBBJ1XbyfCH9Y\n\
hjcfKxDorWgQ70MKDpXq0kSVUW797YN8gEX1ImNHY83mtaqFWX7dJCUeS2O1CO0u\n\
hjlry49luMxwihb37J6lIkn+Te/bZG+yPQ2JbL7R7ls9qUYn6kc/STJbgoYl5UGF\n\
W82Rz1cu2qnJaiRWM+VWUgsugQBm50VGpA/XqJOu0zD+2Kd9rbXG3onfb9OgCunc\n\
oS7iNIaVxtC1CxQif3mMDOWB1uyKfHdvT0TI+SB2NcECgYEA8cz62yp0RDW/JJ9r\n\
hy5ybxlzO3hdVjjAVZxfIouducTqMBuyUkx++dRdlZw9ZzPv4lmH/2SSJL1AKO5N\n\
tDGfP2d4hJEq3h0/SaDFws30G+wOwTqhnGg85K1tlaEsVwSfi75ysyUSe6fHG4Gd\n\
PJu6SD/GrnrP5TOhsUX4YjdBPmMCgYEA6R8izna9QIQ96l2QsnQrYNjG638zlcoi\n\
BLlsjOZAumzKAQzWRRV3syYF/mT3f5XztvqAOxO0lLiEuIrN0bEHrtyAPW20sLor\n\
J26lIg6DKcLn2usF6cVH3lZ9wMkYNnB964HVHAbLAbvHt2wDe52prLOyoaI9hFW8\n\
QX7fXM0cEfMCgYEA1GnhpU6yPImKlL2HVIW0+Ww+Emw2clwAejSYTlyf/mXB5Okv\n\
yaRxlz67x4YtWtABV7KXCgBrceLezhYqJLXC04wuDycHpX/D7QZTNEKAw1anz4+W\n\
ETRw6eKAdgXUUc056yUu1bknd4F4I+3FF00mY/nCq9wvbYXh8T0qaf1oT0kCgYBe\n\
+XxouR9pRXckIHJvAb0ae7avAue1QuCRJCoBTV1ZljDGNMCm7JQTvI38ko3ywNjF\n\
11CewOx1GqeXt5UatRD4QIVOgX938KDcM8u7EEaHoQxsuAt0rhcVkg8xl7vxker7\n\
I636+RmD30kChVL17cnOvncsOrjtTTChTlsZp8aj2QKBgArjTem4GHzDdJJxhfL6\n\
VP0+Grk2ZTMe0X0bqjRJR6AqZGan6xEJhzm+C5MGQmtFL0tcJkAuTd7vNDsaDIRs\n\
A7DtwujXB5nrz7LRaBd+r3eABII+AfoKsQcL5k9qjAQlD8f90Dgf4s1lhoSAMyhc\n\
mTYAY9Eh5PWJ3JdnRkL9Vt86\n\
-----END PRIVATE KEY-----\n";

/// Its public key (PKCS#1 DER, base64), for checking what it signed.
const SA_PUBLIC_KEY: &str = concat!(
    "MIIBCgKCAQEA3DD5BBaYKw8oW8mUl+xMySIQO1Ww0D3gDqCIKjIAf1qfI36vt0l3hZMJQgQfpj7q",
    "tBB3BYaI1HtXShmEP/Z0i6fg7Aj1rX/JuNvc1n4d773w9mgI+GZ4fCdUbKVGJYgAtRvF8C/ecuh1",
    "+aITEixMO+2v65Zw8ty2gbhOer4IyQ5CVdP8mVNd0nvkGr4o0ZplVc2TgMyyPCa+fQ62H/jWFBFE",
    "FOumetjdD7QiXq76b7cwmp72rrv8vNXu4wGtv5ZwsQieC/DRrn5reNiiDSc3GTEdft0bZACd6YRy",
    "kEberd6XXsDiwpcCi52mPoqqAtewfQ1FdYgwbuREaz2R4ebK+QIDAQAB",
);

#[derive(Default)]
struct State {
    /// Key name → its purpose and whether its version is enabled.
    keys: BTreeMap<String, (String, bool)>,
    tokens: usize,
}

/// A stand-in Cloud KMS, serving until dropped with the process.
#[derive(Clone)]
pub struct Mock {
    /// `http://127.0.0.1:<port>`.
    pub addr: String,
    state: Arc<Mutex<State>>,
}

type Env = Box<dyn Fn(&str) -> Option<String>>;

impl Mock {
    /// A stand-in on a free loopback port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let addr = format!("http://{}", listener.local_addr().expect("bound"));
        let state = Arc::new(Mutex::new(State::default()));
        let (s, a) = (Arc::clone(&state), addr.clone());
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                let (s, a) = (Arc::clone(&s), a.clone());
                std::thread::spawn(move || serve(c, &s, &a));
            }
        });
        Self { addr, state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A key named `name` (`projects/…/cryptoKeys/…`) for `purpose`
    /// (`ENCRYPT_DECRYPT`, …), its primary version enabled.
    pub fn add_key(&self, name: &str, purpose: &str) {
        self.lock()
            .keys
            .insert(name.to_string(), (purpose.to_string(), true));
    }

    /// Enables or disables the key's primary version.
    pub fn set_enabled(&self, name: &str, enabled: bool) {
        if let Some(k) = self.lock().keys.get_mut(name) {
            k.1 = enabled;
        }
    }

    /// Tokens handed out by the token endpoint and the metadata server.
    pub fn tokens_issued(&self) -> usize {
        self.lock().tokens
    }

    fn env_with(&self, vars: Vec<(&str, String)>) -> Env {
        let mut all: BTreeMap<String, String> =
            vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        all.insert(
            "CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDKMS".into(),
            format!("{}/", self.addr),
        );
        Box::new(move |k| all.get(k).cloned())
    }

    /// The environment of a client given the token as it is, as
    /// variables.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            (
                "CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDKMS".into(),
                format!("{}/", self.addr),
            ),
            ("GOOGLE_OAUTH_ACCESS_TOKEN".into(), ACCESS_TOKEN.into()),
        ]
    }

    /// A client given the token as it is (`GOOGLE_OAUTH_ACCESS_TOKEN`).
    pub fn env_token(&self) -> Env {
        self.env_with(vec![("GOOGLE_OAUTH_ACCESS_TOKEN", ACCESS_TOKEN.into())])
    }

    /// A client given a token the stand-in refuses.
    pub fn env_bad_token(&self) -> Env {
        self.env_with(vec![("GOOGLE_OAUTH_ACCESS_TOKEN", "not-the-token".into())])
    }

    /// A client with a service account's key file, written in `dir`.
    pub fn env_service_account(&self, dir: &Path) -> Env {
        let file = dir.join("service-account.json");
        let c = json!({
            "type": "service_account",
            "client_email": SA_EMAIL,
            "private_key_id": "mock-key-1",
            "private_key": SA_PRIVATE_KEY,
            "token_uri": format!("{}/token", self.addr),
        });
        std::fs::write(&file, c.to_string()).expect("the credentials written");
        self.env_with(vec![(
            "GOOGLE_APPLICATION_CREDENTIALS",
            file.display().to_string(),
        )])
    }

    /// A client with a user's refresh token (as gcloud leaves it), in a
    /// file written in `dir`.
    pub fn env_user(&self, dir: &Path) -> Env {
        let file = dir.join("authorized-user.json");
        let c = json!({
            "type": "authorized_user",
            "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET,
            "refresh_token": REFRESH_TOKEN,
            "token_uri": format!("{}/token", self.addr),
        });
        std::fs::write(&file, c.to_string()).expect("the credentials written");
        self.env_with(vec![(
            "GOOGLE_APPLICATION_CREDENTIALS",
            file.display().to_string(),
        )])
    }

    /// A client on a machine whose metadata server is the stand-in.
    pub fn env_metadata(&self) -> Env {
        let host = self.addr.trim_start_matches("http://").to_string();
        self.env_with(vec![("GCE_METADATA_HOST", host)])
    }
}

fn tag(key: &str, aad: &str) -> [u8; 32] {
    sha256(format!("{key}\0{aad}").as_bytes())
}

fn pad(key: &str, b: &[u8]) -> Vec<u8> {
    let p = sha256(format!("{key}\0pad").as_bytes());
    b.iter().zip(p.iter().cycle()).map(|(x, y)| x ^ y).collect()
}

fn answer(c: &mut TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    let _ = write!(
        c,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn error(c: &mut TcpStream, status: u16, kind: &str, message: &str) {
    answer(
        c,
        status,
        &json!({ "error": { "code": status, "status": kind, "message": message } }),
    );
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut t = s.replace('-', "+").replace('_', "/");
    while !t.len().is_multiple_of(4) {
        t.push('=');
    }
    base64_decode(&t)
}

/// Whether `jwt` is the stand-in service account's, signed by its key,
/// asking for Cloud KMS at `aud`.
fn signed_by_the_account(jwt: &str, aud: &str) -> bool {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let (Some(claims), Some(sig), Some(public)) = (
        b64url_decode(parts[1]),
        b64url_decode(parts[2]),
        base64_decode(SA_PUBLIC_KEY),
    ) else {
        return false;
    };
    let signed = format!("{}.{}", parts[0], parts[1]);
    let key = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        public,
    );
    if key.verify(signed.as_bytes(), &sig).is_err() {
        return false;
    }
    let c: Value = serde_json::from_slice(&claims).unwrap_or(Value::Null);
    c["iss"] == json!(SA_EMAIL) && c["aud"] == json!(aud) && c["scope"] == json!(crate::SCOPE)
}

fn serve(mut c: TcpStream, state: &Mutex<State>, addr: &str) {
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
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    let issue = |c: &mut TcpStream| {
        state.lock().unwrap_or_else(|e| e.into_inner()).tokens += 1;
        answer(
            c,
            200,
            &json!({ "access_token": ACCESS_TOKEN, "expires_in": 3599, "token_type": "Bearer" }),
        );
    };

    if path == "/token" && method == "POST" {
        let text = String::from_utf8_lossy(&body).into_owned();
        let f: BTreeMap<String, String> = text
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect();
        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
        let ok = match get("grant_type") {
            "urn:ietf:params:oauth:grant-type:jwt-bearer" => {
                signed_by_the_account(get("assertion"), &format!("{addr}/token"))
            }
            "refresh_token" => {
                get("client_id") == CLIENT_ID
                    && get("client_secret") == CLIENT_SECRET
                    && get("refresh_token") == REFRESH_TOKEN
            }
            _ => false,
        };
        if !ok {
            return answer(
                &mut c,
                400,
                &json!({ "error": "invalid_grant", "error_description": "refused" }),
            );
        }
        return issue(&mut c);
    }
    if path == "/computeMetadata/v1/instance/service-accounts/default/token" {
        if header("metadata-flavor") != Some("Google") {
            return error(&mut c, 403, "FORBIDDEN", "Metadata-Flavor missing");
        }
        return issue(&mut c);
    }

    if header("authorization") != Some(&format!("Bearer {ACCESS_TOKEN}")) {
        return error(
            &mut c,
            401,
            "UNAUTHENTICATED",
            "Request had invalid authentication credentials.",
        );
    }
    let Some(resource) = path.strip_prefix("/v1/") else {
        return error(&mut c, 404, "NOT_FOUND", path);
    };
    let (name, op) = match resource.rsplit_once(':') {
        Some((n, op)) => (n.to_string(), op),
        None => (resource.to_string(), ""),
    };
    let key = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys
        .get(&name)
        .cloned();
    let Some((purpose, enabled)) = key else {
        return error(&mut c, 404, "NOT_FOUND", &format!("{name} not found."));
    };
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let aad = req["additionalAuthenticatedData"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let version = format!("{name}/cryptoKeyVersions/1");
    match (method, op) {
        ("GET", "") => answer(
            &mut c,
            200,
            &json!({
                "name": name,
                "purpose": purpose,
                "primary": {
                    "name": version,
                    "state": if enabled { "ENABLED" } else { "DISABLED" },
                },
            }),
        ),
        ("POST", "encrypt" | "decrypt") if !enabled => error(
            &mut c,
            400,
            "FAILED_PRECONDITION",
            &format!("{version} is not enabled, current state is: DISABLED."),
        ),
        ("POST", "encrypt") if purpose == "ENCRYPT_DECRYPT" => {
            let plain = req["plaintext"]
                .as_str()
                .and_then(base64_decode)
                .unwrap_or_default();
            let mut blob = tag(&name, &aad).to_vec();
            blob.extend_from_slice(&pad(&name, &plain));
            answer(
                &mut c,
                200,
                &json!({ "name": version, "ciphertext": base64_encode(&blob) }),
            );
        }
        ("POST", "decrypt") if purpose == "ENCRYPT_DECRYPT" => {
            let blob = req["ciphertext"]
                .as_str()
                .and_then(base64_decode)
                .unwrap_or_default();
            if blob.len() < 32 || blob[..32] != tag(&name, &aad) {
                return error(
                    &mut c,
                    400,
                    "INVALID_ARGUMENT",
                    "Decryption failed: the ciphertext is invalid.",
                );
            }
            answer(
                &mut c,
                200,
                &json!({ "plaintext": base64_encode(&pad(&name, &blob[32..])) }),
            );
        }
        _ => error(
            &mut c,
            400,
            "INVALID_ARGUMENT",
            &format!("unsupported {op}"),
        ),
    }
}
