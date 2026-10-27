# Changelog

## Unreleased

- `ncsql \copy <table> [(<columns>)] FROM '<file>' [WITH (…)]`: loads a
  text or CSV file through `COPY … FROM STDIN`, locally or remotely.
- `nucleus mcp`: the `mcp` module's Model Context Protocol server over
  stdin and stdout, for a local database or a server; reads checked by the
  parser and run read-only.
- `ncsql`: `\lint`, `\statements`; errors with a `HINT:` line; comments
  in `\d`.

## 0.1.0 — 2026-10-18

- The full edition: `nucleus kv`, `nucleus sql` / `ncsql`, `nucleus server`
  with the `ncp` front end and pg-compat's (`--pg-compat`, `--front-end`).
