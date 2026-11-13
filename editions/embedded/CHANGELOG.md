# Changelog

## Unreleased

- The embedded edition (PLAN.md §M14.23 step 1): `Database::open(path,
  options)` — a directory or `:memory:` — with the server's maintenance
  and embedding worker, `connect()` sessions over the nucleus protocol
  through pipes in the process, `close()`; the C API
  `include/nucleus_embedded.h` (`nc_open`, `nc_connect`, `nc_send`,
  `nc_recv`, `nc_end`, `nc_disconnect`, `nc_close`, `nc_free_error`,
  `nc_version`); features `embeddings` and `local-models`.
