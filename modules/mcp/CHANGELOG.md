# Changelog

## Unreleased

- `recall` answers carry `as_of`, the commit read (nucleus PLAN.md §M14.24
  R5): an agent cites a memory by its id and `as_of`.

- Memory tools (nucleus PLAN.md §M14.24 R1): `remember`, `recall` and
  `forget` over memory stores, with `Options::memory` — independent of
  `allow_writes`.  A tool not offered is refused as unknown.

- `summarize_table` (nucleus PLAN.md §M14.27 S8b): `SUMMARIZE` of a
  table, one row a column.
- First version (nucleus PLAN.md §M14.10): `Server` over a `Database` —
  `list_tables`, `describe_table`, `sample_rows`, `query`, `explain`,
  `lint`, and `execute` when writes are allowed; reads refused if not
  reads and run read-only, with a row and a time limit; typed JSON
  results; errors with their hint.
