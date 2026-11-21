# Changelog

## Unreleased

- A server's Unix socket (nucleus PLAN.md §M14.33 R2), through the C
  driver: a path as the host reaches `nucleus server --socket`.
- Client-side encrypted columns (nucleus PLAN.md §M14.30 K5):
  `ColumnKey(master:name:)` — `encrypt`, `decrypt`, `blindIndex`;
  `Connection.presentSubjectKey` and `holdSubjectKey` (protocol version 8,
  the `k` message).
- The size budget is 10 MiB on arm64 (was 9 MiB): an app's engine,
  9,443,936 bytes, passed 9 MiB by 6.6 KB (nucleus PLAN.md §M14.23).
- The size budget is 9 MiB on arm64 (was 8 MiB): the engine's JSON
  (nucleus PLAN.md §M14.15 J1–J3) took an app's engine to 7.71 MiB, and
  Android's to 8.17 MiB.
- json (nucleus PLAN.md §M14.15): `Value.json`, a `json` value's text,
  both ways.
- The first version (nucleus PLAN.md §M14.23 step 3): `Nucleus` over the
  C driver — `Connection` (`query`, `execute`, `executeMany`, `pipeline`,
  `cursor`, `copy`, `transaction`), `Value` for every nucleus type — and
  `NucleusEmbedded`, a database in the app over
  `NucleusEmbedded.xcframework` (`Database.open`, `open(named:)`,
  `connect`, `close`).
