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
canonical string, arrays, `null`; another registered type is `Ext` (its
name and binary form).  Protocol version 4: the password is proved by
SCRAM-SHA-256, never sent.  Errors are
`NucleusError` with
`code` (SQLSTATE).  Queries on one connection run one after another.
Tests: `npm test`; example: `examples/js/quickstart.ts`.
