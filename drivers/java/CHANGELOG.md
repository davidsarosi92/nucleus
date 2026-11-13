# Changelog

## Unreleased

- Android (nucleus PLAN.md §M14.23 step 3): the driver uses no API newer
  than Android 8 (API 26) has, and `NucleusClient.connect(in, out,
  transport, config)` logs in over streams of the caller's — the
  embedded edition's session in the Android library (`drivers/android`).

- Sparse vectors (nucleus PLAN.md §M14.12 V7): a `sparsevec` value is a
  `SparseVector` (dimensions, 0-based positions and values;
  `SparseVector.of(map, dims)`), sent and read in rows and batches.
- Vectors (nucleus PLAN.md §M14.12): a `vector` value is a `float[]`,
  sent and read in rows, batches and `load` (a column of them as
  `float[][]`).

- Pipelining (nucleus PLAN.md §M14.20 W8):
  `NucleusClient.pipeline(List<Statement>)` sends statements without
  waiting for each reply and gives each one's `Reply` (its result or its
  error) in its place, `PIPELINE_WINDOW` bytes at a time;
  `executeMany(sql, rows)` runs a statement for many rows, all or none,
  in one transaction of their own outside a block.

- Bulk load (nucleus PLAN.md §M14.11): `NucleusClient.load(table, rows
  [, columns])` and `load(table, Map<String, ?>)` (lists, object or
  primitive arrays) as columnar batches in one `COPY … FROM STDIN`;
  `copy(sql, InputStream | String)` for text or CSV.  A plain query
  answers a COPY with a refusal.
- Protocol version 7 (nucleus PLAN.md §M14.11): columnar results.
  `NucleusClient.batches([rows,] sql, params…)` returns `Batches` of
  `Batch`es whose `ColumnBatch`es are buffers read in place (`shorts()`,
  `ints()`, `longs()`, `floats()`, `doubles()`, `bytes()`, `offset(i)`),
  with `isNull`, `get`, `toList`.  `Column` has the protocol's type `tag`.
- Protocol version 6 (nucleus PLAN.md §M14.11): results stream.
  `NucleusClient.cursor([fetch,] sql, params…)` returns a `Cursor`
  (`Iterable`, `AutoCloseable`) reading a statement's rows a batch at a
  time; closing it or the next query stops the statement.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `NucleusException.hint()` (null when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `Config.token(…)`.
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS) when the
  server offers it.
- Client certificates (mutual TLS): `Config.tlsCertificate(cert, key)`.
- TLS (nucleus PLAN.md §M14.25): `Config.tls(true)`, `tlsCa(path)`,
  `tlsVerify(false)`, over JSSE.
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (`Scram`, over `javax.crypto`), never sent; the server's
  signature is checked.
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
