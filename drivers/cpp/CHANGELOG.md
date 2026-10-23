# Changelog

## Unreleased

- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `Error::hint()` (empty when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `Config::token`.
- SCRAM-SHA-256-PLUS over TLS, through the C driver.
- Client certificates: `Config::tls_cert`, `tls_key`.
- TLS through the C driver: `Config::tls`, `tls_ca`, `tls_no_verify`.
- Protocol version 4 (nucleus PLAN.md §M14.25), through the C driver: the
  password is proved by SCRAM-SHA-256, never sent.
- Protocol version 3 (nucleus PLAN.md §M14.26): `SmallInt`, `Integer`,
  `Real`, `Bytes`, `Time`, `Inet` and `Cidr` values.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
