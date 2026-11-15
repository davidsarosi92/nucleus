# Migrations and typed queries

A small project the way an application keeps its database: the schema as
versioned migrations, the queries as SQL, and typed code in each language
generated from both — the database checking every query (PLAN.md §M14.16).

| | |
|---|---|
| [`migrations/`](migrations) | the schema's changes, each with its `down` (`nucleus migrate new <name>`) |
| [`queries.sql`](queries.sql) | the application's queries, each named: `-- name: user_by_email :one` |
| [`ts/queries.ts`](ts/queries.ts), [`python/queries.py`](python/queries.py), [`java/Queries.java`](java/Queries.java), [`rust/src/queries.rs`](rust/src/queries.rs) | generated: a typed function per query |
| `ts/app.ts`, `python/app.py`, `java/App.java`, `rust/src/main.rs` | a program in each language that uses them |

Run it all (from the repository root; it starts a server of its own):

```sh
cargo build --release -p nucleus-cli
examples/migrations/run.sh target/release/nucleus            # every language installed
examples/migrations/run.sh target/release/nucleus python     # or some
```

The steps it takes, by hand against your own server:

```sh
nucleus migrate up --dir migrations                    # the pending migrations, each in one transaction
nucleus codegen ts --queries queries.sql --out ts/queries.ts
nucleus codegen python --queries queries.sql --out python/queries.py
nucleus codegen java --queries queries.sql --out java/Queries.java
nucleus codegen rust --queries queries.sql --out rust/src/queries.rs
```

After a change to a migration or a query, `run.sh … --generate` writes the
code again; without it, `run.sh` fails when the committed code is not what
the schema and the queries make (CI runs it).  A query that does not check
fails with its place: `queries.sql:12: user_by_email: column "nope" does
not exist`.

In TypeScript the generated code imports the driver from the repository
(`--import ../../../drivers/js/src/index.ts`); in a project of your own it
is `nucleus-db`, the default.  The programs use their own ids (100s,
200s, …), so all four can run against one database.
