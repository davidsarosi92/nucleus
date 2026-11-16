//! A stand-in for Azure Key Vault, for tests (the `mock` feature): a key's
//! `get`, `encrypt` and `decrypt` (RSA-OAEP-256) on a loopback port, the
//! bearer token checked; Entra ID's token endpoint (a client secret or a
//! federated token) and a managed identity's endpoint.  Keys have
//! versions (rotation keeps the old ones) and can be disabled.  Not a
//! cipher: its "ciphertext" masks the plaintext with a pad of the key's
//! version — enough for a test to see no key in the clear, nothing more.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_sql::ext::sha256;
use serde_json::{Value, json};

use crate::{b64url, b64url_decode};

/// The token the stand-in hands out and takes.
pub const ACCESS_TOKEN: &str = "mock-access-token";
const TENANT: &str = "mock-tenant";
const CLIENT: &str = "mock-client";
const SECRET: &str = "mock-secret";
const FEDERATED: &str = "mock-federated-token";
const IDENTITY_HEADER: &str = "mock-identity-header";

struct MockKey {
    kty: String,
    ops: Vec<String>,
    enabled: bool,
    /// Its versions, the current last.
    versions: Vec<String>,
}

#[derive(Default)]
struct State {
    keys: BTreeMap<String, MockKey>,
    tokens: usize,
}

