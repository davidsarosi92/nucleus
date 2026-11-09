# Changelog

## Unreleased

- Sparse vectors (nucleus PLAN.md §M14.12 V7): a `sparsevec` value is a
  `SparseVector` (`dims`, 0-based `indices`, `values`;
  `SparseVector.of(map, dims)`), sent and read in rows and batches.
- Vectors (nucleus PLAN.md §M14.12): a `vector` value is a
  `Float32Array`, sent and read in rows, batches and `load`.

- Pipelining (nucleus PLAN.md §M14.20 W8): `Client.pipeline(statements)`
  sends statements without waiting for each reply and gives each one's
  `Result` or `NucleusError` in its place, `PIPELINE_WINDOW` bytes at a
  time; `Client.executeMany(sql, rows)` runs a statement for many rows,
  all or none, in one transaction of their own outside a block.

- Faster reads of large results (nucleus PLAN.md §M14.20 W1): a frame's
  chunks are joined once, when the frame is whole, instead of at every
  chunk, which made a large frame's read quadratic in its size.
- Bulk load (nucleus PLAN.md §M14.11): `Client.load(table, rows |
  columns, { columns })` as columnar batches in one `COPY … FROM STDIN`;
  `Client.copy(sql, data)` with text or CSV (a string, bytes, or an
  (async) iterable such as a read stream), waiting while the connection
  is full.  A plain query answers a COPY with a refusal.
- Protocol version 7 (nucleus PLAN.md §M14.11): columnar results.
  `Client.batches(sql, params, { rows })` returns `Batches` of `Batch`es,
  each column a typed array read in place (`ColumnBatch.values`,
  `offsets`, `valid`, `isNull`, `get`, `toArray`; `Batch.toRows`).
  `Column` has the protocol's type `tag`.
- Protocol version 6 (nucleus PLAN.md §M14.11): results stream.
  `Client.stream(sql, params, { fetch })` returns a `Stream` to read with
  `for await`, a batch at a time; leaving the loop, `close()` or the next
  query stops the statement.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `NucleusError.hint` (`null` when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `connect({ token })`.
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS) when the
  server offers it.
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
