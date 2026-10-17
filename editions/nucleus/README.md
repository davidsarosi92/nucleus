# nucleus — the full edition

The `nucleus` and `ncsql` binaries: everything nucleus has, chosen here
(nucleus PLAN.md §M14.19 — editions are the builds that choose modules).

- `nucleus kv <dir> …` — the KV store.
- `nucleus sql` / `ncsql` — the SQL shell, on a local database or a
  server.
- `nucleus server <dir>` — the server: the nucleus protocol, plus every
  front end this edition includes — `pg` (PostgreSQL compatibility,
  `modules/pg-compat`), started with `--pg-compat` or `--front-end pg`.

See the repository's [README](../../README.md) and [DOCS.md](../../DOCS.md)
§6 and §9.  `tests/cli.rs` drives the binaries.
