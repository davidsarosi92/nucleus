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

Types: `bool`, `int`, `float`, `decimal.Decimal`, `str`, `datetime.date`,
`datetime.datetime` (naive for timestamp, UTC for timestamptz),
`nucleus.Interval` (and `timedelta` as a parameter), `uuid.UUID`, `list`,
`None`; another registered type is `nucleus.Ext` (its name and binary
form).
Errors are `nucleus.Error` subclasses with `.code` (SQLSTATE).  Tests:
`tests/test_nucleus.py`; example: `examples/python/quickstart.py`.
