# nucleus-embedded — nucleus inside an application

The engine as a library (PLAN.md §M14.23): a database in a directory, or
in memory, opened by the process that uses it — no server, no login.
Each session speaks the nucleus protocol through a pipe in the process,
so the drivers run over it unchanged: the Python and JavaScript drivers
have one API for a server and for a database in the process, and a
program moves between them by its connection string (`file:app.db`,
`:memory:`, `nucleus://host:20035`).

```c
#include "nucleus_embedded.h"

char *error = NULL;
nce_database *db = nce_open("app.db", "cache=64MB", &error);   /* or ":memory:" */
nce_connection *c = nce_connect(db, &error);
nce_send(c, hello, hello_len);                /* the nucleus protocol's bytes */
int64_t n = nce_recv(c, buf, sizeof buf, -1);  /* bytes; 0 at the end */
nce_disconnect(c);
nce_close(db, &error);
```

From Rust, `nucleus_embedded::Database::open(path, options)?` and
`nucleus_client::Client::over(Box::new(db.connect()?), config)`.

- **Builds:** a shared library (`libnucleus_embedded.{so,dylib}`,
  `nucleus_embedded.dll`) for Python's `ctypes`, a static one for the
  Node-API addon (`drivers/js/native`) and for Swift and Kotlin, and the
  Rust crate.
- **Options:** `cache` (the page cache), `sync` (`ordered` or `full`),
  `wal` (`minimal` or `full`).
- **Sharing:** a directory this process has open already is shared; the
  last handle's `close` checkpoints and frees it; another process's open
  directory is refused (its lock).
- **Features:** `embeddings` (models of OpenAI-compatible servers, Cohere,
  Voyage) and `local-models` (models run in the process), both on.

- **In WebAssembly** (the browser's edition, `wasm32-wasip1`,
  `drivers/js/wasm/build.sh`): no threads, so sessions are stepped —
  `nce_exchange` / `nce_feed` run the whole messages given and return the
  reply (`Database::exchange` in Rust); a COPY's data goes with the COPY.
  A directory lives in a pool of files the host opened ahead (the
  browser's OPFS), imported as `nucleus_slots`
  (`nucleus_platform::slots`); `nce_pool_free` says when to add more.

DOCS.md §11 (*In the process*, *In the browser*) has the details;
`tests/embedded.rs` runs the slt suite through a session's pipe and
through a stepped session.
