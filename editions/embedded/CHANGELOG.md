# Changelog

## Unreleased

- Builds for `wasm32-wasip1` (PLAN.md §M14.23 step 2), the browser's
  edition: sessions without threads (`Database::exchange`, C `nce_exchange`,
  `nce_feed`, `nce_exchange_ended`, `nce_exchange_free`), `nce_alloc` /
  `nce_dealloc` for a WebAssembly host, a directory kept in the host's pool
  of files (imports `nucleus_slots`; `nce_pool_free`); no background
  threads there.

- The embedded edition (PLAN.md §M14.23 step 1): `Database::open(path,
  options)` — a directory or `:memory:` — with the server's maintenance
  and embedding worker, `connect()` sessions over the nucleus protocol
  through pipes in the process, `close()`; the C API
  `include/nucleus_embedded.h` (`nce_open`, `nce_connect`, `nce_send`,
  `nce_recv`, `nce_end`, `nce_disconnect`, `nce_close`, `nce_free_error`,
  `nce_version`); features `embeddings` and `local-models`.
