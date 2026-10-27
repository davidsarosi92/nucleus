# nucleus — the Python driver

Pure Python (3.8+), no dependencies, DB-API 2.0 style.

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
`ipaddress.ip_network` (cidr), `uuid.UUID`, `list`, `None`; another
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
own layout), `conn.pandas(sql)`, `conn.polars(sql)`, or batch by batch:
`for b in conn.batches(sql, rows=65536): b.column("x").to_numpy()`.
numpy, pyarrow, pandas and polars are optional — needed only by what
uses them.  Tests:
`tests/test_nucleus.py`; example: `examples/python/quickstart.py`.
