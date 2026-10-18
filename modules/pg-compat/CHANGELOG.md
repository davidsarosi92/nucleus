# Changelog

## Unreleased

- `smallint` and `integer` are their own types in nucleus now: sent as
  `int2` (oid 21) and `int4` (oid 23), in text and binary (checked
  against PostgreSQL 18's `int2send` / `int4send`); binary parameters of
  those types are range-checked.

## 0.1.0 — 2026-10-17

- Moved out of `nucleus-sql` (`pgcatalog`) and `nucleus-server` (`pg`)
  into this module, built on the public API only (nucleus PLAN.md
  §M14.19).  `pg_catalog`, `information_schema` and the `pg_*` functions
  now exist only in sessions whose registry has `PgCompat`.
