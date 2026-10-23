package io.github.davidsarosi92.nucleus;

import java.math.BigDecimal;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.OffsetDateTime;
import java.time.ZoneOffset;
import java.util.Arrays;
import java.util.List;
import java.util.Objects;
import java.util.UUID;

/**
 * The Java driver against a running nucleus server (NUCLEUS_HOST, NUCLEUS_PORT;
 * scripts/driver-tests.sh starts one). Plain Java: run its main.
 */
public final class NucleusTest {
    private static int passed;

    private static Config config() {
        return new Config()
                .host(Objects.requireNonNullElse(System.getenv("NUCLEUS_HOST"), "127.0.0.1"))
                .port(Integer.parseInt(Objects.requireNonNullElse(System.getenv("NUCLEUS_PORT"), "20035")));
    }

    private static String table() {
        return "java_" + UUID.randomUUID().toString().substring(0, 8);
    }

    private static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
    }

    private static void equal(Object got, Object want, String what) {
        if (!Objects.equals(got, want)) throw new AssertionError(what + ": got " + got + ", want " + want);
    }

    private static String errorCode(Runnable r) {
        try {
            r.run();
        } catch (NucleusException e) {
            return e.code();
        }
        throw new AssertionError("no error");
    }

    static void typesBothWays() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, b BOOLEAN, f FLOAT, s TEXT, n NUMERIC(10, 2),"
                    + " d DATE, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, tags TEXT[])");
            List<Object> row = Arrays.asList(1, true, 0.5, "héllo", new BigDecimal("-123.45"),
                    LocalDate.of(2024, 2, 29), LocalDateTime.of(2024, 3, 10, 14, 5, 6, 500_000_000),
                    OffsetDateTime.of(2024, 3, 10, 12, 0, 0, 0, ZoneOffset.UTC), new Interval(1, 2, 3_000_000),
                    Arrays.asList("a", null, "c"));
            Result r = db.query("INSERT INTO " + t + " VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)", row.toArray());
            equal(r.command() + " " + r.count(), "INSERT 1", "insert");
            Result got = db.query("SELECT * FROM " + t);
            equal(got.rows(), List.of(row), "round trip");
            equal(got.columns().get(9).type(), "text[]", "array type");
            equal(db.query("SELECT 1.50 + 1, $1 + 1", "41").rows().get(0), List.of(new BigDecimal("2.50"), 42), "numeric and text param");
            equal(db.query("SELECT NULL").rows().get(0).get(0), null, "null");
            equal(got.records().get(0).get("s"), "héllo", "records");
        }
    }

    static void uuid() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            UUID u = UUID.fromString("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
            db.query("CREATE TABLE " + t + " (id UUID PRIMARY KEY, n INT)");
            db.query("INSERT INTO " + t + " VALUES ($1, 1)", u);
            Result r = db.query("SELECT id, id::text, gen_random_uuid() FROM " + t);
            equal(r.columns().get(0).type(), "uuid", "uuid column type");
            equal(r.rows().get(0).get(0), u, "uuid value");
            equal(r.rows().get(0).get(1), u.toString(), "uuid text");
            equal(((UUID) r.rows().get(0).get(2)).version(), 4, "a version-4 uuid");
            equal(db.query("SELECT n FROM " + t + " WHERE id = $1", u.toString().toUpperCase()).rows(), List.of(List.of(1)), "text parameter");
        }
    }

    static void coreTypes() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            String mood = t + "_mood";
            db.query("CREATE TYPE " + mood + " AS ENUM ('sad', 'happy')");
            db.query("CREATE TABLE " + t + " (s SMALLINT, i INTEGER, r REAL, b BYTEA, tm TIME, ch CHAR(3),"
                    + " ip INET, net CIDR, m " + mood + ")");
            byte[] bytes = {0, (byte) 0xff};
            List<Object> row = Arrays.asList((short) -2, 70000, 0.1f, bytes, LocalTime.of(12, 34, 56, 500_000_000),
                    "ab ", Inet.parse("10.1.2.3/8"), Cidr.parse("2001:db8::/32"), "happy");
            db.query("INSERT INTO " + t + " VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", row.toArray());
            Result got = db.query("SELECT * FROM " + t);
            List<Object> back = got.rows().get(0);
            equal(Arrays.equals((byte[]) back.get(3), bytes), true, "bytea");
            List<Object> rest = new java.util.ArrayList<>(back);
            rest.set(3, null);
            List<Object> want = new java.util.ArrayList<>(row);
            want.set(3, null);
            equal(rest, want, "core types round trip");
            equal(got.columns().stream().map(Column::type).toList(),
                    List.of("smallint", "integer", "real", "bytea", "time", "character", "inet", "cidr", mood),
                    "column types");
            equal(back.get(6).toString() + " " + back.get(7), "10.1.2.3/8 2001:db8::/32", "address text");
            equal(Inet.parse("::ffff:1.2.3.4").toString(), "::ffff:1.2.3.4", "mapped address");
            equal(db.query("SELECT count(*) FROM " + t + " WHERE b = $1 AND ip <<= $2", bytes,
                    Cidr.parse("10.0.0.0/8")).rows(), List.of(List.of(1L)), "parameters");
            equal(db.query("SELECT time '24:00'").rows().get(0).get(0), LocalTime.MAX, "end of day");
        }
    }

    static void errors() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            equal(errorCode(() -> db.query("SELECT * FROM nosuch")), "42P01", "undefined table");
            equal(errorCode(() -> db.query("SELECT 1 / 0")), "22012", "division by zero");
            String t = table();
            db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY)");
            db.query("INSERT INTO " + t + " VALUES (1)");
            equal(errorCode(() -> db.query("INSERT INTO " + t + " VALUES (1)")), "23505", "duplicate");
            equal(db.queryAll("SELECT 1; SELECT 2").size(), 2, "two results");
        }
    }

    static void transactions() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY)");
            db.transaction(c -> c.query("INSERT INTO " + t + " VALUES (1)"));
            try {
                db.transaction(c -> {
                    c.query("INSERT INTO " + t + " VALUES (2)");
                    throw new IllegalStateException("roll back");
                });
                throw new AssertionError("no exception");
            } catch (IllegalStateException expected) {
                // rolled back
            }
            equal(db.status(), NucleusClient.Status.IDLE, "status");
            equal(db.query("SELECT id FROM " + t).rows(), List.of(List.of(1)), "committed rows");
        }
    }

    static void authentication() {
        equal(errorCode(() -> NucleusClient.connect(config().password("wrong"))), "28P01", "wrong password");
    }

    static void tls() {
        String ca = System.getenv("NUCLEUS_TLS_CA");
        if (ca == null) {
            System.out.println("(the server has no TLS)");
            return;
        }
        try (NucleusClient db = NucleusClient.connect(config().tlsCa(java.nio.file.Path.of(ca)))) {
            equal(db.query("SELECT 'over tls'").rows().get(0).get(0), "over tls", "verified");
        }
        try (NucleusClient db = NucleusClient.connect(config().tls(true).tlsVerify(false))) {
            equal(db.query("SELECT 'ok'").rows().get(0).get(0), "ok", "unverified");
        }
        // The test CA is not among Java's.
        errorCode(() -> NucleusClient.connect(config().tls(true)));
        // A client certificate naming the user stands in for its password.
        String cert = System.getenv("NUCLEUS_TLS_CERT"), key = System.getenv("NUCLEUS_TLS_KEY");
        if (cert != null) {
            Config byCert = config().tlsCa(java.nio.file.Path.of(ca)).password("wrong")
                    .tlsCertificate(java.nio.file.Path.of(cert), java.nio.file.Path.of(key));
            try (NucleusClient db = NucleusClient.connect(byCert)) {
                equal(db.query("SELECT current_user").rows().get(0).get(0), "nucleus", "by certificate");
            }
        }
    }

    private static void run(String name, Runnable test) {
        test.run();
        passed++;
        System.out.println("ok " + name);
    }

    public static void main(String[] args) {
        run("types both ways", NucleusTest::typesBothWays);
        run("uuid", NucleusTest::uuid);
        run("core types", NucleusTest::coreTypes);
        run("errors", NucleusTest::errors);
        run("transactions", NucleusTest::transactions);
        run("authentication", NucleusTest::authentication);
        run("tls", NucleusTest::tls);
        check(passed == 7, "all tests ran");
        System.out.println(passed + " tests passed");
    }
}
