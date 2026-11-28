# Changelog

## Unreleased

- Task queues (nucleus PLAN.md §M16.5 S7): `db.queueOf(name)` →
  `QueueOf` — `add(Map)` (its id), `take(limit, leaseMs)`, `finish(task)`,
  `fail(task, error, retryAfterMs)` ("retry" or "dead"), `process(limit,
  leaseMs, work)`: each task done exactly once with its work, a failing
  one failed, a conflict run again, a lost lease left.
- Streams (nucleus PLAN.md §M16.5 S4, protocol version 10, the `w`
  message): `db.streamOf(name)` → `StreamOf` — `append(Map)`,
  `consume(group, limit)` (in the connection's transaction),
  `position(group)`, `await(after, timeoutMs)`, `process(group, limit,
  timeoutMs, work)`: a consumer group's next batch exactly once, the
  work's writes and the position committed together, a conflict run
  again, an exception rolling back; `db.waitStream(name, after,
  timeoutMs)` (the server holds no transaction while it waits; Android
  too).
- A collection's documents (nucleus PLAN.md §M16 Q4, protocol version
  9, the `O` message): `db.collection(name)` → `DocumentCollection` —
  `find(filter[, projection, sort, skip, limit])`, `findOne`,
  `countDocuments`, `insertOne` / `insertMany` (their `_id`s),
  `updateOne` / `updateMany` / `replaceOne(filter, doc, upsert)` →
  `UpdateResult(matched, modified, upsertedId)`, `deleteOne` /
  `deleteMany`; JSON texts in, `Json` out; in the connection's
  transaction (Android too).
- A server's Unix socket (nucleus PLAN.md §M14.33 R2): a path as the
  host — `new Config().host("/tmp/nucleus.sock")` — reaches `nucleus
  server --socket` (Java 16's Unix-domain channels, looked up when a
  path is used, so the driver still builds for Android, which has none);
  no TLS there.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `new ColumnKey(master, name)` — `encrypt`, `decrypt`, `blindIndex` —
  format 1, XChaCha20-Poly1305 in plain Java (Android 8 too).  Protocol
  version 8: `presentSubjectKey(subject, secret)` and `holdSubjectKey`
  (the `k` message, nucleus DOCS.md §8.40).
- json (nucleus PLAN.md §M14.15): `Json`, a `json` value's text, both
  ways (and in `load`).
- A server's SCRAM first message asking for more than 1 000 000 PBKDF2
  iterations is refused as malformed (a server could otherwise hold the
  client in the key derivation for minutes; nucleus PLAN.md §M14.29 S2).

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
