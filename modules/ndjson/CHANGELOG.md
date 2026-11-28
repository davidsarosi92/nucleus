# Changelog

## Unreleased

- First version (nucleus PLAN.md §M16.5 S6): the `Ndjson` front end —
  newline-delimited JSON over TCP onto streams: a login, `append`
  (entries converted to the stream's column types), `subscribe` (entries
  from a `seq` on, live, with heartbeats; a gap named with where the
  stream begins), errors as JSON with their SQLSTATE.
