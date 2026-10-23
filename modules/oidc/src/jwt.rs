//! JSON Web Tokens (RFC 7519) in compact form, their signatures (RFC 7515,
//! 7518, 8037) and key sets (RFC 7517).

use ring::signature;
use serde_json::{Map, Value};

/// base64url without padding (RFC 4648 §5), as JOSE writes it.
pub(crate) fn base64url(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return Err("not base64url".into()),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// A public key of a key set.
#[derive(Clone, Debug, PartialEq)]
pub struct Jwk {
    /// Its id (`kid`), which a token's header names.
    pub kid: Option<String>,
    /// The algorithm it is for, if it says (`alg`).
    pub alg: Option<String>,
    key: Key,
}

#[derive(Clone, Debug, PartialEq)]
enum Key {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// An uncompressed point (`04 x y`) on P-256 or P-384.
    Ec {
        curve: String,
        point: Vec<u8>,
    },
    Ed25519(Vec<u8>),
}

/// A key set (JWKS).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Jwks {
    /// Its keys (those of a kind it does not know left out).
    pub keys: Vec<Jwk>,
}

fn field<'a>(o: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    o.get(name).and_then(Value::as_str)
}

impl Jwks {
    /// From its JSON text.
    pub fn from_json(text: &str) -> Result<Jwks, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("bad JWKS: {e}"))?;
        Self::from_value(&v)
    }

    pub(crate) fn from_value(v: &Value) -> Result<Jwks, String> {
        let keys = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("a JWKS needs keys")?;
        let mut out = Vec::new();
        for k in keys {
            let Some(o) = k.as_object() else { continue };
            // Signing keys only.
            if field(o, "use").is_some_and(|u| u != "sig") {
                continue;
            }
            let b = |name: &str| -> Result<Vec<u8>, String> {
                base64url(field(o, name).ok_or_else(|| format!("a key lacks {name}"))?)
            };
            let key = match field(o, "kty") {
                Some("RSA") => Key::Rsa {
                    n: b("n")?,
                    e: b("e")?,
                },
                Some("EC") => {
                    let curve = field(o, "crv").unwrap_or_default().to_string();
                    let (x, y) = (b("x")?, b("y")?);
                    let size = match curve.as_str() {
                        "P-256" => 32,
                        "P-384" => 48,
                        _ => continue,
                    };
                    if x.len() != size || y.len() != size {
                        return Err("an EC key of the wrong size".into());
                    }
                    let mut point = vec![4];
                    point.extend(x);
                    point.extend(y);
                    Key::Ec { curve, point }
                }
                Some("OKP") if field(o, "crv") == Some("Ed25519") => Key::Ed25519(b("x")?),
                _ => continue,
            };
            out.push(Jwk {
                kid: field(o, "kid").map(String::from),
                alg: field(o, "alg").map(String::from),
                key,
            });
        }
        Ok(Jwks { keys: out })
    }

    /// The key `kid` names — or, for a token naming none, the only key.
    pub(crate) fn find(&self, kid: Option<&str>) -> Option<&Jwk> {
        match kid {
            Some(kid) => self.keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
            None if self.keys.len() == 1 => self.keys.first(),
            None => None,
        }
    }
}

/// A token, parsed but not yet trusted.
pub(crate) struct Unverified {
    header: Map<String, Value>,
    claims: Map<String, Value>,
    signed: String,
    signature: Vec<u8>,
}

/// Splits a compact JWT into its parts.
pub(crate) fn parse(token: &str) -> Result<Unverified, String> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a JWT".into());
    };
    let object = |part: &str| -> Result<Map<String, Value>, String> {
        match serde_json::from_slice(&base64url(part)?) {
            Ok(Value::Object(o)) => Ok(o),
            _ => Err("not a JWT".into()),
        }
    };
    Ok(Unverified {
        header: object(h)?,
        claims: object(c)?,
        signed: format!("{h}.{c}"),
        signature: base64url(s)?,
    })
}

impl Unverified {
    /// The key id its header names.
    pub(crate) fn kid(&self) -> Option<&str> {
        field(&self.header, "kid")
    }

    /// Its claims, if its signature is one of `keys`'.
    pub(crate) fn verify(self, keys: &Jwks) -> Result<Map<String, Value>, String> {
        let alg = field(&self.header, "alg").ok_or("it names no algorithm")?;
        let key = keys
            .find(self.kid())
            .ok_or("it is signed by a key the issuer does not have")?;
        if key.alg.as_deref().is_some_and(|a| a != alg) {
            return Err("its algorithm is not its key's".into());
        }
        let msg = self.signed.as_bytes();
        let sig = &self.signature;
        let ok = match (&key.key, alg) {
            (Key::Rsa { n, e }, _) => {
                let params: &signature::RsaParameters = match alg {
                    "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                    "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                    "RS512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                    "PS256" => &signature::RSA_PSS_2048_8192_SHA256,
                    "PS384" => &signature::RSA_PSS_2048_8192_SHA384,
                    "PS512" => &signature::RSA_PSS_2048_8192_SHA512,
                    _ => return Err(format!("{alg} is not an RSA algorithm")),
                };
                signature::RsaPublicKeyComponents { n, e }
                    .verify(params, msg, sig)
                    .is_ok()
            }
            (Key::Ec { curve, point }, _) => {
                let params: &dyn signature::VerificationAlgorithm = match (curve.as_str(), alg) {
                    ("P-256", "ES256") => &signature::ECDSA_P256_SHA256_FIXED,
                    ("P-384", "ES384") => &signature::ECDSA_P384_SHA384_FIXED,
                    _ => return Err(format!("{alg} does not go with a {curve} key")),
                };
                signature::UnparsedPublicKey::new(params, point)
                    .verify(msg, sig)
                    .is_ok()
            }
            (Key::Ed25519(x), "EdDSA" | "Ed25519") => {
                signature::UnparsedPublicKey::new(&signature::ED25519, x)
                    .verify(msg, sig)
                    .is_ok()
            }
            _ => return Err(format!("{alg} does not go with its key")),
        };
        if ok {
            Ok(self.claims)
        } else {
            Err("its signature does not verify".into())
        }
    }
}
