# Changelog

## Unreleased

- A collection's documents (nucleus PLAN.md §M16 Q4, protocol version
  9, the `O` message): `conn.collection(name)` → `Collection` — `find(filter,
  projection, sort, skip, limit)`, `find_one`, `count_documents`,
  `insert_one` / `insert_many` (their `_id`s), `update_one` /
  `update_many` / `replace_one(filter, doc, upsert=)` → `UpdateResult`
  (`matched`, `modified`, `upserted_id`), `delete_one` / `delete_many`;
  dicts (or JSON texts) in, dicts out, a sort a dict or `(field, 1 | -1)`
  pairs; in the connection's transaction (the embedded edition too).
- The wheel carries `THIRD_PARTY.md` beside `COPYRIGHT`: the notices of
  the third-party code and data inside the embedded library (Snowball,
  BSD-3-Clause; Unicode tables, Unicode License v3).
- A date or timestamp Python cannot hold (before year 1, past 9999;
  the database has them) raised `OverflowError` in the middle of the
  replies, which stayed unread: the next query read the rest of this
  one's (`SELECT 42` came back empty, and the query after it got 42).
  Now it is a `DataError` (`22008`) once the replies are read — in a
  pipeline the statement's place holds it; a stream fails that row and
  goes on.
- `load` sends a `vector` column of float32 numpy arrays (a 2-D array,
  or a list of 1-D arrays of one dimension, without NULLs) as one block
  a batch: 900,000 SIFT vectors encoded in 0.4 s, not 2.2.
- A server's Unix socket (nucleus PLAN.md §M14.33 R2): a path as the
  host — `nucleus.connect("/tmp/nucleus.sock")` — reaches `nucleus server
  --socket`; no TLS there.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `nucleus.columns.ColumnKey(master, name)` — `encrypt`, `decrypt`,
  `blind_index` — format 1, XChaCha20-Poly1305 in plain Python.  Protocol
  version 8: `present_subject_key(subject, secret)` and
  `hold_subject_key` (the `k` message, nucleus DOCS.md §8.40).
- Licence: the driver stays MIT; the embedded edition's library in the
  wheel is AGPL-3.0-only — the wheel is `MIT AND AGPL-3.0-only`, with
  both licences and COPYRIGHT in its `dist-info` (the repository's
  COPYRIGHT).
- json (nucleus PLAN.md §M14.15): a `json` value arrives as what
  `json.loads` makes; a `dict` parameter goes as json, `nucleus.Json(x)`
  sends a list or a string as json; in batches too (Arrow: `string`).
- `recall` returns `as_of` too, the commit it read (nucleus PLAN.md
  §M14.24 R5): a citation is a memory's `id` and `as_of`.

- A server's SCRAM first message asking for more than 1 000 000 PBKDF2
  iterations is refused as malformed (a server could otherwise hold the
  client in the key derivation for minutes; nucleus PLAN.md §M14.29 S2).

- Memory stores (nucleus PLAN.md §M14.24): `Connection.remember(store,
  subject, content, kind=, confidence=, expires=, source=, replaces=)` →
  `(id, action)`, `recall(store, subject, query, k=10)` → dicts best
  first, `forget(store, id)` → whether there was one;
  `forget_subject(subject)` → `(erasure, rows, scrubbed, found)`
  (`FORGET SUBJECT` and its scrub, §M14.24 R2).

- The embedded edition (nucleus PLAN.md §M14.23): `connect("file:app.db")`,
  `connect(":memory:")`, `nucleus.open(path, options)` and
  `nucleus.Database(path, options).connect(user=…)` run the same API on a
  database in this process, through the engine's library (`ctypes`;
  `NUCLEUS_EMBEDDED_LIB` or beside the package); `build_wheel.py` makes the
  platform wheel.  `connect` takes connection strings:
  `nucleus://user:password@host:port/database?tls=true&tls_ca=…&token=…`.

- Sparse vectors (nucleus PLAN.md §M14.12 V7): a `sparsevec` value is a
  `nucleus.SparseVector` (`dims`, 0-based `indices`, `values`;
  `SparseVector.of(dict, dims)`), sent and read in rows and batches.
- Vectors (nucleus PLAN.md §M14.12): a `vector` value is an
  `array('f')`; one or a one-dimensional float32 numpy array is sent as
  a vector, in parameters, `executemany` and `load`.
- Vector columns in batches (§M14.12 V2b): `to_numpy()` an `(n, d)`
  float32 array over the batch's bytes (NULL rows masked), `to_arrow()`
  a `fixed_size_list<float32>`; a column whose vectors differ in
  dimension stays objects.

- Pipelining (nucleus PLAN.md §M14.20 W8): `Connection.pipeline(statements)`
  sends statements without waiting for each reply and gives each one's
  `Result` or `Error` (not raised) in its place, `PIPELINE_WINDOW` bytes
  at a time; `Connection.execute_many(sql, rows)` runs a statement for
  many rows, all or none, in one transaction of their own outside a
  block — and the DB-API `Cursor.executemany` is it (it ran a statement
  at a time, each committed alone).

- Text columns of a batch (`to_list`, `to_numpy`) are decoded once and
  cut, not value by value: a million short strings 450 → 100 ms.

- Bulk load (nucleus PLAN.md §M14.11): `Connection.load(table, data,
  columns=None)` sends a pandas DataFrame, a pyarrow Table, a dict of
  columns or rows as columnar batches in one `COPY … FROM STDIN` (numpy
  columns encoded without a Python object per value; pandas' missing
  values as NULL); `Connection.copy(sql, data)` sends text or CSV (bytes,
  str, a file, chunks).  A plain query answers a COPY with a refusal.
  SQLSTATE class 57 (cancelled, timed out) is `OperationalError`.
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
