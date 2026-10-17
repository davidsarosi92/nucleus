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

Types: `boolean`, `number` (or `bigint` beyond 2^53), `Decimal` (exact),
`string`, `Date` (date, timestamp, timestamptz — UTC, millisecond precision
when read), `Interval`, uuid as its canonical string, arrays, `null`;
another registered type is `Ext` (its name and binary form).  Errors are
`NucleusError` with
`code` (SQLSTATE).  Queries on one connection run one after another.
Tests: `npm test`; example: `examples/js/quickstart.ts`.
