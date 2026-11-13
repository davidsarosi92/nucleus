# Changelog

## Unreleased

- The issuer's metadata and keys are fetched within 20 seconds in all
  (`nucleus-http`'s `total`): a login waits for them (nucleus PLAN.md
  §M14.29 S2).

- The issuer's metadata and keys are fetched through `nucleus-http` (the
  HTTP client this module had, now a module of its own).
- First version (nucleus PLAN.md §M14.25): `Oidc`, a `nucleus_server::Bearer`
  checking JWTs (RS*, PS*, ES256, ES384, EdDSA) against a static JWKS or
  the issuer's (discovery); the user claim names the role, a role claim
  grants roles for the session, claims are read-only settings.
