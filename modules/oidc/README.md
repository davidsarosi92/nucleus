# nucleus-oidc

OpenID Connect for [nucleus](https://github.com/davidsarosi92/nucleus):
logins by identity tokens (JWTs) an identity provider issues — single
sign-on for people, and for programs and AI agents that already hold a
token (nucleus PLAN.md §M14.25).  A module, so nucleus's core carries no
JWT or JSON library.

An **`Oidc`** is a `nucleus_server::Bearer`.  A client sends its token
through the OAUTHBEARER mechanism (nucleus DOCS.md §10), and the server
checks it:

- the signature, against the issuer's keys — RS256/384/512,
  PS256/384/512, ES256, ES384, EdDSA (never `none`; a key's `alg` must
  match the token's);
- `iss` is the issuer, `aud` names the audience, `exp` has not passed and
  `nbf` has (a minute of leeway, `leeway`);
- the user claim (`sub`, or `user_claim("preferred_username")`, …) names
  a role that exists and may log in — never a token's (`nct_…`) — even in
  an unclaimed database.

The session runs as that role; the roles a `role_claim("groups")` lists
are granted for the session (those that exist and are not
administrators), and every claim is readable — never settable — as
`current_setting('nucleus.claims.<name>')`, so a row-level security policy
can use a tenant claim.

The keys come from a JWKS document (`Keys::Static(Jwks::from_json(…))`)
or from the issuer (`Keys::Discover`: OpenID Connect Discovery over
HTTPS, plain HTTP only to a loopback address), fetched again when a token
names a key not yet known, at most once a minute.

```rust
use std::sync::Arc;
let oidc = nucleus_oidc::Oidc::new("https://accounts.example.com", "my-app", nucleus_oidc::Keys::Discover)
    .user_claim("email")
    .role_claim("groups");
let config = nucleus_server::Config::default().with_bearer(Arc::new(oidc));
```

The nucleus binary includes it: `nucleus server <dir> --oidc-issuer <url>
--oidc-audience <name> [--oidc-jwks <file>] [--oidc-user-claim <claim>]
[--oidc-role-claim <claim>]`, and `ncsql --token <jwt>` (or `NCTOKEN`).

## Built on the public API only

This folder is a module in nucleus's repository, built to leave it: it
depends on nucleus's public API crates (`nucleus-kv`, `nucleus-server`) by
version, reads no files outside this folder, and CI checks both — the
boundary check and the detachability test.

## Tests

`tests/oidc.rs` — a test issuer with RSA (`tests/fixtures/`), P-256 and
Ed25519 keys signs tokens: they log their subject in, with the session's
roles and claims and row-level security by a claim; expired, early, of
another issuer or audience, without expiry, for an unknown or non-login
role, badly signed, of the wrong algorithm, `alg: none`, of an unknown
key, or not a token at all, they are refused; and the keys are found by
discovery from an issuer on a loopback address.

## Licence

MIT — see [LICENSE](LICENSE).
