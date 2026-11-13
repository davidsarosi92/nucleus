// One program, two places: a database inside this process, or a server —
// only the connection string differs (PLAN.md section M14.23).
//
//   node examples/js/anywhere.ts                    # file:notes.db, in this process
//   node examples/js/anywhere.ts :memory:           # in memory
//   node examples/js/anywhere.ts nucleus://nucleus:nucleus@127.0.0.1:20035/nucleus
//
// In the process it needs the embedded edition's addon (the package
// nucleus-db-<platform>-<arch>, or drivers/js/native/build.sh).

import { connect } from "../../drivers/js/src/index.ts";

const url = process.argv[2] ?? "file:notes.db";
const db = await connect(url);
console.log("connected:", db.serverVersion);
await db.query("DROP TABLE IF EXISTS anywhere_notes");
await db.query("CREATE TABLE anywhere_notes (id INT PRIMARY KEY, body TEXT, v vector(3))");
await db.executeMany("INSERT INTO anywhere_notes VALUES ($1, $2, $3)", [
  [1, "buy apples", new Float32Array([1, 0, 0])],
  [2, "call the bank", new Float32Array([0, 1, 0])],
  [3, "apple pie recipe", new Float32Array([0.9, 0.1, 0])],
]);
const nearest = await db.query("SELECT id, body FROM anywhere_notes ORDER BY v <-> $1 LIMIT 2", [
  new Float32Array([1, 0, 0]),
]);
console.log("nearest to apples:", nearest.rows);
if (nearest.rows.map((r) => r[0]).join() !== "1,3") throw new Error("unexpected order");
for await (const batch of await db.batches("SELECT id FROM anywhere_notes ORDER BY id")) {
  console.log("a column at a time:", batch.column(0).values);
}
await db.query("DROP TABLE anywhere_notes");
await db.close();
console.log("done");
