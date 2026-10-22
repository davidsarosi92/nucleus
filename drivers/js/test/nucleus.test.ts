// The JS/TS driver against a running nucleus server (NUCLEUS_HOST,
// NUCLEUS_PORT; scripts/driver-tests.sh starts one).

import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";

import { Cidr, connect, Decimal, Inet, Interval, NucleusError, Time } from "../src/index.ts";

const options = {
  host: process.env.NUCLEUS_HOST ?? "127.0.0.1",
  port: Number(process.env.NUCLEUS_PORT ?? 20035),
};
const table = () => `js_${randomBytes(4).toString("hex")}`;

test("types both ways", async () => {
  const db = await connect(options);
  const t = table();
  await db.query(`CREATE TABLE ${t} (id INT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2),
    d DATE, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[], big BIGINT)`);
  const when = new Date(Date.UTC(2024, 2, 10, 12, 0, 0, 250));
  const row = [1, true, 0.5, "héllo", Decimal.parse("123.45"), new Date(Date.UTC(2024, 1, 29)),
    when, new Interval(1, 2, 3_000_000n), ["a", null, "c"], 2n ** 62n];
  const r = await db.query(`INSERT INTO ${t} VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)`, row);
  assert.equal(r.command, "INSERT");
  assert.equal(r.count, 1);
  const got = await db.query(`SELECT * FROM ${t}`);
  assert.deepEqual(got.columns.map((c) => c.type),
    ["integer", "boolean", "double precision", "text", "numeric", "date", "timestamptz", "interval", "text[]", "bigint"]);
  const v = got.rows[0];
  assert.deepEqual(v.slice(0, 4), [1, true, 0.5, "héllo"]);
  assert.equal(String(v[4]), "123.45");
  assert.equal((v[5] as Date).toISOString(), "2024-02-29T00:00:00.000Z");
  assert.equal((v[6] as Date).getTime(), when.getTime());
  assert.deepEqual(v[7], new Interval(1, 2, 3_000_000n));
  assert.deepEqual(v[8], ["a", null, "c"]);
  assert.equal(v[9], 2n ** 62n);
  // Exact decimals, NULL, and text parameters parsed where needed.
  const sums = await db.query("SELECT 1.50 + 1, NULL, $1 + 1", ["41"]);
  assert.equal(String(sums.rows[0][0]), "2.50");
  assert.equal(sums.rows[0][1], null);
  assert.equal(sums.rows[0][2], 42);
  assert.deepEqual(got.records()[0].s, "héllo");
  await db.close();
});

test("core types", async () => {
  const db = await connect(options);
  const t = table();
  const mood = `${t}_mood`;
  await db.query(`CREATE TYPE ${mood} AS ENUM ('sad', 'happy')`);
  await db.query(`CREATE TABLE ${t} (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, ch CHAR(3),
    ip INET, net CIDR, m ${mood})`);
  const row = [-2, 70000, 0.1, Buffer.from([0, 255]), Time.parse("12:34:56.5"), "ab ",
    Inet.parse("10.1.2.3/8"), Cidr.parse("2001:db8::/32"), "happy"];
  await db.query(`INSERT INTO ${t} VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)`, row);
  const got = await db.query(`SELECT * FROM ${t}`);
  assert.deepEqual(got.columns.map((c) => c.type),
    ["smallint", "integer", "real", "bytea", "time", "character", "inet", "cidr", mood]);
  assert.deepEqual(got.rows[0], row);
  assert.deepEqual([0, 1, 2, 4, 5, 6, 7, 8].map((i) => String(got.rows[0][i])),
    ["-2", "70000", "0.1", "12:34:56.5", "ab ", "10.1.2.3/8", "2001:db8::/32", "happy"]);
  assert.equal(String(Inet.parse("::ffff:1.2.3.4")), "::ffff:1.2.3.4");
  assert.equal(String(Inet.parse("0:0:0:1:0:0:0:1")), "::1:0:0:0:1");
  const n = await db.query(`SELECT count(*) FROM ${t} WHERE b = $1 AND ip <<= $2`,
    [new Uint8Array([0, 255]), Cidr.parse("10.0.0.0/8")]);
  assert.deepEqual(n.rows, [[1]]);
  await db.close();
});

test("uuid", async () => {
  const db = await connect(options);
  const t = table();
  const u = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
  await db.query(`CREATE TABLE ${t} (id UUID PRIMARY KEY, n INT)`);
  await db.query(`INSERT INTO ${t} VALUES ($1, 1)`, [u.toUpperCase()]);
  const r = await db.query(`SELECT id, gen_random_uuid() FROM ${t}`);
  assert.equal(r.columns[0].type, "uuid");
  assert.equal(r.rows[0][0], u);
  assert.match(r.rows[0][1] as string, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.deepEqual((await db.query(`SELECT n FROM ${t} WHERE id = $1`, [u])).rows, [[1]]);
  await db.close();
});

test("errors", async () => {
  const db = await connect(options);
  await assert.rejects(db.query("SELECT * FROM nosuch"), (e: NucleusError) => e.code === "42P01");
  await assert.rejects(db.query("SELECT 1 / 0"), (e: NucleusError) => e.code === "22012");
  const t = table();
  await db.query(`CREATE TABLE ${t} (id INT PRIMARY KEY)`);
  await db.query(`INSERT INTO ${t} VALUES (1)`);
  await assert.rejects(db.query(`INSERT INTO ${t} VALUES (1)`), (e: NucleusError) => e.code === "23505");
  assert.equal((await db.queryAll("SELECT 1; SELECT 2")).length, 2);
  // Queries issued together run one after another.
  const all = await Promise.all([1, 2, 3].map((i) => db.query("SELECT $1", [i])));
  assert.deepEqual(all.map((r) => r.rows[0][0]), [1, 2, 3]);
  await db.close();
});

test("transactions", async () => {
  const db = await connect(options);
  const t = table();
  await db.query(`CREATE TABLE ${t} (id INT PRIMARY KEY)`);
  await db.transaction(async (c) => {
    await c.query(`INSERT INTO ${t} VALUES (1)`);
    assert.equal(c.status, "transaction");
  });
  await assert.rejects(db.transaction(async (c) => {
    await c.query(`INSERT INTO ${t} VALUES (2)`);
    throw new Error("roll back");
  }));
  assert.equal(db.status, "idle");
  assert.deepEqual((await db.query(`SELECT id FROM ${t}`)).rows, [[1]]);
  await db.close();
});

test("authentication", async () => {
  await assert.rejects(connect({ ...options, password: "wrong" }), (e: NucleusError) => e.code === "28P01");
});
