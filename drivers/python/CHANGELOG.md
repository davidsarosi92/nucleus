# Changelog

## Unreleased

- Protocol version 7 (nucleus PLAN.md §M14.11): columnar results.
  `Connection.batches(sql, params, rows)` yields `Batch`es whose
  `ColumnBatch`es become numpy arrays (`to_numpy()`, fixed widths without
  a copy; dates and timestamps as `datetime64`, NULLs masked) or Arrow
  arrays (`to_arrow()`, in place where Arrow's layout is the batch's);
  `Connection.numpy`, `arrow`, `pandas`, `polars` for a whole result.
  `Column` has the protocol's type `tag`.
- Protocol version 6 (nucleus PLAN.md §M14.11): results stream.
  `Connection.stream(sql, params, fetch)` iterates a statement's rows as
  they arrive, a batch at a time; closing it (or running another
  statement) stops the statement.  The DB-API cursor reads lazily through
  it: `Cursor.execute` runs one statement, `itersize` rows a batch, and a
  query's `rowcount` is -1 until its last row is read.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `Error.hint` (`None` when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
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
