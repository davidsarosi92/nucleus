//! OpenID Connect for nucleus (PLAN.md §M14.25): logins by identity tokens.
//!
//! An [`Oidc`] is a `nucleus_server::Bearer`: a client sends a JWT its
//! identity provider issued (the OAUTHBEARER mechanism), and the server
//! checks it — its signature against the issuer's keys (RS256/384/512,
//! PS256/384/512, ES256, ES384, EdDSA; never `none`), its issuer, its
//! audience, its expiry and start — then logs the session in as the role a
//! claim names (`sub` unless told otherwise), with the roles another claim
//! lists granted for the session, and every claim readable as
//! `current_setting('nucleus.claims.<name>')` (so row-level security can
//! use a tenant claim).
//!
//! The issuer's keys come from a JWKS document given at start
//! ([`Keys::Static`]) or from the issuer itself (OpenID Connect Discovery:
//! `<issuer>/.well-known/openid-configuration`, then its `jwks_uri`, over
//! HTTPS — plain HTTP only to a loopback address, for tests), fetched again
//! when a token names a key not yet known, at most once a minute.

mod http;
mod jwt;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use nucleus_kv::Kv;
use nucleus_server::Identity;

pub use jwt::{Jwk, Jwks};

/// Where the issuer's keys come from.
#[derive(Clone, Debug)]
pub enum Keys {
    /// These.
    Static(Jwks),
    /// The issuer's (discovery), fetched when needed.
    Discover,
}

/// A token check: who issues the tokens, for whom, and how they map to
/// roles.
#[derive(Debug)]
pub struct Oidc {
    issuer: String,
    audience: String,
    user_claim: String,
    role_claim: Option<String>,
    leeway: Duration,
    keys: Keys,
    /// Discovered keys, and when they were fetched.
    fetched: Mutex<Option<(Arc<Jwks>, SystemTime)>>,
}

impl Oidc {
    /// Tokens of `issuer` (their `iss`) for `audience` (in their `aud`),
    /// signed by `keys`; the user is the `sub` claim.
    pub fn new(issuer: &str, audience: &str, keys: Keys) -> Self {
        Oidc {
            issuer: issuer.trim_end_matches('/').to_string(),
            audience: audience.to_string(),
            user_claim: "sub".into(),
            role_claim: None,
            leeway: Duration::from_secs(60),
            keys,
            fetched: Mutex::new(None),
        }
    }

    /// The claim that names the role to log in as (`sub`,
    /// `preferred_username`, `email`, …).
    pub fn user_claim(mut self, claim: &str) -> Self {
        self.user_claim = claim.to_string();
        self
    }

    /// The claim that lists roles granted for the session (an array, or a
    /// string of names separated by spaces); only roles that exist and are
    /// not administrators are granted.
    pub fn role_claim(mut self, claim: &str) -> Self {
        self.role_claim = Some(claim.to_string());
        self
    }

    /// How far clocks may disagree (default a minute).
    pub fn leeway(mut self, leeway: Duration) -> Self {
        self.leeway = leeway;
        self
    }

    /// The key set to check a token against: the static one, or the
    /// discovered one — fetched again (at most once a minute) if `kid` is
    /// not in it.
    fn keys_for(&self, kid: Option<&str>, now: SystemTime) -> Result<Arc<Jwks>, String> {
        if let Keys::Static(k) = &self.keys {
            return Ok(Arc::new(k.clone()));
        }
        let mut cache = self.fetched.lock().map_err(|_| "a key fetch failed")?;
        if let Some((keys, at)) = &*cache {
            let fresh = now.duration_since(*at).unwrap_or_default() < Duration::from_secs(60);
            if keys.find(kid).is_some() || fresh {
                return Ok(Arc::clone(keys));
            }
        }
        let keys = Arc::new(self.discover()?);
        *cache = Some((Arc::clone(&keys), now));
        Ok(keys)
    }

    /// The issuer's keys, by OpenID Connect Discovery.
    fn discover(&self) -> Result<Jwks, String> {
        let meta = http::get_json(&format!("{}/.well-known/openid-configuration", self.issuer))?;
        if meta
            .get("issuer")
            .and_then(|i| i.as_str())
            .map(|i| i.trim_end_matches('/'))
            != Some(self.issuer.as_str())
        {
            return Err("the issuer's metadata names another issuer".into());
        }
        let uri = meta
            .get("jwks_uri")
            .and_then(|u| u.as_str())
            .ok_or("the issuer's metadata has no jwks_uri")?;
        Jwks::from_value(&http::get_json(uri)?)
    }
}

/// A claim as text: a string as it is, an array's items joined by commas,
/// anything else as JSON.
fn text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

fn seconds(v: Option<&serde_json::Value>) -> Option<i64> {
    v.and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

impl nucleus_server::Bearer for Oidc {
    fn describe(&self) -> String {
        format!("OIDC issuer {}", self.issuer)
    }

    fn verify(&self, kv: &dyn Kv, token: &str) -> Result<Identity, String> {
        let now = kv.wall_clock();
        let unverified = jwt::parse(token)?;
        let keys = self.keys_for(unverified.kid(), now)?;
        let claims = unverified.verify(&keys)?;
        let unix = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| "the clock is before 1970")?
            .as_secs() as i64;
        let leeway = self.leeway.as_secs() as i64;
        if claims
            .get("iss")
            .and_then(|i| i.as_str())
            .map(|i| i.trim_end_matches('/'))
            != Some(self.issuer.as_str())
        {
            return Err("it is not of this server's issuer".into());
        }
        let for_us = match claims.get("aud") {
            Some(serde_json::Value::String(a)) => *a == self.audience,
            Some(serde_json::Value::Array(a)) => {
                a.iter().any(|a| a.as_str() == Some(&self.audience))
            }
            _ => false,
        };
        if !for_us {
            return Err("it is not for this server's audience".into());
        }
        match seconds(claims.get("exp")) {
            Some(exp) if unix < exp + leeway => {}
            Some(_) => return Err("it has expired".into()),
            None => return Err("it has no expiry".into()),
        }
        if seconds(claims.get("nbf")).is_some_and(|nbf| unix + leeway < nbf) {
            return Err("it is not valid yet".into());
        }
        let user = claims
            .get(&self.user_claim)
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| format!("it has no {} claim", self.user_claim))?
            .to_string();
        let roles = match self.role_claim.as_ref().and_then(|c| claims.get(c)) {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|r| r.as_str().map(String::from))
                .collect(),
            Some(serde_json::Value::String(s)) => s.split_whitespace().map(String::from).collect(),
            _ => Vec::new(),
        };
        Ok(Identity {
            user,
            roles,
            claims: claims
                .iter()
                .map(|(k, v)| (k.clone(), text(v)))
                .collect::<BTreeMap<_, _>>(),
        })
    }
}
