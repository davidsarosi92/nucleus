# nucleus drivers

Clients for nucleus's own protocol, `ncp` (DOCS.md §10).  Each is a small
library without dependencies; all of them connect to a local server as it
starts (`127.0.0.1:20035`, user and password `nucleus`) unless told
otherwise, and all of them pass the same conformance cases (every type both
ways — `uuid` as each language's native uuid, or a string in JavaScript —,
parameters, errors with their SQLSTATE, transactions, passwords).  They
speak protocol version 2, in which registered types arrive in their binary
form.

| Language | Where | Values |
|----------|-------|--------|
| Rust | [`crates/client`](../crates/client) (`nucleus-client`) | `Value` enum |
| C | [`c/`](c) — `nucleus.h`, `nucleus.c` (C99) | `nc_value` |
| C++ | [`cpp/`](cpp) — `nucleus.hpp` (C++17, over the C driver) | `std::variant` |
| Python | [`python/`](python) — package `nucleus` (3.8+) | `int`, `Decimal`, `datetime`, … |
| JavaScript / TypeScript | [`js/`](js) — `nucleus-db` (Node 18+) | `number`/`bigint`, `Decimal`, `Date`, … |
| Java | [`java/`](java) — `io.github.davidsarosi92.nucleus` (Java 17+) | `Long`, `BigDecimal`, `java.time`, … |

Examples for each are in [`../examples`](../examples).

Tests: `scripts/driver-tests.sh target/release/nucleus [languages]` starts a
server on a free port and runs every driver's tests and example against it
(CI runs all of them).
