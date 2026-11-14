# nucleus — the Python driver

Pure Python (3.8+), no dependencies, DB-API 2.0 style — for a server,
or for a database inside this process (the embedded edition).

```python
import nucleus

conn = nucleus.connect()     # 127.0.0.1:20035, user/password "nucleus"
conn.execute("INSERT INTO t VALUES ($1, $2)", [1, "one"])
print(conn.execute("SELECT * FROM t").records())
with conn.transaction():
    ...
```

Types: `bool`, `int` (smallint, integer, bigint), `float` (real by its
shortest digits, double), `decimal.Decimal`, `str` (text, `char(n)`,
enums), `bytes` (bytea; `bytearray` too as a parameter), `datetime.date`,
`datetime.time` (24:00:00 as `time.max`), `datetime.datetime` (naive for
timestamp, UTC for timestamptz), `nucleus.Interval` (and `timedelta` as a
parameter), `ipaddress.ip_interface` (inet; an address as a parameter),
`ipaddress.ip_network` (cidr), `uuid.UUID`, `array('f')` (vector; a
float32 numpy array too as a parameter), `nucleus.SparseVector`
(sparsevec: `dims`, 0-based `indices`, `values`; `SparseVector.of(dict,
dims)`), json as what `json.loads` makes (a `dict` parameter is json;
`nucleus.Json(x)` sends a list or a string as json), `list`, `None`; another
registered type is `nucleus.Ext` (its name and binary form).  Protocol
version 4: the password is proved by SCRAM-SHA-256, never sent.  TLS:
`connect(tls=True)` (the system's CAs), `tls_ca="ca.pem"`,
`tls_verify=False` (encrypted, unverified); `tls_cert=`, `tls_key=` for
a client certificate; `token=` for an identity token instead of the
password.
Errors are `nucleus.Error` subclasses with `.code` (SQLSTATE) and `.hint`
(what to do about it, or `None`).  Large results stream (protocol version
6): `with conn.stream(sql, params, fetch=1000) as rows: for row in rows:`
reads them as they arrive, the server sending a batch at a time and
waiting until it is read; closing early stops the statement, and a DB-API
`cursor().execute()` reads lazily the same way (`itersize` rows a batch;
one statement; `rowcount` -1 until the last row is read).  Columnar
results (protocol version 7), without a Python object per value:
`conn.numpy(sql)` (a dict of numpy arrays, masked where there are NULLs;
fixed widths read in place from the batch), `conn.arrow(sql)` (a
`pyarrow.Table`; integers, floats, text and bytea in place, in Arrow's
own layout; a `vector` column a `fixed_size_list<float32>`, and in
`numpy` an `(n, d)` float32 array read in place), `conn.pandas(sql)`, `conn.polars(sql)`, or batch by batch:
`for b in conn.batches(sql, rows=65536): b.column("x").to_numpy()`.
numpy, pyarrow, pandas and polars are optional — needed only by what
uses them.  Bulk load, the other way: `conn.load("t", df)` — a
DataFrame, an Arrow table, a dict of columns or rows, sent as columnar
batches in one `COPY` (numpy numbers, dates and timestamps without a
Python object per value) — and `conn.copy("COPY t FROM STDIN WITH
(FORMAT csv)", open("t.csv", "rb"))` for text or CSV.  Many
statements in one round trip: `conn.pipeline([(sql, params), …])` (each
one's `Result`, or its `Error` in its place); one statement for many
rows, all or none, one commit for all: `conn.execute_many(sql, rows)`
(and the DB-API `cursor.executemany`).  Memory for a model, in a memory
store (`CREATE MEMORY STORE notes USING m`): `conn.remember("notes",
"ann", "prefers short answers", kind="preference",
expires=timedelta(days=30))` → `(id, "inserted" | "merged" |
"replaced")` (`replaces=old_id` keeps the old one as history),
`conn.recall("notes", "ann", "how to answer", k=5)` → the subject's
current memories as dicts, best first (each with `as_of`, the commit
read: cite a memory by its `id` and `as_of`; `SET request_id = 'gen-1'`
records the reads that serve a request in `nc_catalog.reads`), `conn.forget("notes", id)`;
`conn.forget_subject("ann")` → `(erasure, rows, scrubbed, found)` erases
her rows from every table with subject columns (`FORGET SUBJECT`), scrubs
the files and searches them (`found == 0`: none of their values is left).  Tests:
`tests/test_nucleus.py`; example: `examples/python/quickstart.py`.

## In this process (the embedded edition)

The same API on a database in a directory, or in memory, inside this
process — no server.  A connection string chooses, so a program moves
between the two by changing only that:

```python
conn = nucleus.connect("file:app.db")            # directory app.db, made if missing
conn = nucleus.connect("file:app.db?cache=64MB") # with options (cache, sync, wal)
conn = nucleus.connect(":memory:")               # gone when closed
conn = nucleus.connect("nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus?tls=true")

with nucleus.Database(":memory:") as db:         # several sessions on one
    a, b = db.connect(), db.connect(user="app")
```

There is no login in the process: the session is the user it names, and
the directory's file permissions guard the data.  A directory this
process has open already is shared; another process's is refused.  It
needs the engine's library: the `nucleus-db` wheel for your platform
carries it (`build_wheel.py` makes one from a build: `cargo build
--release -p nucleus-embedded`, then `python3 build_wheel.py
target/release/libnucleus_embedded.dylib`), or `NUCLEUS_EMBEDDED_LIB`
names it.  The pure-Python wheel speaks to servers only.

