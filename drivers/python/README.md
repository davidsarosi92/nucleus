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
version 4: the password is proved by SCRAM-SHA-256, never sent.
Errors are `nucleus.Error` subclasses with `.code` (SQLSTATE).  Tests:
`tests/test_nucleus.py`; example: `examples/python/quickstart.py`.
