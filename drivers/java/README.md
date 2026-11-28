# The Java driver

Plain Java 17+, no dependencies (`pom.xml` for Maven users; the classes
also compile with `javac` alone).

```java
try (NucleusClient db = NucleusClient.connect(new Config())) {   // 127.0.0.1:20035, nucleus / nucleus
    db.query("INSERT INTO t VALUES ($1, $2)", 1, new BigDecimal("9.99"));
    for (var r : db.query("SELECT * FROM t").records()) System.out.println(r);
    db.transaction(tx -> tx.query("UPDATE t SET n = n + 1"));
}
```

Types: `Boolean`, `Short` (smallint), `Integer` (integer), `Long`
(bigint), `Float` (real), `Double`, `BigDecimal`, `String` (text,
`char(n)`, enums), `byte[]` (bytea), `LocalDate`, `LocalTime` (time;
24:00:00 as `LocalTime.MAX`), `LocalDateTime` (timestamp), `OffsetDateTime`
(timestamptz, UTC), `Interval` (and `Duration` as a parameter), `Inet` and
`Cidr` (an `InetAddress` and a mask length; an `InetAddress` as a
parameter is a host), `UUID`, `float[]` (vector), `SparseVector` (sparsevec: dimensions, 0-based positions and values), `Json` (json: its text — the driver brings no JSON library), `List`, `null`; another registered type is
`Ext` (its name and binary form).  Protocol version 4: the password is
proved by SCRAM-SHA-256, never sent.  TLS: `new Config().tls(true)`
(Java's CAs), `.tlsCa(Path.of("ca.pem"))`, `.tlsVerify(false)`
(encrypted, unverified), `.tlsCertificate(cert, key)` for a client
certificate (a PKCS #8 key); `.token(…)` for an identity token instead of
the password.  Errors are
`NucleusException` with `code()` (SQLSTATE) and `hint()` (what to do about
it, or null).  Large results stream (protocol version 6): `try
(NucleusClient.Cursor rows = db.cursor(1000, sql, params…)) { for (var row
: rows) … }` reads them as they arrive, the server sending a batch at a
time and waiting until it is read; closing early (or the next query)
stops the statement.  Columnar results (protocol version 7): `try
(NucleusClient.Batches bs = db.batches(65536, sql, params…)) { for (var b :
bs) b.column("x").ints() … }` — each column a buffer read in place
(`ints()`, `longs()`, `doubles()`, …; text as `bytes()` and `offset(i)`),
with `isNull(i)` and `get(i)`.  Bulk load: `db.load("t", Map.of("id", new
int[] {…}, "name", List.of(…)))` (or rows) as columnar batches in one
`COPY` — primitive arrays of the column's width sent as they are — and
`db.copy("COPY t FROM STDIN WITH (FORMAT csv)", inputStream)`.  Many
statements in one round trip: `db.pipeline(List.of(new Statement(sql,
params…), …))` (each one's `Reply`: its result or its error); one
statement for many rows, all or none, one commit for all:
`db.executeMany(sql, rows)`.  Tests:
`NucleusTest` (a plain
main); example: `examples/java/Quickstart.java`.
Types from your schema: `nucleus codegen java <db> --package com.example
--out Schema.java` writes a record per table with its `SELECT` and
`fromRow(row)` (DOCS §8.37).

## Encrypted columns

A column's values encrypted here and decrypted here: the server stores
`bytea` ciphertext and never holds the key; a blind index beside it finds
equal values (nucleus PLAN.md §M14.30 K5, DOCS.md §11 — what it leaves
visible is said there).  `master` is 32 bytes the application keeps.

```java
ColumnKey key = new ColumnKey(master, "users.email");
db.query("INSERT INTO users VALUES ($1, $2, $3)", 1L, key.encrypt("ann@example.com"),
        key.blindIndex("ann@example.com"));
var rows = db.query("SELECT email FROM users WHERE email_index = $1", key.blindIndex("ann@example.com")).rows();
key.decrypt((byte[]) rows.get(0).get(0));   // "ann@example.com"
```

A sealed table's rows whose key their user holds: `db.presentSubjectKey(subject, secret)`.

## Documents

A collection's documents by MongoDB's requests (`CREATE COLLECTION`;
nucleus DOCS.md §8.41, §10 *Documents*), in the connection's transaction:

```java
DocumentCollection people = db.collection("people");
people.insertOne("{\"name\": \"ann\", \"age\": 34}");          // its _id
List<Json> found = people.find("{\"age\": {\"$gte\": 30}}", "{\"name\": 1}", "{\"age\": -1}", 0, 10);
people.updateOne("{\"name\": \"ann\"}", "{\"$inc\": {\"age\": 1}}", true);
people.deleteMany("{\"age\": {\"$lt\": 18}}");
```

## Streams

A stream's entries (`CREATE STREAM`; nucleus DOCS.md §8.43): appended in a
transaction, numbered by its commit; a consumer group's next batch
handled exactly once — the work's writes and the group's position
committed together, a conflict with another consumer run again; a wait
for the next entry that holds no transaction on the server:

```java
StreamOf jobs = db.streamOf("jobs");
jobs.append(Map.of("name", "resize", "image", 42L));
jobs.process("workers", 100, 30000, entries -> {     // maps, seq first
    for (var e : entries) db.query("INSERT INTO done VALUES ($1)", e.get("seq"));
});                                                   // entries processed
jobs.await(0, 1000);                                  // the newest seq
```

## Task queues

A task queue's tasks (`CREATE QUEUE`; nucleus DOCS.md §8.45): added (a
row), taken under a lease, each done exactly once — the work's writes and
the task's `FINISH` committed together; a task whose work fails is failed
into a retry after the backoff or, past its attempts, a dead letter; one
whose lease ran out while it worked is left to the worker that took it
again:

```java
QueueOf jobs = db.queueOf("jobs");
jobs.add(Map.of("kind", "resize"));             // its id
jobs.process(10, null, task ->                  // id, attempt, the columns
        db.query("INSERT INTO thumbnails VALUES ($1)", task.get("id")));
Map<String, Object> task = jobs.take(1, 60_000).get(0);   // or by hand
jobs.fail(task, "timeout", null);               // finish(task) / fail(...): "retry" | "dead"
```

## Live queries

A query's result kept as the commits change it (nucleus DOCS.md §8.47):

```java
try (Subscription s = db.subscribe("SELECT id, title FROM tasks WHERE done = $1", false)) {
    System.out.println(s.rows());                // the result now
    for (Subscription.Change c : s.changes(30_000)) {
        System.out.println(c.diff() + " " + c.row()); // +1 came, -1 went
    }
}
```

## Licence

MIT — see [LICENSE](LICENSE).  The database itself is AGPL-3.0-only; a
program that reaches a nucleus server through this driver is not bound by
it (the repository's [COPYRIGHT](../../COPYRIGHT)).
