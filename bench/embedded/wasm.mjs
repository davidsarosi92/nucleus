// The edition in WebAssembly under Node (nucleus PLAN.md §M14.23), in
// memory: an `INSERT … SELECT` of 200,000 rows, a `GROUP BY` over them,
// a point query (one round trip into the engine, the median of 1,000).
// Each figure is printed as an `@metric` line too, for
// `bench/method.py run`, which repeats the whole script (§M14.31).
//
//   drivers/js/wasm/build.sh && (cd drivers/js && npx tsc)
//   node bench/embedded/wasm.mjs [rows]

import { connect } from "../../drivers/js/dist/browser.js";

const rows = Number(process.argv[2] ?? 200000);
const metric = (name, value, unit = "ms") =>
  console.log("@metric " + JSON.stringify({ name, values: [value], unit, system: "nucleus", phase: "warm" }));
const time = async (f) => {
  const s = performance.now();
  await f();
  return performance.now() - s;
};

const db = await connect(":memory:");
await db.query("CREATE TABLE t (id INT PRIMARY KEY, g INT, v TEXT)");
const insert = await time(() =>
  db.query(`INSERT INTO t SELECT i, i % 100, 'row ' || i FROM generate_series(1, ${rows}) i`));
const groups = await time(async () => {
  const r = await db.query("SELECT g, count(*), sum(id) FROM t GROUP BY g");
  if (r.rows.length !== 100) throw new Error(`${r.rows.length} groups`);
});
const points = [];
for (let i = 0; i < 1100; i++) {
  const id = 1 + ((i * 7919) % rows);
  const t = await time(() => db.query("SELECT v FROM t WHERE id = $1", [id]));
  if (i >= 100) points.push(t);
}
points.sort((a, b) => a - b);
const point = points[points.length >> 1];
await db.close();
console.log(`WebAssembly in Node ${process.version}, in memory: INSERT … SELECT of ${rows} rows ` +
  `${insert.toFixed(0)} ms, GROUP BY ${groups.toFixed(0)} ms, a point query ${(point * 1000).toFixed(0)} µs`);
metric(`INSERT … SELECT of ${rows} rows`, insert);
metric("GROUP BY over them", groups);
metric("a point query", point * 1000, "µs");
console.log("@system " + JSON.stringify({ nucleus: { runtime: `node ${process.version}`, edition: "wasm32-wasip1" } }));
