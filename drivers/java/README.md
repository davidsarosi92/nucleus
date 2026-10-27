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
parameter is a host), `UUID`, `List`, `null`; another registered type is
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
with `isNull(i)` and `get(i)`.  Tests: `NucleusTest` (a plain
main); example: `examples/java/Quickstart.java`.
