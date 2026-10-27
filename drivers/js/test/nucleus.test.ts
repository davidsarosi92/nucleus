// The JS/TS driver against a running nucleus server (NUCLEUS_HOST,
// NUCLEUS_PORT; scripts/driver-tests.sh starts one).

import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";

import { Cidr, connect, Decimal, Inet, Interval, NucleusError, Time } from "../src/index.ts";
import type { Value } from "../src/index.ts";

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
  // What to do about it: the hint (protocol version 5).
  await assert.rejects(db.query(`SELECT ib FROM ${t}`), (e: NucleusError) => e.hint === 'Perhaps you meant column "id".');
  await assert.rejects(db.query("SELECT 'x'::integer"), (e: NucleusError) => e.hint === null);
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

test("streams", async () => {
  const db = await connect(options);
  const big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b";
  // Ten billion rows: only what is read is produced.
  const s = await db.stream(big, [], { fetch: 500 });
  assert.deepEqual(s.columns.map((c) => c.name), ["a", "b"]);
  let i = 0;
  for await (const _row of s) {
    if (++i === 1201) break;
  }
  assert.equal(s.count, 1500);
  assert.deepEqual((await db.query("SELECT 1")).rows, [[1]]);
  const all = await db.stream("SELECT i FROM generate_series(1, 2500) i", [], { fetch: 100 });
  let n = 0;
  for await (const _row of all) n++;
  assert.equal(n, 2500);
  assert.equal(all.command, "SELECT");
  assert.equal(all.count, 2500);
  // An error before the first row rejects at once; after rows, from the
  // iteration.
  await assert.rejects(db.stream("SELECT nope"), (e: NucleusError) => e.code === "42703");
  const got: Value[][] = [];
  await assert.rejects(
    (async () => {
      for await (const row of await db.stream("SELECT 1 / (300 - i) FROM generate_series(1, 400) i", [], { fetch: 64 })) got.push(row);
    })(),
    (e: NucleusError) => e.code === "22012",
  );
  assert.equal(got.length, 299);
  // A stream left unread is closed by the next statement.
  await db.stream(big);
  assert.deepEqual((await db.query("SELECT 2")).rows, [[2]]);
  await assert.rejects(db.stream("SELECT 1; SELECT 2"), (e: NucleusError) => e.code === "42601");
  assert.equal(db.status, "idle");
  await db.close();
});

test("columnar batches", async () => {
  const db = await connect(options);
  const t = table();
  await db.query(`CREATE TABLE ${t} (id INT PRIMARY KEY, b BOOLEAN, s SMALLINT, big BIGINT, r REAL, f FLOAT,
    n NUMERIC(10, 2), tx TEXT, ch CHAR(3), bt BYTEA, d DATE, tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ,
    iv INTERVAL, u UUID, ip INET, net CIDR, arr INT[])`);
  await db.query(`INSERT INTO ${t} SELECT i, i % 2 = 0, i::smallint, i * 1000000000000, i / 4.0, i / 8.0, i / 3.0,
    'v' || i, 'c' || (i % 10), decode('ff00', 'hex'), DATE '2024-01-01' + i, TIME '12:00' + i * INTERVAL '1 second',
    TIMESTAMP '2024-01-01' + i * INTERVAL '1 hour', TIMESTAMPTZ '2024-01-01 00:00Z' + i * INTERVAL '1 minute',
    i * INTERVAL '1 day', gen_random_uuid(), '10.0.0.1'::inet, '10.0.0.0/8'::cidr, ARRAY[i, NULL]
    FROM generate_series(1, 300) i`);
  await db.query(`UPDATE ${t} SET b = NULL, s = NULL, big = NULL, r = NULL, f = NULL, n = NULL, tx = NULL, ch = NULL,
    bt = NULL, d = NULL, tm = NULL, ts = NULL, tz = NULL, iv = NULL, u = NULL, ip = NULL, net = NULL, arr = NULL
    WHERE id % 3 = 0`);
  const sql = `SELECT * FROM ${t} ORDER BY id`;
  const want = (await db.query(sql)).rows;
  const got: Value[][] = [];
  const sizes: number[] = [];
  const bs = await db.batches(sql, [], { rows: 128 });
  for await (const b of bs) {
    sizes.push(b.rows);
    got.push(...b.toRows());
  }
  assert.deepEqual(sizes, [128, 128, 44]);
  assert.equal(bs.count, 300);
  assert.deepEqual(got, want);
  // Typed arrays, read in place.
  for await (const b of await db.batches(`SELECT id, big, f, tx FROM ${t} ORDER BY id`)) {
    const ids = b.column("id").values;
    assert.ok(ids instanceof Int32Array);
    assert.deepEqual([...ids.subarray(0, 3)], [1, 2, 3]);
    assert.ok(b.column("big").values instanceof BigInt64Array);
    assert.equal(b.column("big").values[3], 4_000_000_000_000n);
    assert.ok(b.column("f").isNull(2));
    assert.equal((b.column("f").values as Float64Array)[3], 0.5);
    assert.equal(b.column("tx").get(0), "v1");
  }
  // Empty: no batch; an error after batches rejects the iteration.
  let n = 0;
  for await (const _b of await db.batches(`SELECT * FROM ${t} WHERE false`)) n++;
  assert.equal(n, 0);
  await assert.rejects(
    (async () => {
      for await (const b of await db.batches("SELECT 1 / (250 - i) FROM generate_series(1, 400) i", [], { rows: 100 })) n += b.rows;
    })(),
    (e: NucleusError) => e.code === "22012",
  );
  assert.equal(n, 249);
  await db.close();
});

