//! OpenID Connect's identity tokens (`nucleus-oidc`): a token as a client
//! sends it, before anything about it is trusted (its three parts, their
//! base64 and JSON, the key and algorithm it names, its signature); the
//! claims of a token the issuer did sign (its times, issuer, audience and
//! the claims a session reads); and the issuer's key set.
#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use nucleus_kv::Kv;
use nucleus_oidc::{Jwks, Keys, Oidc};
use nucleus_server::Bearer;
use ring::signature::{Ed25519KeyPair, KeyPair};

fn b64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

struct Issuer {
    key: Ed25519KeyPair,
    oidc: Oidc,
    kv: Arc<dyn Kv>,
}

fn issuer() -> &'static Issuer {
    static I: OnceLock<Issuer> = OnceLock::new();
    I.get_or_init(|| {
        let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("a key");
        let jwks = format!(
            r#"{{"keys": [{{"kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": "{}"}},
                {{"kty": "RSA", "kid": "rsa", "alg": "RS256", "n": "{}", "e": "AQAB"}}]}}"#,
            b64(key.public_key().as_ref()),
            b64(&[0xC5; 256]),
        );
        let keys = Jwks::from_json(&jwks).expect("the issuer's keys");
        let oidc =
            Oidc::new("https://issuer.example", "nucleus", Keys::Static(keys)).role_claim("roles");
        let kv = nucleus_fuzz::database();
        let mut s = nucleus_sql::Session::new(kv.clone());
        s.execute("CREATE USER alice; CREATE ROLE reader")
            .expect("the roles");
        Issuer { key, oidc, kv }
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let i = issuer();
    match mode % 3 {
        // A token as a client sends it.
        0 => {
            let _ = i.oidc.verify(&*i.kv, text);
        }
        // The issuer's signature on the fuzzer's header and claims.
        1 => {
            let (h, c) = nucleus_fuzz::split(rest);
            let signed = format!("{}.{}", b64(h), b64(c));
            let sig = i.key.sign(signed.as_bytes());
            let _ = i
                .oidc
                .verify(&*i.kv, &format!("{signed}.{}", b64(sig.as_ref())));
        }
        // An issuer's key set.
        _ => {
            if let Ok(keys) = Jwks::from_json(text) {
                let oidc = Oidc::new("https://issuer.example", "nucleus", Keys::Static(keys));
                let _ = oidc.verify(&*i.kv, "eyJhbGciOiJFUzI1NiIsImtpZCI6ImsifQ.e30.AAAA");
            }
        }
    }
});
