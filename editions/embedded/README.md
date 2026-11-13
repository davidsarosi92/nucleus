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
nc_database *db = nc_open("app.db", "cache=64MB", &error);   /* or ":memory:" */
nc_connection *c = nc_connect(db, &error);
nc_send(c, hello, hello_len);                /* the nucleus protocol's bytes */
int64_t n = nc_recv(c, buf, sizeof buf, -1);  /* bytes; 0 at the end */
nc_disconnect(c);
nc_close(db, &error);
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

DOCS.md §11 (*In the process*) has the details; `tests/embedded.rs` runs
the slt suite through it.
