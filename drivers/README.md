# nucleus drivers

Clients for nucleus's own protocol, `ncp` (DOCS.md §10).  Each is a small
library without dependencies; all of them connect to a local server as it
starts (`127.0.0.1:20035`, user and password `nucleus`) unless told
otherwise, and all of them pass the same conformance cases (every type both
ways — `uuid` as each language's native uuid, or a string in JavaScript —,
parameters, errors with their SQLSTATE, transactions, passwords).  They
speak protocol version 4: the password is proved by SCRAM-SHA-256 and
never sent, registered types arrive in their binary form, and `smallint`,
`integer`, `real`, `bytea`, `time`, `inet` and `cidr` as each language's
native (or closest) type.  Each can use TLS — the server's certificate
checked against the system's CAs or one's own, or unverified — and show
a client certificate, which logs its user in without a password — and
over TLS binds SCRAM to the session (SCRAM-SHA-256-PLUS) — and logs in
with an OpenID Connect identity token instead of a password (DOCS.md §10,
§11).  A token (`CREATE TOKEN`, DOCS.md §9) is a
user and a password to each of them: its `login` and its `secret`.

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
