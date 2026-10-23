# nucleus-pg-compat

PostgreSQL compatibility for [nucleus](https://github.com/davidsarosi92/nucleus):
a module, so that `psql` and PostgreSQL drivers and tools can talk to a
nucleus database.  nucleus's own interface — the `ncp` protocol,
`nc_catalog` — comes first and is never shaped by this module
(nucleus PLAN.md, principle 11).

- **`Pg`** — a front end for `nucleus-server`: PostgreSQL's protocol,
  version 3 (startup with SSL accepted when the server has TLS — the
  `nucleus-tls` module — and GSS declined; trust or SCRAM-SHA-256 (over
  TLS also SCRAM-SHA-256-PLUS, bound to the session) — the
  password is proved, never sent — and the simple and extended query
  protocols, text and binary formats).
- **`PgCompat`** — the SQL side, registered in a session's registry:
  `pg_catalog` (`pg_class`, `pg_attribute`, `pg_constraint`, `pg_index`,
  `pg_type`, `pg_namespace`, …) and `information_schema` as views of the
  nucleus catalog, the `pg_*` functions their queries call
  (`format_type`, `pg_get_constraintdef`, `pg_typeof`, …) and the
  `::regclass` family.  Enough for `psql`'s `\d` commands.  The `Pg` front
  end's sessions have it (`registry()`); other sessions do not.

It is frozen: kept working, not extended for the catalog queries
particular tools send.

```rust
use std::sync::Arc;
let mut fronts = nucleus_server::FrontEnds::builtin();
fronts.add(Arc::new(nucleus_pg_compat::Pg)).unwrap();
// nucleus_server::serve_front_end(fronts.get("pg").unwrap().clone(), listener, kv, config)
```

The nucleus binary includes it: `nucleus server <dir> --pg-compat`.

## Built on the public API only

This folder is a module in nucleus's repository, built to leave it: it
depends on nucleus's public API crates (`nucleus-kv`, `nucleus-sql`'s
`ext` module, `nucleus-server`) by version, reads no files outside this
folder, and CI checks both — the boundary check and the detachability
test (`cargo package`, which builds it from its packaged form alone).

## Tests

- `tests/wire.rs` — the SQL layer's scripts (`nucleus-testkit`) and this
  module's (`tests/slt/`) over the PostgreSQL protocol, authentication,
  and the extended protocol through the `postgres` driver.
- `tests/catalog.rs` — this module's scripts in a local session.

## Licence

MIT — see [LICENSE](LICENSE).
