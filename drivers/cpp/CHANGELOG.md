# Changelog

## Unreleased

- Live queries (nucleus PLAN.md §M16.6 S3): `db.subscribe(sql, params)`
  → `Subscription` — `rows()` (the result, kept on this side), `at()`,
  `changes(timeout_ms)` (the next batch as `Change{diff, row}`, applied),
  `close()`.
- Task queues (nucleus PLAN.md §M16.5 S7): `db.queue_of(name)` →
  `QueueOf` — `add(columns, values)` (its id), `take(limit, lease_ms)`
  (`Task`: id, attempt, values), `finish(task)`, `fail(task, error,
  retry_after_ms)` ("retry" or "dead"), `process(limit, lease_ms, work)`:
  each task done exactly once with its work, a failing one failed, a
  conflict run again, a lost lease left.
- Streams (nucleus PLAN.md §M16.5 S4, protocol version 10):
  `db.stream_of(name)` → `nucleus::StreamOf` — `append(columns,
  values)`, `consume(group, limit)` (in the connection's transaction),
  `position(group)`, `wait(after, timeout_ms)`, `process(group, limit,
  timeout_ms, work)`: a consumer group's next batch exactly once, the
  work's writes and the position committed together, a conflict run
  again, an exception rolling back; `db.wait_stream(name, after,
  timeout_ms)`.
- A collection's documents (nucleus PLAN.md §M16 Q4, protocol version
  9, through `nc_document`): `db.collection(name)` → `nucleus::Collection`
  — `find(filter, FindOptions)`, `find_one`, `count_documents`,
  `insert_one` / `insert_many`, `update_one` / `update_many` /
  `replace_one(filter, doc, upsert)` → `UpdateResult`, `delete_one` /
  `delete_many`; JSON texts in, `Json` out.
- A server's Unix socket (nucleus PLAN.md §M14.33 R2), through the C
  driver: a path as the host reaches `nucleus server --socket`.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `nucleus::ColumnKey(master, name)` — `encrypt`, `decrypt`,
  `blind_index`; `Connection::present_subject_key` and
  `hold_subject_key` (protocol version 8, the `k` message).
- json (nucleus PLAN.md §M14.15): `nucleus::Json`, a `json` value's
  text, both ways.
- Sparse vectors (nucleus PLAN.md §M14.12 V7): `nucleus::SparseVector`
  (dimensions, 0-based positions and values) in `Value`.
- Vectors (nucleus PLAN.md §M14.12): `nucleus::Vector`, a
  `std::vector<float>`, in `Value`.

- Pipelining, through the C driver: `Connection::pipeline(statements)`
  gives each statement's `Result` or `Error` in its place;
  `Connection::execute_many(sql, rows)` runs a statement for many rows,
  all or none.

- Bulk load, through the C driver: `Connection::copy(sql, data)` with a
  string or an `std::istream`.
- Protocol version 7 (nucleus PLAN.md §M14.11), through the C driver:
  `Connection::batches(sql, params, rows)` returns `Batches` — `next()`,
  `rows()`, `values<T>(col)`, `text(col, row)`, `is_null(col, row)`.
- Protocol version 6 (nucleus PLAN.md §M14.11), through the C driver:
  `Connection::cursor(sql, params, fetch)` returns a `Cursor` (a range of
  rows, or `next(row)`) read a batch at a time; destroyed or `close()`d
  early it stops the statement.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `Error::hint()` (empty when there is none).
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `Config::token`.
- SCRAM-SHA-256-PLUS over TLS, through the C driver.
- Client certificates: `Config::tls_cert`, `tls_key`.
- TLS through the C driver: `Config::tls`, `tls_ca`, `tls_no_verify`.
- Protocol version 4 (nucleus PLAN.md §M14.25), through the C driver: the
  password is proved by SCRAM-SHA-256, never sent.
- Protocol version 3 (nucleus PLAN.md §M14.26): `SmallInt`, `Integer`,
  `Real`, `Bytes`, `Time`, `Inet` and `Cidr` values.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
