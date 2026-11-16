//! A stand-in for Vault's Transit engine, for tests (the `mock` feature):
//! reading a key, `encrypt` and `decrypt` on a loopback port, the token
//! checked (and revocable), the associated data bound into the ciphertext
//! as an AEAD binds it.  Not a cipher: its "ciphertext" masks the
//! plaintext with a pad of the key's name — enough for a test to see no
//! key in the clear, nothing more.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use nucleus_sql::ext::{base64_decode, base64_encode, sha256};
use serde_json::{Value, json};

/// The stand-in's token.
pub const TOKEN: &str = "mock-token";

#[derive(Default)]
struct State {
    /// (mount, key) → the key's type.
    keys: BTreeMap<(String, String), String>,
    revoked: bool,
}

/// A stand-in Transit engine, serving until dropped with the process.
#[derive(Clone)]
pub struct Mock {
    /// `http://127.0.0.1:<port>`.
    pub addr: String,
    state: Arc<Mutex<State>>,
}

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

    /// A key `key` of type `ty` (`aes256-gcm96`, …) in the engine at
    /// `mount`.
    pub fn add_key(&self, mount: &str, key: &str, ty: &str) {
        self.lock()
            .keys
            .insert((mount.to_string(), key.to_string()), ty.to_string());
    }

    /// Revokes the token: every request refused from now on.
    pub fn revoke(&self) {
        self.lock().revoked = true;
    }

    /// The environment a client of it needs, as variables.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            ("VAULT_ADDR".to_string(), self.addr.clone()),
            ("VAULT_TOKEN".to_string(), TOKEN.to_string()),
        ]
    }

    /// [`env_vars`](Self::env_vars) as a lookup, for
    /// [`VaultTransit::new`](crate::VaultTransit::new).
    pub fn env(&self) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: BTreeMap<String, String> = self.env_vars().into_iter().collect();
        move |k| vars.get(k).cloned()
    }
}

fn tag(key: &str, ad: &str) -> [u8; 32] {
    sha256(format!("{key}\0{ad}").as_bytes())
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

fn errors(c: &mut TcpStream, status: u16, errors: &[&str]) {
    answer(c, status, &json!({ "errors": errors }));
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
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    if st.revoked || header("x-vault-token") != Some(TOKEN) {
        drop(st);
        return errors(&mut c, 403, &["permission denied", "invalid token"]);
    }
    // /v1/<mount>/<op>/<key>
    let Some(rest) = path.strip_prefix("/v1/") else {
        drop(st);
        return errors(&mut c, 404, &[]);
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() < 3 {
        drop(st);
        return errors(&mut c, 404, &[]);
    }
    let key = segments[segments.len() - 1].to_string();
    let op = segments[segments.len() - 2];
    let mount = segments[..segments.len() - 2].join("/");
    let Some(ty) = st.keys.get(&(mount, key.clone())).cloned() else {
        drop(st);
        return errors(&mut c, 404, &[]);
    };
    drop(st);
    let aead = ["aes256-gcm96", "aes128-gcm96", "chacha20-poly1305"].contains(&ty.as_str());
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let ad = req["associated_data"].as_str().unwrap_or("").to_string();
    match (method, op) {
        ("GET", "keys") => answer(
            &mut c,
            200,
            &json!({ "data": {
                "name": key,
                "type": ty,
                "derived": false,
                "supports_encryption": aead,
                "supports_decryption": aead,
                "latest_version": 1,
            }}),
        ),
        ("POST", "encrypt") if aead => {
            let plain = req["plaintext"]
                .as_str()
                .and_then(base64_decode)
                .unwrap_or_default();
            let mut blob = tag(&key, &ad).to_vec();
            blob.extend_from_slice(&pad(&key, &plain));
            answer(
                &mut c,
                200,
                &json!({ "data": {
                    "ciphertext": format!("vault:v1:{}", base64_encode(&blob)),
                    "key_version": 1,
                }}),
            );
        }
        ("POST", "decrypt") if aead => {
            let blob = req["ciphertext"]
                .as_str()
                .and_then(|c| c.strip_prefix("vault:v1:"))
                .and_then(base64_decode)
                .unwrap_or_default();
            if blob.len() < 32 || blob[..32] != tag(&key, &ad) {
                return errors(&mut c, 400, &["cipher: message authentication failed"]);
            }
            answer(
                &mut c,
                200,
                &json!({ "data": { "plaintext": base64_encode(&pad(&key, &blob[32..])) } }),
            );
        }
        _ => errors(&mut c, 400, &[&format!("unsupported operation {op}")]),
    }
}