test("copy and load", async () => {
  const db = await connect(options);
  const t = table();
  await db.query(`CREATE TABLE ${t} (id INT PRIMARY KEY, name TEXT DEFAULT 'none', x FLOAT, big BIGINT,
    at TIMESTAMPTZ, n NUMERIC(8, 2), ok BOOLEAN, u UUID, tags TEXT[])`);
  assert.equal(await db.copy(`COPY ${t} (id, name) FROM STDIN`, "1\tann\n2\t\\N\n"), 2);
  assert.equal(await db.copy(`COPY ${t} (id, x) FROM STDIN WITH (FORMAT csv, HEADER)`, ["id,x\n3,", Buffer.from("1.5\n")]), 1);
  await assert.rejects(db.copy(`COPY ${t} (id) FROM STDIN`, "9\n1\n"), (e: NucleusError) => e.code === "23505" && e.message.includes("line 2"));
  const at = new Date(Date.UTC(2024, 0, 2, 3, 4));
  const u = "0b9e1d7c-2a4b-4c5d-8e6f-7a8b9c0d1e2f";
  assert.equal(await db.load(t, [[10, "x", 0.5, 7n, at, Decimal.parse("1.25"), true, u, ["a", null]], [11, null, null, null, null, null, null, null, null]]), 2);
  const got = (await db.query(`SELECT name, x, big, at, n, ok, u, tags FROM ${t} WHERE id = 10`)).rows[0];
  assert.deepEqual(got, ["x", 0.5, 7, at, Decimal.parse("1.25"), true, u, ["a", null]]);
  // Columns as typed arrays, sent as they are; some columns only.
  const ids = new Int32Array(50_000).map((_, i) => 100 + i);
  const xs = new Float64Array(50_000).map((_, i) => i / 2);
  assert.equal(await db.load(t, { id: ids, x: xs }), 50_000);
  assert.deepEqual((await db.query(`SELECT count(*), sum(x), min(name) FROM ${t} WHERE id >= 100`)).rows, [[50_000, (49_999 * 50_000) / 4, "none"]]);
  await assert.rejects(db.load(t, { id: [7], big: ["no"] }), (e: NucleusError) => e.message.includes('"big"'));
  await assert.rejects(db.load(t, { id: [2 ** 40] }), (e: NucleusError) => e.message.includes("out of range"));
  // A COPY without data, through a plain query, is refused.
  await assert.rejects(db.query(`COPY ${t} FROM STDIN`), (e: NucleusError) => e.code === "57014");
  assert.deepEqual((await db.query("SELECT 1")).rows, [[1]]);
  await db.close();
});

test("authentication", async () => {
  await assert.rejects(connect({ ...options, password: "wrong" }), (e: NucleusError) => e.code === "28P01");
});

test("directory", { skip: process.env.NUCLEUS_LDAP_USER === undefined && "the server has no directory" }, async () => {
  const user = process.env.NUCLEUS_LDAP_USER as string;
  const password = process.env.NUCLEUS_LDAP_PASSWORD as string;
  const tlsCa = process.env.NUCLEUS_TLS_CA as string;
  const db = await connect({ ...options, user, password, tlsCa });
  assert.equal((await db.query("SELECT current_user")).rows[0][0], user);
  await db.close();
  await assert.rejects(connect({ ...options, user, password: "wrong", tlsCa }));
  await assert.rejects(connect({ ...options, user, password }));
});

test("token", { skip: process.env.NUCLEUS_TOKEN === undefined && "the server has no issuer" }, async () => {
  const db = await connect({ ...options, user: "", token: process.env.NUCLEUS_TOKEN });
  const r = await db.query("SELECT current_user, current_setting('nucleus.claims.tenant')");
  assert.deepEqual(r.rows[0], ["drivers_oidc", "acme"]);
  await db.close();
  await assert.rejects(connect({ ...options, token: "not.a.token" }), (e: NucleusError) => e.code === "28000");
});

test("tls", { skip: process.env.NUCLEUS_TLS_CA === undefined && "the server has no TLS" }, async () => {
  const ca = process.env.NUCLEUS_TLS_CA as string;
  const db = await connect({ ...options, tlsCa: ca });
  assert.equal((await db.query("SELECT 'over tls'")).rows[0][0], "over tls");
  await db.close();
  const unverified = await connect({ ...options, tls: true, tlsVerify: false });
  assert.equal((await unverified.query("SELECT 'ok'")).rows[0][0], "ok");
  await unverified.close();
  // The test CA is not among Node's.
  await assert.rejects(connect({ ...options, tls: true }));
  // A client certificate naming the user stands in for its password.
  if (process.env.NUCLEUS_TLS_CERT !== undefined) {
    const byCert = await connect({
      ...options,
      tlsCa: ca,
      tlsCert: process.env.NUCLEUS_TLS_CERT,
      tlsKey: process.env.NUCLEUS_TLS_KEY,
      password: "wrong",
    });
    assert.equal((await byCert.query("SELECT current_user")).rows[0][0], "nucleus");
    await byCert.close();
  }
});
