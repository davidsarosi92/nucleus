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
`Ext` (its name and binary form).  Protocol version 3.  Errors are
`NucleusException` with `code()` (SQLSTATE).  Tests: `NucleusTest` (a plain
main); example: `examples/java/Quickstart.java`.
