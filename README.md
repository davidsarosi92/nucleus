# nucleus

A layered, multi-model database written in Rust from scratch.  Its core is a
transactional, ordered key-value engine; SQL and document models are built on
top of it, and a distributed implementation of the same KV contract comes
later — without changing the layers above.

**Status:** the core (PLAN.md §F0) and the SQL layer (§F1) exist: a
crash-safe copy-on-write B+tree with a write-ahead log, MVCC transactions
(snapshot isolation and serializable), the KV contract, a PostgreSQL-like
SQL dialect (joins of every kind, CTEs, foreign keys, numeric, date/time
and array types) with a cost-based optimizer, and a server that speaks the
PostgreSQL wire protocol.  The document model (§F2) is next.  See [PLAN.md](PLAN.md) for the roadmap
and [DOCS.md](DOCS.md) for what exists today.

## Using it

```sh
nucleus kv ./mydb put greeting "hello world"
nucleus kv ./mydb get greeting
nucleus kv ./mydb            # interactive shell: begin, put, get, scan, commit, …
```

SQL, and a server `psql` or any PostgreSQL driver can connect to:

```sh
nucleus sql ./mydb -c "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)"
nucleus sql ./mydb                      # SQL shell
nucleus server ./mydb --listen 127.0.0.1:5432
psql -h 127.0.0.1 -p 5432 -c "SELECT * FROM t"
psql -h 127.0.0.1 -p 5432 -c '\d t'    # psql's \d works too
```

nucleus describes itself in its own catalog, `nc_catalog`
(`SELECT * FROM nc_catalog.tables`); `pg_catalog` exists for PostgreSQL
clients (DOCS.md §8.11).

As a Rust library: `nucleus_kv::Database::open(dir, Options::default())`,
then `begin` / `get` / `scan` / `put` / `commit` (see DOCS.md §6).

## Building

```sh
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it on
first use.  nucleus builds for Linux, macOS, Windows and
[d-os](https://github.com/davidsarosi92/d-os) (PLAN.md, "Build targets").

## License

MIT — see [LICENSE](LICENSE).
