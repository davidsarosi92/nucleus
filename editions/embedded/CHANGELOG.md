# Changelog

## Unreleased

- Builds for `wasm32-wasip1` (PLAN.md §M14.23 step 2), the browser's
  edition: sessions without threads (`Database::exchange`, C `nc_exchange`,
  `nc_feed`, `nc_exchange_ended`, `nc_exchange_free`), `nc_alloc` /
  `nc_dealloc` for a WebAssembly host, a directory kept in the host's pool
  of files (imports `nucleus_slots`; `nc_pool_free`); no background
  threads there.

- The embedded edition (PLAN.md §M14.23 step 1): `Database::open(path,
  options)` — a directory or `:memory:` — with the server's maintenance
  and embedding worker, `connect()` sessions over the nucleus protocol
  through pipes in the process, `close()`; the C API
  `include/nucleus_embedded.h` (`nc_open`, `nc_connect`, `nc_send`,
  `nc_recv`, `nc_end`, `nc_disconnect`, `nc_close`, `nc_free_error`,
  `nc_version`); features `embeddings` and `local-models`.