/// A stand-in Key Vault, serving until dropped with the process.
#[derive(Clone)]
pub struct Mock {
    /// `http://127.0.0.1:<port>`: the vault's URL.
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
        let s = Arc::clone(&state);
        std::thread::spawn(move || {
            for c in listener.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || serve(c, &s));
            }
        });
        Self { addr, state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A key `name` of type `kty` (`RSA`, `RSA-HSM`, `EC`) allowed `ops`.
    pub fn add_key(&self, name: &str, kty: &str, ops: &[&str]) {
        self.lock().keys.insert(
            name.to_string(),
            MockKey {
                kty: kty.to_string(),
                ops: ops.iter().map(|o| o.to_string()).collect(),
                enabled: true,
                versions: vec![format!("{:032x}", 1)],
            },
        );
    }

    /// A new version of key `name`, the old ones kept.
    pub fn rotate(&self, name: &str) {
        if let Some(k) = self.lock().keys.get_mut(name) {
            let n = k.versions.len() + 1;
            k.versions.push(format!("{n:032x}"));
        }
    }

    /// Enables or disables key `name`.
    pub fn set_enabled(&self, name: &str, enabled: bool) {
        if let Some(k) = self.lock().keys.get_mut(name) {
            k.enabled = enabled;
        }
    }

    /// Tokens handed out.
    pub fn tokens_issued(&self) -> usize {
        self.lock().tokens
    }

    /// Key `name`'s URL (`http://127.0.0.1:<port>/keys/<name>`).
    pub fn key_url(&self, name: &str) -> String {
        format!("{}/keys/{name}", self.addr)
    }

    fn env_of(vars: Vec<(&str, String)>) -> Env {
        let all: BTreeMap<String, String> =
            vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        Box::new(move |k| all.get(k).cloned())
    }

    /// The environment of a client with a service principal's secret, as
    /// variables.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            ("AZURE_AUTHORITY_HOST".into(), self.addr.clone()),
            ("AZURE_TENANT_ID".into(), TENANT.into()),
            ("AZURE_CLIENT_ID".into(), CLIENT.into()),
            ("AZURE_CLIENT_SECRET".into(), SECRET.into()),
        ]
    }

    /// A client with a service principal's secret.
    pub fn env_secret(&self) -> Env {
        let vars = self.env_vars();
        Box::new(move |k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    /// A client with a wrong secret.
    pub fn env_bad_secret(&self) -> Env {
        Self::env_of(vec![
            ("AZURE_AUTHORITY_HOST", self.addr.clone()),
            ("AZURE_TENANT_ID", TENANT.into()),
            ("AZURE_CLIENT_ID", CLIENT.into()),
            ("AZURE_CLIENT_SECRET", "not-the-secret".into()),
        ])
    }

    /// A client with workload identity's federated token, in a file
    /// written in `dir`.
    pub fn env_federated(&self, dir: &Path) -> Env {
        let file = dir.join("federated-token");
        std::fs::write(&file, FEDERATED).expect("the token written");
        Self::env_of(vec![
            ("AZURE_AUTHORITY_HOST", self.addr.clone()),
            ("AZURE_TENANT_ID", TENANT.into()),
            ("AZURE_CLIENT_ID", CLIENT.into()),
            ("AZURE_FEDERATED_TOKEN_FILE", file.display().to_string()),
        ])
    }

    /// A client with a managed identity's endpoint.
    pub fn env_identity(&self) -> Env {
        Self::env_of(vec![
            ("IDENTITY_ENDPOINT", format!("{}/msi/token", self.addr)),
            ("IDENTITY_HEADER", IDENTITY_HEADER.into()),
        ])
    }
}

fn pad(version: &str, b: &[u8]) -> Vec<u8> {
    let p = sha256(format!("{version}\0pad").as_bytes());
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

fn error(c: &mut TcpStream, status: u16, code: &str, message: &str) {
    answer(
        c,
        status,
        &json!({ "error": { "code": code, "message": message } }),
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
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let issue = |c: &mut TcpStream, expires: Value| {
        state.lock().unwrap_or_else(|e| e.into_inner()).tokens += 1;
        answer(
            c,
            200,
            &json!({ "access_token": ACCESS_TOKEN, "expires_in": expires, "token_type": "Bearer" }),
        );
    };

    if path == format!("/{TENANT}/oauth2/v2.0/token") && method == "POST" {
        let text = String::from_utf8_lossy(&body).into_owned();
        let f: BTreeMap<String, String> = text
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect();
        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
        let ok = get("grant_type") == "client_credentials"
            && get("client_id") == CLIENT
            && get("scope") == "https://vault.azure.net/.default"
            && (get("client_secret") == SECRET
                || (get("client_assertion_type")
                    == "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
                    && get("client_assertion") == FEDERATED));
        if !ok {
            return answer(
                &mut c,
                401,
                &json!({ "error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided." }),
            );
        }
        return issue(&mut c, json!(3599));
    }
    if path == "/msi/token" {
        let resource_ok = query.contains("resource=https%3A%2F%2Fvault.azure.net");
        if header("x-identity-header") != Some(IDENTITY_HEADER) || !resource_ok {
            return error(&mut c, 400, "invalid_request", "bad identity request");
        }
        // (The identity endpoints say how long in a string.)
        return issue(&mut c, json!("3599"));
    }

    if header("authorization") != Some(&format!("Bearer {ACCESS_TOKEN}")) {
        return error(
            &mut c,
            401,
            "Unauthorized",
            "AKV10000: Request is missing a Bearer or PoP token.",
        );
    }
    if !query.contains("api-version=7.4") {
        return error(&mut c, 400, "BadParameter", "api-version");
    }
    // /keys/<name>[/<version>][/<op>]
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segments.first() != Some(&"keys") || segments.len() < 2 {
        return error(&mut c, 404, "NotFound", path);
    }
    let name = segments[1];
    let (version, op) = match &segments[2..] {
        [] => (None, ""),
        [op] => (None, *op),
        [v, op] => (Some(*v), *op),
        _ => return error(&mut c, 404, "NotFound", path),
    };
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(k) = st.keys.get(name) else {
        drop(st);
        return error(
            &mut c,
            404,
            "KeyNotFound",
            &format!("A key with (name/id) {name} was not found in this key vault."),
        );
    };
    let (kty, ops, enabled) = (k.kty.clone(), k.ops.clone(), k.enabled);
    let current = k.versions.last().cloned().unwrap_or_default();
    let known = version.is_none_or(|v| k.versions.iter().any(|x| x == v));
    drop(st);
    if !known {
        return error(&mut c, 404, "KeyNotFound", "no such version");
    }
    let version = version.map_or(current, str::to_string);
    let kid = format!("https://mock.vault/keys/{name}/{version}");
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match (method, op) {
        ("GET", "") => answer(
            &mut c,
            200,
            &json!({
                "key": { "kid": kid, "kty": kty, "key_ops": ops },
                "attributes": { "enabled": enabled },
            }),
        ),
        ("POST", "encrypt" | "decrypt") if !enabled => error(
            &mut c,
            403,
            "Forbidden",
            &format!("Operation {op} is not allowed on a disabled key."),
        ),
        ("POST", "encrypt" | "decrypt") if !ops.iter().any(|o| o == op) => error(
            &mut c,
            403,
            "Forbidden",
            &format!("Operation {op} is not permitted on this key."),
        ),
        ("POST", "encrypt" | "decrypt") if req["alg"] != json!("RSA-OAEP-256") => {
            error(&mut c, 400, "BadParameter", "alg")
        }
        ("POST", "encrypt") => {
            let plain = req["value"]
                .as_str()
                .and_then(b64url_decode)
                .unwrap_or_default();
            answer(
                &mut c,
                200,
                &json!({ "kid": kid, "value": b64url(&pad(&version, &plain)) }),
            );
        }
        ("POST", "decrypt") => {
            let cipher = req["value"]
                .as_str()
                .and_then(b64url_decode)
                .unwrap_or_default();
            answer(
                &mut c,
                200,
                &json!({ "kid": kid, "value": b64url(&pad(&version, &cipher)) }),
            );
        }
        _ => error(&mut c, 400, "BadParameter", op),
    }
}
