# nucleus

A layered, multi-model database written in Rust from scratch.  Its core is a
transactional, ordered key-value engine; SQL and document models are built on
top of it, and a distributed implementation of the same KV contract comes
later — without changing the layers above.

**Status:** the core (PLAN.md §F0) and the SQL layer (§F1) exist: a
crash-safe copy-on-write B+tree with a write-ahead log, MVCC transactions
(snapshot isolation and serializable), the KV contract, a PostgreSQL-like
SQL dialect (joins of every kind, CTEs, foreign keys, numeric, date/time
and array types) with a cost-based optimizer, its own system catalog
(`nc_catalog`), and a server that speaks nucleus's own protocol (`ncp`) —
with drivers for Rust, C, C++, Python, JavaScript/TypeScript and Java, and
the `ncsql` shell — and, optionally, PostgreSQL's.  Next is modularity
(PLAN.md §M14.19: registries, extensible and evolvable types, an extension
API — the principles come first), then the rest of the developer platform
(§M14.9–§M14.23: SQL analyzer, AI access, large data and analytics,
vectors, full-text and hybrid search, embeddings, embedded editions for
Python, JavaScript, the browser and phones, branches, time travel, …), then the document model (§F2).  See
[PLAN.md](PLAN.md) for the roadmap and [DOCS.md](DOCS.md) for what exists
today.

## Using it

```sh
nucleus kv ./mydb put greeting "hello world"
nucleus kv ./mydb get greeting
nucleus kv ./mydb            # interactive shell: begin, put, get, scan, commit, …
```

SQL, and a server that nucleus's drivers ([`drivers/`](drivers), with
[`examples/`](examples) in every language) and `ncsql` connect to — and,
with `--pg-compat`, `psql` and PostgreSQL drivers too:

```sh
nucleus sql ./mydb -c "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)"
ncsql ./mydb                            # SQL shell (same as `nucleus sql`)
nucleus server ./mydb                   # nucleus protocol on 127.0.0.1:20035
ncsql -c '\d t'                         # the shell, connected (user and password: nucleus)
nucleus server ./mydb --pg-compat       # also PostgreSQL's protocol on 5432
PGPASSWORD=nucleus psql -h 127.0.0.1 -U nucleus -c '\d t'
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
