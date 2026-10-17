# Changelog

## 0.1.0 — 2026-10-17

- Moved out of `nucleus-sql` (`pgcatalog`) and `nucleus-server` (`pg`)
  into this module, built on the public API only (nucleus PLAN.md
  §M14.19).  `pg_catalog`, `information_schema` and the `pg_*` functions
  now exist only in sessions whose registry has `PgCompat`.
