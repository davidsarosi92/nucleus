# Changelog

## Unreleased

- Client certificates (mutual TLS): `connect({ tlsCert, tlsKey })`.
- TLS (nucleus PLAN.md §M14.25): `connect({ tls, tlsCa, tlsVerify })`, over
  `node:tls`.
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (`node:crypto`), never sent; the server's signature is
  checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `smallint`, `integer` and
  `real` (its shortest digits) as `number`, `bytea` as `Buffer`, `time` as
  `Time`, `inet` and `cidr` as `Inet` and `Cidr`, `char(n)` and enums as
  `string` with their own column type names.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
