//! OpenID Connect logins end to end: a test issuer's keys (RSA, P-256,
//! Ed25519), tokens it signs, a server that takes them, and the Rust client
//! logging in with them — and with tokens it must not take.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nucleus_client::{Client, Config, Error, Value};
use nucleus_kv::{Database, Kv, Options};
use nucleus_oidc::{Jwks, Keys, Oidc};
use ring::rand::SystemRandom;
use ring::signature::{self, KeyPair};
use serde_json::json;

const ISSUER: &str = "https://issuer.test";
const AUDIENCE: &str = "nucleus-tests";

fn b64(b: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let v = chunk.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(A[(v >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A test identity provider: three keys, and tokens it signs.
struct Issuer {
    rsa: signature::RsaKeyPair,
    p256: signature::EcdsaKeyPair,
    ed: signature::Ed25519KeyPair,
}

impl Issuer {
    fn new() -> Issuer {
        let rng = SystemRandom::new();
        let p256 = signature::EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let ed = signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Issuer {
            rsa: signature::RsaKeyPair::from_der(include_bytes!("fixtures/rsa-test-key.der"))
                .unwrap(),
            p256: signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                p256.as_ref(),
                &rng,
            )
            .unwrap(),
            ed: signature::Ed25519KeyPair::from_pkcs8(ed.as_ref()).unwrap(),
        }
    }

    fn jwks(&self) -> serde_json::Value {
        let rsa: ring::rsa::PublicKeyComponents<Vec<u8>> = self.rsa.public().into();
        let p = self.p256.public_key().as_ref();
        json!({"keys": [
            {"kty": "RSA", "kid": "rsa", "alg": "RS256", "use": "sig",
             "n": b64(&rsa.n),
             "e": b64(&rsa.e)},
            {"kty": "EC", "kid": "p256", "crv": "P-256", "x": b64(&p[1..33]), "y": b64(&p[33..])},
            {"kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": b64(self.ed.public_key().as_ref())},
            {"kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB"},
        ]})
    }

    /// A token signed with the key `kid` by `alg`.
    fn sign(&self, alg: &str, kid: &str, claims: &serde_json::Value) -> String {
        let header = json!({"alg": alg, "kid": kid, "typ": "JWT"});
        let signed = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(claims.to_string().as_bytes())
        );
        let rng = SystemRandom::new();
        let sig = match kid {
            "rsa" => {
                let mut s = vec![0; self.rsa.public().modulus_len()];
                self.rsa
                    .sign(
                        &signature::RSA_PKCS1_SHA256,
                        &rng,
                        signed.as_bytes(),
                        &mut s,
                    )
                    .unwrap();
                s
            }
            "p256" => self
                .p256
                .sign(&rng, signed.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            _ => self.ed.sign(signed.as_bytes()).as_ref().to_vec(),
        };
        format!("{signed}.{}", b64(&sig))
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Claims for `sub`, valid for an hour.
fn claims(sub: &str) -> serde_json::Value {
    json!({"iss": ISSUER, "aud": [AUDIENCE, "other"], "sub": sub,
           "exp": now() + 3600, "iat": now(), "tenant": "acme", "groups": ["readers", "root"]})
}

fn start(oidc: Oidc) -> SocketAddr {
    // A real database (and so the real clock: the tokens' times are real).
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nucleus-oidc-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db: Arc<dyn Kv> = Arc::new(Database::open(&dir, Options::default()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = nucleus_server::Config::default().with_bearer(Arc::new(oidc));
    std::thread::spawn(move || nucleus_server::serve(listener, db, config));
    addr
}

fn with_token(addr: SocketAddr, token: &str) -> Result<Client, Error> {
    Client::connect(Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        user: String::new(),
        password: None,
        token: Some(token.into()),
        ..Config::default()
    })
}

fn code(r: Result<Client, Error>) -> String {
    match r {
        Err(Error::Server(e)) => e.code,
        Err(e) => format!("{e}"),
        Ok(_) => "ok".into(),
    }
}

fn setup(addr: SocketAddr) {
    let mut admin = Client::connect(Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ..Config::default()
    })
    .unwrap();
    for sql in [
        "CREATE USER alice",
        "CREATE ROLE readers",
        "CREATE ROLE root SUPERUSER",
        "CREATE TABLE notes (id int, tenant text)",
        "INSERT INTO notes VALUES (1, 'acme'), (2, 'globex')",
        "GRANT SELECT ON notes TO readers",
        "ALTER TABLE notes ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY by_tenant ON notes USING (tenant = current_setting('nucleus.claims.tenant'))",
    ] {
        admin.execute(sql, &[]).unwrap();
    }
}

#[test]
fn tokens_log_their_subject_in() {
    let issuer = Issuer::new();
    let jwks = Jwks::from_json(&issuer.jwks().to_string()).unwrap();
    assert_eq!(jwks.keys.len(), 3, "the encryption key is left out");
    let addr = start(Oidc::new(ISSUER, AUDIENCE, Keys::Static(jwks)).role_claim("groups"));
    setup(addr);
    for (alg, kid) in [("RS256", "rsa"), ("ES256", "p256"), ("EdDSA", "ed")] {
        let mut c = with_token(addr, &issuer.sign(alg, kid, &claims("alice"))).unwrap();
        assert_eq!(
            c.rows(
                "SELECT current_user, current_setting('nucleus.claims.tenant')",
                &[]
            )
            .unwrap(),
            vec![vec![Value::from("alice"), Value::from("acme")]],
            "{alg}"
        );
        // The groups claim grants readers (an administrator role it
        // names is not granted); row-level security sees the tenant.
        assert_eq!(
            c.rows("SELECT id FROM notes", &[]).unwrap(),
            vec![vec![Value::Integer(1)]]
        );
        assert!(c.execute("CREATE ROLE x", &[]).is_err());
        // Claims cannot be changed.
        let err = c
            .execute("SET nucleus.claims.tenant = 'globex'", &[])
            .unwrap_err();
        assert!(matches!(err, Error::Server(e) if e.code == "55P02"));
    }
}

#[test]
fn bad_tokens_are_refused() {
    let issuer = Issuer::new();
    let jwks = Jwks::from_json(&issuer.jwks().to_string()).unwrap();
    let addr = start(Oidc::new(ISSUER, AUDIENCE, Keys::Static(jwks)));
    setup(addr);
    let refused =
        |claims: serde_json::Value| code(with_token(addr, &issuer.sign("ES256", "p256", &claims)));
    let mut c = claims("alice");
    c["exp"] = json!(now() - 3600);
    assert_eq!(refused(c), "28000", "expired");
    let mut c = claims("alice");
    c["nbf"] = json!(now() + 3600);
    assert_eq!(refused(c), "28000", "not yet valid");
    let mut c = claims("alice");
    c["iss"] = json!("https://elsewhere.test");
    assert_eq!(refused(c), "28000", "another issuer");
    let mut c = claims("alice");
    c["aud"] = json!("someone-else");
    assert_eq!(refused(c), "28000", "another audience");
    let mut c = claims("alice");
    c.as_object_mut().unwrap().remove("exp");
    assert_eq!(refused(c), "28000", "no expiry");
    // A role that does not exist (in an unclaimed database too, whose
    // bootstrap would take any name), or may not log in.
    assert_eq!(refused(claims("mallory")), "28000");
    assert_eq!(refused(claims("readers")), "28000");
    // A bad signature, an algorithm its key is not for, alg none, an
    // unknown key, garbage.
    let good = issuer.sign("ES256", "p256", &claims("alice"));
    let (signed, sig) = good.rsplit_once('.').unwrap();
    let flipped = format!(
        "{signed}.{}{}",
        if sig.starts_with('A') { "B" } else { "A" },
        &sig[1..]
    );
    assert_eq!(code(with_token(addr, &flipped)), "28000");
    assert_eq!(
        code(with_token(
            addr,
            &issuer.sign("RS256", "p256", &claims("alice"))
        )),
        "28000"
    );
    let none = format!(
        "{}.{}.",
        b64(br#"{"alg":"none"}"#),
        b64(claims("alice").to_string().as_bytes())
    );
    assert_eq!(code(with_token(addr, &none)), "28000");
    assert_eq!(
        code(with_token(
            addr,
            &issuer.sign("EdDSA", "nope", &claims("alice"))
        )),
        "28000"
    );
    assert_eq!(code(with_token(addr, "not.a.token")), "28000");
    // The password still works alongside.
    assert!(
        Client::connect(Config {
            host: addr.ip().to_string(),
            port: addr.port(),
            ..Config::default()
        })
        .is_ok()
    );
}

/// The keys found by OpenID Connect Discovery, from an issuer on a
/// loopback address (plain HTTP is allowed there only).
#[test]
fn keys_are_discovered() {
    let issuer = Issuer::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (meta, jwks) = (
        json!({"issuer": base, "jwks_uri": format!("{base}/keys")}).to_string(),
        issuer.jwks().to_string(),
    );
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut req = [0u8; 2048];
            let n = s.read(&mut req).unwrap();
            let path = String::from_utf8_lossy(&req[..n]);
            let body = if path.starts_with("GET /.well-known/openid-configuration ") {
                &meta
            } else {
                &jwks
            };
            // Chunked, as many servers send it.
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            );
        }
    });
    let addr = start(Oidc::new(&base, AUDIENCE, Keys::Discover));
    setup(addr);
    let mut c = claims("alice");
    c["iss"] = json!(base);
    let mut client = with_token(addr, &issuer.sign("RS256", "rsa", &c)).unwrap();
    assert_eq!(
        client.rows("SELECT current_user", &[]).unwrap(),
        vec![vec![Value::from("alice")]]
    );
    // An issuer on a non-loopback address must be HTTPS.
    let addr = start(Oidc::new("http://203.0.113.1", AUDIENCE, Keys::Discover));
    assert_eq!(
        code(with_token(addr, &issuer.sign("RS256", "rsa", &c))),
        "28000"
    );
}
