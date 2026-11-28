# libnucleus — the C driver

C99, no dependencies (POSIX sockets; Winsock on Windows — not yet tested in
CI).  `nucleus.h` documents the API: `nc_connect`, `nc_query` (the last
statement's result, or NULL with an `nc_error` holding the SQLSTATE, the message and the
hint),
`nc_value_at`, `nc_format`, `nc_close`.  Values are `nc_value` (numeric as a
128-bit mantissa and scale, dates as days and timestamps as µs since
2000-01-01, `uuid` as an `nc_uuid` of 16 bytes — `nc_uuid_value`,
`nc_uuid_parse` —, `vector` as `NC_VECTOR` (a `float` pointer and the
dimensions; `nc_vector`), `sparsevec` as `NC_SPARSEVEC` (`as.sparse`: the
dimensions, the 0-based positions and values, their count; `nc_sparsevec`),
`json` as `NC_JSON` (its text in `as.text`; `nc_json`)
and another registered type as `NC_EXT`, its name and
binary form; `nc_column_type_name` names a registered column type or an
enum).  Protocol version 3: `NC_SMALLINT` and `NC_INTEGER` (`as.integer`),
`NC_REAL` (`as.real4`), `NC_BYTEA` (a pointer and length), `NC_TIME` (µs
since midnight), `NC_INET` / `NC_CIDR` (family, mask length, address;
`nc_inet_parse`); `NC_CHAR` and `NC_ENUM` columns hold text values.
Protocol version 4: `nc_connect` proves the password by SCRAM-SHA-256
(with the driver's own SHA-256, HMAC, PBKDF2 and base64) and never sends
it; a server that cannot sign the exchange is refused (`08006`).  TLS
in a build with OpenSSL (`make TLS=openssl`, which defines
`NC_WITH_OPENSSL`; link with `pkg-config --libs openssl`): `cfg.tls = 1`
checks the server against the system's CAs, `cfg.tls_ca` against a PEM
file, `cfg.tls_no_verify = 1` not at all; `cfg.tls_cert` / `tls_key`
show a client certificate; `nc_tls_supported()` says whether the build
can.  `cfg.token` logs in with an OpenID Connect identity token instead of
the password.  Large results stream (protocol version 6):
`nc_cursor_open(db, sql, params, n, fetch, &err)`, then `nc_cursor_next`
(1 a row — row 0 of `nc_cursor_result` —, 0 the end, -1 an error), and
`nc_cursor_close`; the server sends `fetch` rows at a time and waits until
they are read, and closing early (`nc_cursor_stop`, `nc_cursor_close`, or
the next query) stops the statement.  Columnar results (protocol version
7): `nc_batches_open(db, sql, params, n, rows, &err)`, then
`nc_cursor_next` a batch at a time; `nc_batch_rows`, `nc_batch_data`
(a pointer to a column's values: `int32_t *`, `double *`, …),
`nc_batch_offsets` and `nc_batch_validity` point into the batch.  Bulk
load: `nc_copy(db, "COPY t FROM STDIN", data, len, &err)`, or
`nc_copy_start` / `nc_copy_write` (pieces of any size) / `nc_copy_end` —
text or CSV, as the statement says.  Many statements in one round trip:
`nc_pipeline(db, stmts, n, results, errors, &err)` (`nc_statement`s;
each one's result, or NULL and its error); one statement for many rows,
all or none, one commit for all: `nc_execute_many(db, sql, rows, nrows,
nparams, &err)`.

```sh
make                # build/libnucleus.a
make test           # build/test_nucleus (needs a running server)
```

Or compile `nucleus.c` into your program.  Example: `examples/c/quickstart.c`.

## Encrypted columns

A column's values encrypted here and decrypted here: the server stores
`bytea` ciphertext and never holds the key; a blind index beside it finds
equal values (nucleus PLAN.md §M14.30 K5, DOCS.md §11 — what it leaves
visible is said there).  `master` is 32 bytes the application keeps.

```c
nc_column_key k;
uint8_t sealed[15 + NC_COLUMN_OVERHEAD], index[NC_BLIND_INDEX_LEN];
nc_column_key_init(&k, master, "users.email");
nc_encrypt(&k, NC_PLAIN_TEXT, "ann@example.com", 15, NULL, sealed);   /* a bytea to send */
nc_blind_index(&k, NC_PLAIN_TEXT, "ann@example.com", 15, index);     /* WHERE email_index = $1 */
/* back: nc_decrypt(&k, v->as.bytes.ptr, v->as.bytes.len, &type, out, &len) */
```

A sealed table's rows whose key their user holds: `nc_present_subject_key(db, subject, secret, &err)`.

## Documents

A collection's documents by MongoDB's requests (`CREATE COLLECTION`;
nucleus DOCS.md §8.41, §10 *Documents*), in the connection's transaction:

```c
nc_document_request q = {0};
q.op = NC_DOC_FIND;
q.collection = "people";
q.filter = "{\"age\": {\"$gte\": 30}}";
q.sort = "{\"age\": -1}";
q.limit = 10;
nc_result *r = nc_document(db, &q, &err);   /* rows of one json column, doc */
```

## Streams

A stream's entries (`CREATE STREAM`; nucleus DOCS.md §8.43): appended in a
transaction, numbered by its commit; a consumer group's next batch
handled exactly once — the work's writes and the group's position
committed together, a conflict with another consumer run again; a wait
for the next entry that holds no transaction on the server:

```c
static int work(nc_conn *db, const nc_result *entries, void *arg) {
    /* seq first; the writes on db; 0 commits them with the position */
    return 0;
}
/* ... */
int64_t n = nc_stream_process(db, "jobs", "workers", 100, 30000, work, NULL, &err);
int64_t newest = nc_wait_stream(db, "jobs", 0, 1000, &err);
```

## Task queues

A task queue's tasks (`CREATE QUEUE`; nucleus DOCS.md §8.45): added (a
row), taken under a lease, each done exactly once — the work's writes and
the task's `FINISH` committed together; a task whose work fails is failed
into a retry after the backoff or, past its attempts, a dead letter; one
whose lease ran out while it worked is left to the worker that took it
again:

```c
/* row `row` of tasks: id, attempt, then the queue's columns; 0 finishes */
static int work(nc_conn *c, const nc_result *tasks, size_t row, void *arg) { ... }

int64_t finished = nc_queue_process(db, "jobs", 10, 0 /* the queue's lease */, work, NULL, &err);
```

Adding, taking, finishing and failing by hand are statements: `INSERT`,
`TAKE FROM jobs LIMIT n`, `FINISH TASK id OF jobs ATTEMPT n` (in the
work's transaction), `FAIL TASK id OF jobs ATTEMPT n ERROR 'why'`.

## Live queries

A query's result as the commits change it (nucleus DOCS.md §8.47): rows
of `(nc_ts, nc_diff, columns...)`, the first result each `+1`, then each
batch of changes for the caller to apply.

```c
nc_value p = nc_bool(0);
nc_result *r = nc_subscribe(db, "open_tasks", "SELECT id, title FROM tasks WHERE done = $1", &p, 1, &err);
/* ... the first result ... */
nc_result_free(r);
r = nc_changes(db, "open_tasks", 30000, &err);   /* waits up to 30 s */
/* each row: nc_ts, nc_diff (+n came, -n went), then the columns */
nc_result_free(r);
nc_unsubscribe(db, "open_tasks", &err);
```

## Licence

MIT — see [LICENSE](LICENSE).  The database itself is AGPL-3.0-only; a
program that reaches a nucleus server through this driver is not bound by
it (the repository's [COPYRIGHT](../../COPYRIGHT)).
