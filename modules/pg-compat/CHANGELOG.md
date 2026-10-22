# Changelog

## Unreleased

- `smallint` and `integer` are their own types in nucleus now: sent as
  `int2` (oid 21) and `int4` (oid 23), in text and binary (checked
  against PostgreSQL 18's `int2send` / `int4send`); binary parameters of
  those types are range-checked.
- Enums travel as text (oid 25): PostgreSQL gives each enum an oid of its
  own, which nucleus does not have yet; `pg_typeof` names them.
- `inet` (oid 869, array 1041) and `cidr` (oid 650, array 651), binary
  as PostgreSQL's (checked against `inet_send` / `cidr_send`); a binary
  cidr parameter with host bits is refused.
- `char(n)` is nucleus's own blank-padded type: `bpchar` (oid 1042, typmod
  n + 4) names it, and `format_type` shows `character(n)`.
- `time` (oid 1083, array 1183): binary as µs since midnight (checked
  against PostgreSQL 18).
- `bytea` (oid 17, array 1001): text in the hex form, binary as its
  bytes (checked against PostgreSQL 18).
- `real` is its own type: sent as `float4` (oid 700), binary checked
  against PostgreSQL 18's `float4send`; a binary double parameter for a
  real column is narrowed and range-checked.

## 0.1.0 — 2026-10-17

- Moved out of `nucleus-sql` (`pgcatalog`) and `nucleus-server` (`pg`)
  into this module, built on the public API only (nucleus PLAN.md
  §M14.19).  `pg_catalog`, `information_schema` and the `pg_*` functions
  now exist only in sessions whose registry has `PgCompat`.
