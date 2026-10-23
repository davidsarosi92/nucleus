# Changelog

## Unreleased

- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `NucleusException.hint()` (null when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `Config.token(…)`.
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS) when the
  server offers it.
- Client certificates (mutual TLS): `Config.tlsCertificate(cert, key)`.
- TLS (nucleus PLAN.md §M14.25): `Config.tls(true)`, `tlsCa(path)`,
  `tlsVerify(false)`, over JSSE.
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (`Scram`, over `javax.crypto`), never sent; the server's
  signature is checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `smallint` as `Short`,
  `integer` as `Integer`, `real` as `Float` (and those sent natively as
  parameters), `bytea` as `byte[]`, `time` as `LocalTime`, `inet` and
  `cidr` as the new `Inet` and `Cidr` records, `char(n)` and enums as
  `String` with their own column type names.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
