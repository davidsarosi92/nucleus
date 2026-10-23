# Changelog

## Unreleased

- First version (nucleus PLAN.md §M14.25): `Oidc`, a `nucleus_server::Bearer`
  checking JWTs (RS*, PS*, ES256, ES384, EdDSA) against a static JWKS or
  the issuer's (discovery); the user claim names the role, a role claim
  grants roles for the session, claims are read-only settings.
