# Changelog

## Unreleased

- An identity token instead of the password: `connect(token=…)`.
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS) when the
  server offers it.
- Client certificates (mutual TLS): `connect(tls_cert=…, tls_key=…)`.
- TLS (nucleus PLAN.md §M14.25): `connect(tls=True, tls_ca=…,
  tls_verify=False)`, over `ssl`.
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (`hashlib`, `hmac`), never sent; the server's signature is
  checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `smallint` and `integer`
  as `int`, `real` as `float` (its shortest digits), `bytea` as `bytes`,
  `time` as `datetime.time`, `inet` and `cidr` as `ipaddress` interfaces
  and networks, `char(n)` and enums as `str` with their own column type
  names.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
