# nucleus drivers

Clients for nucleus's own protocol, `ncp` (DOCS.md §10).  Each is a small
library without dependencies; all of them connect to a local server as it
starts (`127.0.0.1:20035`, user and password `nucleus`) unless told
otherwise — the database they name is `nucleus` (the main one) unless
told another, a branch's name (`CREATE BRANCH`, DOCS.md §8.32) for a
branch —, and all of them pass the same conformance cases (every type both
ways — `uuid` as each language's native uuid, or a string in JavaScript —,
parameters, errors with their SQLSTATE, transactions, passwords).  They
speak protocol version 7: large results stream, read a batch at a time
through each driver's cursor (Rust `cursor`, Python and JavaScript
`stream`, Java, C and C++ `cursor`) — or in columnar batches, a buffer
per column, without a value per cell (`batches` in each; Python into
numpy, Arrow, pandas and polars) — and load in bulk through `COPY … FROM
STDIN` (`copy` with text or CSV in each; `load` of rows or columns as
columnar batches in Python — DataFrames and Arrow tables too —,
JavaScript and Java; Rust `copy_in` / `copy_rows`) — send many
statements in one round trip (`pipeline` in each), and run one statement
for many rows of parameters all or none, one commit for all
(`execute_many`; JavaScript and Java `executeMany`, Python's DB-API
`executemany` too) — errors carry a hint (what to do about them),
the password is proved by SCRAM-SHA-256 and never sent, registered types arrive in their binary form (one a driver does not know
by its name and bytes — pg-compat's `tsvector` and `tsquery` their text,
DOCS.md §8.22), and `smallint`,
`integer`, `real`, `bytea`, `time`, `inet` and `cidr` as each language's
native (or closest) type.  Each can use TLS — the server's certificate
checked against the system's CAs or one's own, or unverified — and show
a client certificate, which logs its user in without a password — and
over TLS binds SCRAM to the session (SCRAM-SHA-256-PLUS) — and logs in
with an OpenID Connect identity token instead of a password, and give a
directory (LDAP) the password it checks, over TLS only (DOCS.md §10,
§11).  A token (`CREATE TOKEN`, DOCS.md §9) is a
user and a password to each of them: its `login` and its `secret`.

| Language | Where | Values |
|----------|-------|--------|
| Rust | [`crates/client`](../crates/client) (`nucleus-client`) | `Value` enum |
| C | [`c/`](c) — `nucleus.h`, `nucleus.c` (C99) | `nc_value` |
| C++ | [`cpp/`](cpp) — `nucleus.hpp` (C++17, over the C driver) | `std::variant` |
| Python | [`python/`](python) — package `nucleus` (3.8+) | `int`, `Decimal`, `datetime`, … |
| JavaScript / TypeScript | [`js/`](js) — `nucleus-db` (Node 18+) | `number`/`bigint`, `Decimal`, `Date`, … |
| Java | [`java/`](java) — `io.github.davidsarosi92.nucleus` (Java 17+; Android 8+) | `Long`, `BigDecimal`, `java.time`, … |
| Swift | [`swift/`](swift) — package `Nucleus` (iOS 15+, macOS 12+, Linux; over the C driver) | `Value` enum: `Int64`, `Numeric`, `Date`, `UUID`, … |

A database inside the program, no server (PLAN.md §M14.23): Python and
JavaScript (`connect("file:app.db")`, also in a browser), iOS
([`swift/`](swift), product `NucleusEmbedded`) and Android
([`android/`](android), a Kotlin library over the Java driver) — the
same API as against a server.

Examples for each are in [`../examples`](../examples).

Tests: `scripts/driver-tests.sh target/release/nucleus [languages]` starts a
server on a free port and runs every driver's tests and example against it
(CI runs all of them).
