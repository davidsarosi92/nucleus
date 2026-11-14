# Changelog

## Unreleased

- json (nucleus PLAN.md §M14.15): `Value.json`, a `json` value's text,
  both ways.
- The first version (nucleus PLAN.md §M14.23 step 3): `Nucleus` over the
  C driver — `Connection` (`query`, `execute`, `executeMany`, `pipeline`,
  `cursor`, `copy`, `transaction`), `Value` for every nucleus type — and
  `NucleusEmbedded`, a database in the app over
  `NucleusEmbedded.xcframework` (`Database.open`, `open(named:)`,
  `connect`, `close`).
