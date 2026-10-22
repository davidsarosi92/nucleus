# Changelog

## Unreleased

- Protocol version 3 (nucleus PLAN.md §M14.26): `NC_SMALLINT`,
  `NC_INTEGER`, `NC_REAL`, `NC_BYTEA`, `NC_TIME`, `NC_INET`, `NC_CIDR`
  values (with constructors and `nc_inet_parse`), `NC_CHAR` and `NC_ENUM`
  columns.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
