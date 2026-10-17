// nucleus from JavaScript/TypeScript: connect, create, insert with
// parameters, query, a transaction.  Run a server first
// (`nucleus server ./mydb`), then:  node examples/js/quickstart.ts

import { connect, Decimal, NucleusError } from "../../drivers/js/src/index.ts";

// The defaults reach a local server: 127.0.0.1:20035, user/password nucleus.
const db = await connect({ port: Number(process.env.NUCLEUS_PORT ?? 20035) });
console.log("connected to", db.serverVersion);

await db.query("DROP TABLE IF EXISTS js_orders");
await db.query(`CREATE TABLE js_orders (
  id INT PRIMARY KEY,
  customer TEXT NOT NULL,
  total NUMERIC(10, 2),
  placed TIMESTAMPTZ,
  tags TEXT[]
)`);

for (const [id, customer, total, tags] of [
  [1, "Anna", "19.99", ["new"]],
  [2, "Béla", "120.00", ["vip", "repeat"]],
] as const) {
  await db.query("INSERT INTO js_orders VALUES ($1, $2, $3, $4, $5)",
    [id, customer, Decimal.parse(total), new Date(), [...tags]]);
}

// A transaction: both statements or neither.
await db.transaction(async (tx) => {
  await tx.query("UPDATE js_orders SET total = total * 0.9 WHERE customer = $1", ["Béla"]);
  await tx.query("INSERT INTO js_orders (id, customer, total) VALUES ($1, $2, $3)", [3, "Csilla", Decimal.parse("5.50")]);
});

const { rows } = await db.query("SELECT id, customer, total, tags FROM js_orders ORDER BY id");
for (const [id, customer, total, tags] of rows) console.log(id, customer, String(total), tags);

const sum = (await db.query("SELECT sum(total) FROM js_orders")).rows[0][0];
console.log("sum of totals:", String(sum), "(exact, a Decimal)");

try {
  await db.query("INSERT INTO js_orders (id, customer) VALUES (1, 'dup')");
} catch (e) {
  if (e instanceof NucleusError) console.log("as expected:", e.code, e.message);
}

await db.query("DROP TABLE js_orders");
await db.close();
