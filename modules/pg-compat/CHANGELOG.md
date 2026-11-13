# Changelog

## Unreleased

- `handle_stream`: a client served over any byte stream (no TLS, no
  address), for connections made in the process and for fuzzing; found
  by fuzzing (nucleus PLAN.md §M14.29 S2): `websearch_to_tsquery` panicked
  on a text where a word was followed by a character of more than one
  byte.

- Generated columns as PostgreSQL's catalogs show them:
  `pg_attribute.attgenerated = 's'` (and `atthasdef`), the expression in
  `pg_attrdef`, `information_schema.columns.is_generated = 'ALWAYS'` and
  its `generation_expression` (new) — stored and embedding columns
  alike (§M14.22 E4).
- `tsvector` and `tsquery` on the wire as PostgreSQL's types (oids 3614,
  3615; `pg_type` rows with their arrays, `format_type`), in binary
  format as `tsvectorsend` / `tsquerysend` give them and taken as
  `tsvectorrecv` / `tsqueryrecv` take them (checked against 305 of
  PostgreSQL 18's); `tsvectorsend()` and `tsquerysend()` in SQL.  They
  were sent as `text` (oid 25).
- `<N>` takes distances up to 16,384, and the clean-up's distance across
  stop words is not capped, as PostgreSQL's (`cat <16384> the <-> dog`
  is `<16385>`; 16,383 before).
- The rewrite to a text index on a `column` table and a partitioned one.
- `ts_rank` and `ts_rank_cd` (`[weights,] vector, query
  [, normalization]`) as PostgreSQL computes them, to the bit;
  `ts_headline([cfg,] document, query [, options])` as its default
  parser's headline, every option (checked against 2,700 more of
  PostgreSQL 18's answers).
- PostgreSQL's text search: `tsvector` and `tsquery` (PostgreSQL's text
  forms), `to_tsvector`, `to_tsquery`, `plainto_tsquery`,
  `phraseto_tsquery`, `websearch_to_tsquery`, `@@` — evaluated as
  PostgreSQL's `TS_execute` (checked against 2,400 of PostgreSQL 18's
  answers); `to_tsvector(cfg, col) @@ maker(cfg, 'q')` rewritten to
  nucleus's `col @@ q` by the analyzer `cfg`, which a text index serves.
- `pg_statistic_ext` lists each `CREATE STATISTICS`;
  `pg_get_statisticsobjdef_columns` gives its columns (psql's `\d`).
- `COPY … FROM STDIN` (CopyInResponse, CopyData, CopyDone, CopyFail),
  from simple and extended queries; COPY data after an error is dropped.
- Results stream: rows are sent as the executor produces them (simple
  queries, and Execute without a row limit).
- A role whose password a directory checks (LDAP) logs in by the cleartext
  method over TLS only (`28000` without).
- SCRAM-SHA-256-PLUS over TLS (`tls-server-end-point` channel binding),
  offered first; a `y,,` client is refused where binding is possible.
- `SSLRequest` is accepted when the server has TLS (`Config::tls`, the
  `nucleus-tls` module), read unbuffered so nothing sent before the
  handshake counts as inside it; SCRAM takes libpq's `y,,` header over TLS.
- Passwords by SCRAM-SHA-256 (SASL, nucleus PLAN.md §M14.25) instead of
  cleartext: the password never crosses the wire.
- `current_setting` is nucleus's own now (it reads `SET` values too); the
  module's copy is gone.
- `pg_roles` and `pg_auth_members` show the database's roles and
  memberships (nucleus's `nc_catalog.roles` / `role_members` under their
  PostgreSQL names); `pg_class.relowner` and `pg_get_userbyid` its owners.

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
