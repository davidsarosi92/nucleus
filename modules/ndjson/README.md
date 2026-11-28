# nucleus-ndjson

A sample protocol adapter for [nucleus](https://github.com/davidsarosi92/nucleus):
newline-delimited JSON over TCP onto streams.  It is the pattern a module
that maps an external protocol onto nucleus follows (nucleus PLAN.md
§M16.5 S6, D21), built on the public API alone —
`nucleus_sql::ext::adapters` (`append_json`, `Subscription`, `members`)
and `nucleus_server::FrontEnd`.

```sh
nucleus server ./mydb --front-end ndjson     # 127.0.0.1:20038
```

One JSON object a line, each way:

```text
→ {"login": "app", "password": "…"}
← {"ok": "login"}
→ {"append": "ticks", "entries": [{"symbol": "X", "price": 1.5}, {"symbol": "Y"}]}
← {"ok": "append", "appended": 2}
→ {"subscribe": "ticks", "after": 0}
← {"price": 1.5, "seq": 1, "symbol": "X", "time": "2026-11-28T10:00:00+00:00"}
← {"price": null, "seq": 2, "symbol": "Y", "time": "2026-11-28T10:00:00+00:00"}
← {"heartbeat": 2}
```

- **`login`** first: the server's users and passwords, its login
  throttle, refused logins audited; refused, the connection ends.  The
  password travels as written, so on a network use the server's TLS (a
  client that starts with a TLS handshake gets it) or keep the listener
  on `127.0.0.1`.
- **`append`** — `entries` (an array of objects) or `entry` (one object),
  appended in one transaction: fields by the stream's column names,
  converted to their types (a value that does not convert refuses the
  whole append), fields without a column passed over, columns without a
  field `NULL`.  The user needs `INSERT` on the stream.
- **`subscribe`** — the connection becomes the stream's entries after
  `after` (0, or none: from the first), one a line in `seq` order, until
  the client goes away; `{"heartbeat": seq}` (the last `seq` sent) after
  ten seconds with none.  The client keeps its position: to resume, it
  subscribes after the last `seq` it has.  Entries after it that the
  stream's retention trimmed are a gap —
  `{"error": {"code": "22000", "message": "entries 2 to 3 of stream \"ticks\" are gone …", "begins": 4}}`
  — and the connection ends; a client that accepts the gap subscribes
  after `begins − 1`.  The user needs `SELECT`; row policies apply.
- **Errors** are `{"error": {"code": SQLSTATE, "message": …}}`; after a
  refused `append` the connection goes on.  A line is at most 16 MiB.

Licence: AGPL-3.0-only, as the database.
