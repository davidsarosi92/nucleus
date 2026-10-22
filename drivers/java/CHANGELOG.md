# Changelog

## Unreleased

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
