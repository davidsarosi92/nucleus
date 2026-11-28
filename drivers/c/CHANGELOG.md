# Changelog

## Unreleased

- Live queries (nucleus PLAN.md §M16.6 S3): `nc_subscribe(c, name, sql,
  params, n, err)`, `nc_changes(c, name, timeout_ms, err)` — results of
  `(nc_ts, nc_diff, columns...)`, the first result and then each batch of
  changes, which the caller applies — and `nc_unsubscribe(c, name, err)`.
- Task queues (nucleus PLAN.md §M16.5 S7): `nc_queue_process(c, queue,
  limit, lease_ms, work, arg, err)`: tasks taken and each done exactly
  once — the work (`nc_task_work`, 0 to finish), `FINISH` and the commit
  together; a refused task failed, a conflict run again, a lost lease
  left.
- Streams (nucleus PLAN.md §M16.5 S4, protocol version 10, the `w`
  message): `nc_wait_stream(db, stream, after, timeout_ms, &err)` waits
  for an entry past `after` (the server holds no transaction meanwhile)
  and returns the newest `seq`; `nc_stream_process(db, stream, group,
  limit, timeout_ms, work, arg, &err)` handles a consumer group's next
  batch exactly once — `work(db, entries, arg)` does its writes on the
  connection and returns 0 to commit them with the group's position;
  a conflict runs it again.
- A collection's documents (nucleus PLAN.md §M16 Q4, protocol version
  9, the `O` message): `nc_document(db, &request, &err)` with an
  `nc_document_request` (`NC_DOC_FIND`, `_COUNT`, `_INSERT`, `_UPDATE`,
  `_REPLACE`, `_DELETE`; collection, filter, body, sort as JSON texts,
  skip, limit, `NC_DOC_MANY` / `NC_DOC_UPSERT`) → an `nc_result`; in the
  connection's transaction.
- A server's Unix socket (nucleus PLAN.md §M14.33 R2): a path as
  `nc_config.host` — `"/tmp/nucleus.sock"` — reaches `nucleus server
  --socket` (not on Windows); no TLS there.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `nc_column_key_init`, `nc_encrypt`, `nc_decrypt`, `nc_blind_index` —
  format 1, XChaCha20-Poly1305 in C99.  Protocol version 8:
  `nc_present_subject_key` and `nc_hold_subject_key` (the `k` message,
  nucleus DOCS.md §8.40).
- json (nucleus PLAN.md §M14.15): `NC_JSON` (its text in `as.text`),
  `nc_json(text)`.
- A server's SCRAM first message asking for more than 1 000 000 PBKDF2
  iterations is refused as malformed (a server could otherwise hold the
  client in the key derivation for minutes; nucleus PLAN.md §M14.29 S2).

- A transport of the caller's (nucleus PLAN.md §M14.23 step 3):
  `nc_connect_io(cfg, io, err)` logs in over `nc_io` (`send`, `recv`,
  `close` and their context) instead of a socket — the embedded
  edition's session in the Swift package.

- Sparse vectors (nucleus PLAN.md §M14.12 V7): `NC_SPARSEVEC` values
  (`as.sparse`: dimensions, 0-based positions and values, their count),
  `nc_sparsevec(dims, indices, values, nnz)`.
- Vectors (nucleus PLAN.md §M14.12): `NC_VECTOR` values (`as.vector`: a
  `float` pointer and the dimensions), `nc_vector(components, dims)`; a
  batch's vector column is its floats with offsets.

- Pipelining (nucleus PLAN.md §M14.20 W8): `nc_pipeline` sends
  statements (`nc_statement`) without waiting for each reply and gives
  each one's result or error in its place, `NC_PIPELINE_WINDOW` bytes at
  a time; `nc_execute_many` runs a statement for many rows of parameters,
  all or none, in one transaction of their own outside a block.  A frame
  goes out in one `send`, and replies are read through a 64 KiB buffer
  (a frame was two `send`s and two `recv`s): a point read 27.5 → 25 µs.

- Bulk load (nucleus PLAN.md §M14.11): `nc_copy_start`, `nc_copy_write`,
  `nc_copy_end` and `nc_copy` for `COPY … FROM STDIN` with text or CSV.
  `nc_query` and cursors answer a COPY with a refusal.
- Protocol version 7 (nucleus PLAN.md §M14.11): columnar results.
  `nc_batches_open` (`NC_DEFAULT_BATCH`), read with `nc_cursor_next`;
  `nc_batch_rows`, `nc_batch_data`, `nc_batch_offsets`,
  `nc_batch_validity`, `nc_batch_is_null` point into the batch.
- Protocol version 6 (nucleus PLAN.md §M14.11): results stream.
  `nc_cursor_open`, `nc_cursor_next`, `nc_cursor_result`,
  `nc_cursor_stop`, `nc_cursor_close` read a statement's rows a batch at a
  time (`NC_DEFAULT_FETCH`); the next query or `nc_close` ends a cursor
  left open.
- Protocol version 5 (nucleus PLAN.md §M14.10): errors carry a hint,
  `nc_error.hint` (empty when there is none); `nc_error` grew by it.
- A directory's (LDAP) password through PLAIN, over TLS only.
- An identity token instead of the password: `cfg.token` (OAUTHBEARER).
- Over TLS, SCRAM bound to the session (SCRAM-SHA-256-PLUS,
  `tls-server-end-point` by OpenSSL) when the server offers it.
- Client certificates (mutual TLS): `cfg.tls_cert`, `cfg.tls_key`.
- TLS (nucleus PLAN.md §M14.25) in a build with `NC_WITH_OPENSSL` (`make
  TLS=openssl`): `cfg.tls`, `cfg.tls_ca`, `cfg.tls_no_verify`,
  `nc_tls_supported()`; the certificate checked against the system's CAs
  or a CA file, and the host name (or IP address).
- Protocol version 4 (nucleus PLAN.md §M14.25): the password is proved by
  SCRAM-SHA-256 (SHA-256, HMAC, PBKDF2 and base64 in the driver, the nonce
  from `/dev/urandom` or `rand_s`), never sent; the server's signature is
  checked.
- Protocol version 3 (nucleus PLAN.md §M14.26): `NC_SMALLINT`,
  `NC_INTEGER`, `NC_REAL`, `NC_BYTEA`, `NC_TIME`, `NC_INET`, `NC_CIDR`
  values (with constructors and `nc_inet_parse`), `NC_CHAR` and `NC_ENUM`
  columns.

## 0.1.0 — 2026-10-18

- Protocol version 2: `uuid` as the language's native uuid (a string in
  JavaScript), other registered types as their name and binary form.
- First version (nucleus PLAN.md §M14.8): every nucleus type both ways,
  parameters, errors with their SQLSTATE, transactions, passwords.
