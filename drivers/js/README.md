# nucleus-db — the JavaScript/TypeScript driver

Node.js, no dependencies, Promise API, written in TypeScript (Node 22.6+
runs `src/index.ts` directly; `npm run build` emits `dist/` with typings).

```ts
import { connect, Decimal } from "nucleus-db";

const db = await connect();          // 127.0.0.1:20035, user/password "nucleus"
await db.query("INSERT INTO t VALUES ($1, $2)", [1, Decimal.parse("9.99")]);
const { rows } = await db.query("SELECT * FROM t");
await db.transaction(async (tx) => { /* … */ });
await db.close();
```

Types: `boolean`, `number` (smallint, integer, real — by its shortest
digits —, double; bigint too, or `bigint` beyond 2^53), `Decimal` (exact),
`string` (text, `char(n)`, enums), `Buffer` (bytea; any `Uint8Array` as a
parameter), `Date` (date, timestamp, timestamptz — UTC, millisecond
precision when read), `Time` (time of day, µs), `Interval`, `Inet` and
`Cidr` (`.bytes`, `.bits`, `.address`, `parse`, `toString`), uuid as its
canonical string, `Float32Array` (vector), `SparseVector` (sparsevec: `dims`, 0-based `indices`, `values`), arrays, `null`; another registered type is `Ext` (its
name and binary form).  Protocol version 4: the password is proved by
SCRAM-SHA-256, never sent.  TLS: `connect({ tls: true })` (Node's CAs),
`tlsCa: "ca.pem"`, `tlsVerify: false` (encrypted, unverified); `tlsCert`,
`tlsKey` for a client certificate; `token` for an identity token instead
of the password.  Errors are
`NucleusError` with
`code` (SQLSTATE) and `hint` (what to do about it, or `null`).  Queries on one connection run one after another.
Large results stream (protocol version 6): `for await (const row of await
db.stream(sql, params, { fetch: 1000 }))` reads them as they arrive, the
server sending a batch at a time and waiting until it is read; leaving
the loop (or `close()`, or the next query) stops the statement.
Columnar results (protocol version 7): `for await (const b of await
db.batches(sql, params, { rows: 65536 }))` gives a `Batch` whose columns'
`values` are typed arrays read in place (`Int32Array`, `Float64Array`,
`BigInt64Array` for bigint and timestamps — µs since 2000-01-01 —, …;
text as `offsets` and bytes), with `isNull(i)` and `get(i)`.  Bulk load:
`await db.load("t", { id: new Int32Array(…), name: [...] })` (or rows) as
columnar batches in one `COPY` — typed arrays of the column's width sent
as they are — and `await db.copy("COPY t FROM STDIN WITH (FORMAT csv)",
createReadStream("t.csv"))` for text or CSV.  Many statements in one
round trip: `await db.pipeline([[sql, params], …])` (each one's `Result`,
or its `NucleusError` in its place); one statement for many rows, all or
none, one commit for all: `await db.executeMany(sql, rows)`.
Tests: `npm test`; example: `examples/js/quickstart.ts`.

## In this process (the embedded edition)

The same API on a database in a directory, or in memory, inside this
process — no server.  A connection string chooses, so a program moves
between the two by changing only that:

```ts
const db = await connect("file:app.db");             // directory app.db, made if missing
const mem = await connect(":memory:", { options: "cache=64MB" });
const srv = await connect("nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus?tls=true");

const shared = Database.open(":memory:");            // several sessions on one
const a = await shared.connect(), b = await shared.connect({ user: "app" });
shared.close();
```

There is no login in the process: the session is the user it names, and
the directory's file permissions guard the data.  It needs the engine as a
Node-API addon, which Node, Bun and Deno load: `native/build.sh` builds
`native/<platform>-<arch>/nucleus.node` from this checkout (cargo, a C
compiler and Node's headers), the package `nucleus-db-<platform>-<arch>`
will carry it, or `NUCLEUS_EMBEDDED_ADDON` names one.  Each session is read
on a thread of its own and its bytes reach the event loop as a socket's
would: queries never block it.

