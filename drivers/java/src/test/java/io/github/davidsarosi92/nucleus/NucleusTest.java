package io.github.davidsarosi92.nucleus;

import java.math.BigDecimal;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.OffsetDateTime;
import java.time.ZoneOffset;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
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
        return error(r).code();
    }

    private static NucleusException error(Runnable r) {
        try {
            r.run();
        } catch (NucleusException e) {
            return e;
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
            // What to do about it: the hint (protocol version 5).
            equal(error(() -> db.query("SELECT ib FROM " + t)).hint(), "Perhaps you meant column \"id\".", "hint");
            equal(error(() -> db.query("SELECT 'x'::integer")).hint(), null, "no hint");
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

    static void directory() {
        String user = System.getenv("NUCLEUS_LDAP_USER");
        if (user == null) {
            System.out.println("(the server has no directory)");
            return;
        }
        String pw = System.getenv("NUCLEUS_LDAP_PASSWORD");
        java.nio.file.Path ca = java.nio.file.Path.of(System.getenv("NUCLEUS_TLS_CA"));
        try (NucleusClient db = NucleusClient.connect(config().user(user).password(pw).tlsCa(ca))) {
            equal(db.query("SELECT current_user").rows().get(0).get(0), user, "by the directory");
        }
        errorCode(() -> NucleusClient.connect(config().user(user).password("wrong").tlsCa(ca)));
        try {
            NucleusClient.connect(config().user(user).password(pw)).close();
            throw new AssertionError("a directory password without TLS");
        } catch (NucleusException expected) {
            // refused
        }
    }

    static void token() {
        String token = System.getenv("NUCLEUS_TOKEN");
        if (token == null) {
            System.out.println("(the server has no issuer)");
            return;
        }
        try (NucleusClient db = NucleusClient.connect(config().user("").token(token))) {
            equal(db.query("SELECT current_user, current_setting('nucleus.claims.tenant')").rows().get(0),
                    List.of("drivers_oidc", "acme"), "by token");
        }
        equal(errorCode(() -> NucleusClient.connect(config().token("not.a.token"))), "28000", "a bad token");
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

    static void cursors() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b";
            // Ten billion rows: only what is read is produced.
            NucleusClient.Cursor c = db.cursor(500, big);
            equal(c.columns().get(1).name(), "b", "columns");
            int i = 0;
            for (List<Object> row : c) {
                if (++i == 1201) break;
            }
            c.close();
            equal(c.count(), 1500L, "rows sent before the close");
            equal(db.query("SELECT 1").rows().get(0).get(0), 1, "after a close");
            try (NucleusClient.Cursor all = db.cursor(100, "SELECT i FROM generate_series(1, 2500) i")) {
                int n = 0;
                for (List<Object> row : all) n++;
                equal(n, 2500, "every row");
                equal(all.command() + " " + all.count(), "SELECT 2500", "the tag");
            }
            // An error before the first row throws at once; after rows, from the iteration.
            equal(errorCode(() -> db.cursor("SELECT nope")), "42703", "before rows");
            int[] got = {0};
            equal(errorCode(() -> {
                for (List<Object> row : db.cursor(64, "SELECT 1 / (300 - i) FROM generate_series(1, 400) i")) got[0]++;
            }), "22012", "after rows");
            equal(got[0], 299, "rows before the error");
            // A cursor left unread is closed by the next statement.
            db.cursor(big);
            equal(db.query("SELECT 2").rows().get(0).get(0), 2, "after an unread cursor");
            equal(errorCode(() -> db.cursor("SELECT 1; SELECT 2")), "42601", "one statement");
            equal(db.status(), NucleusClient.Status.IDLE, "idle");
        }
    }

    static void columnar() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, b BOOLEAN, s SMALLINT, big BIGINT, r REAL, f FLOAT, "
                    + "n NUMERIC(10, 2), tx TEXT, ch CHAR(3), bt BYTEA, d DATE, tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ, "
                    + "iv INTERVAL, u UUID, ip INET, net CIDR, arr INT[])");
            db.query("INSERT INTO " + t + " SELECT i, i % 2 = 0, i::smallint, i * 1000000000000, i / 4.0, i / 8.0, "
                    + "i / 3.0, 'v' || i, 'c' || (i % 10), decode('ff00', 'hex'), DATE '2024-01-01' + i, "
                    + "TIME '12:00' + i * INTERVAL '1 second', TIMESTAMP '2024-01-01' + i * INTERVAL '1 hour', "
                    + "TIMESTAMPTZ '2024-01-01 00:00Z' + i * INTERVAL '1 minute', i * INTERVAL '1 day', "
                    + "gen_random_uuid(), '10.0.0.1'::inet, '10.0.0.0/8'::cidr, ARRAY[i, NULL] "
                    + "FROM generate_series(1, 300) i");
            db.query("UPDATE " + t + " SET b = NULL, s = NULL, big = NULL, r = NULL, f = NULL, n = NULL, tx = NULL, "
                    + "ch = NULL, bt = NULL, d = NULL, tm = NULL, ts = NULL, tz = NULL, iv = NULL, u = NULL, "
                    + "ip = NULL, net = NULL, arr = NULL WHERE id % 3 = 0");
            String sql = "SELECT * FROM " + t + " ORDER BY id";
            List<List<Object>> want = db.query(sql).rows();
            List<List<Object>> got = new java.util.ArrayList<>();
            List<Integer> sizes = new java.util.ArrayList<>();
            try (NucleusClient.Batches bs = db.batches(128, sql)) {
                for (NucleusClient.Batch b : bs) {
                    sizes.add(b.rows());
                    got.addAll(b.toRows());
                }
                equal(bs.count(), 300L, "rows sent");
            }
            equal(sizes, List.of(128, 128, 44), "batch sizes");
            for (int i = 0; i < want.size(); i++) {
                for (int c = 0; c < want.get(i).size(); c++) {
                    Object w = want.get(i).get(c), g = got.get(i).get(c);
                    check(w instanceof byte[] ? Arrays.equals((byte[]) w, (byte[]) g) : Objects.equals(w, g),
                            "row " + i + " column " + c + ": " + g + " vs " + w);
                }
            }
            // Buffers read in place.
            try (NucleusClient.Batches bs = db.batches("SELECT id, big, f, tx FROM " + t + " ORDER BY id")) {
                NucleusClient.Batch b = bs.next();
                equal(b.column("id").ints().get(2), 3, "ints");
                equal(b.column("big").longs().get(3), 4_000_000_000_000L, "longs");
                check(b.column("f").isNull(2), "a NULL");
                equal(b.column("f").doubles().get(3), 0.5, "doubles");
                equal(b.column("tx").get(0), "v1", "text");
                check(!bs.hasNext(), "one batch");
            }
            int[] n = {0};
            equal(errorCode(() -> {
                for (NucleusClient.Batch b : db.batches(100, "SELECT 1 / (250 - i) FROM generate_series(1, 400) i")) n[0] += b.rows();
            }), "22012", "an error after batches");
            equal(n[0], 249, "rows before the error");
        }
    }

    static void copyAndLoad() {
        try (NucleusClient db = NucleusClient.connect(config())) {
            String t = table();
            db.query("CREATE TABLE " + t + " (id INT PRIMARY KEY, name TEXT DEFAULT 'none', x FLOAT, big BIGINT, "
                    + "at TIMESTAMPTZ, n NUMERIC(8, 2), ok BOOLEAN, u UUID, tags TEXT[])");
            equal(db.copy("COPY " + t + " (id, name) FROM STDIN", "1\tann\n2\t\\N\n"), 2L, "copy text");
            equal(db.copy("COPY " + t + " (id, x) FROM STDIN WITH (FORMAT csv, HEADER)", "id,x\n3,1.5\n"), 1L, "copy csv");
            NucleusException e = error(() -> db.copy("COPY " + t + " (id) FROM STDIN", "9\n1\n"));
            check(e.code().equals("23505") && e.getMessage().contains("line 2"), "a bad row: " + e.getMessage());
            OffsetDateTime at = OffsetDateTime.of(2024, 1, 2, 3, 4, 0, 0, ZoneOffset.UTC);
            UUID u = UUID.randomUUID();
            List<List<Object>> rows = List.of(
                    Arrays.asList(10, "x", 0.5, 7L, at, new BigDecimal("1.25"), true, u, Arrays.asList("a", null)),
                    Arrays.asList(11, null, null, null, null, null, null, null, null));
            equal(db.load(t, rows), 2L, "load rows");
            equal(db.query("SELECT name, x, big, at, n, ok, u, tags FROM " + t + " WHERE id = 10").rows().get(0),
                    Arrays.asList("x", 0.5, 7L, at, new BigDecimal("1.25"), true, u, Arrays.asList("a", null)), "a loaded row");
            int[] ids = new int[50_000];
            double[] xs = new double[50_000];
            for (int i = 0; i < ids.length; i++) {
                ids[i] = 100 + i;
                xs[i] = i / 2.0;
            }
            Map<String, Object> cols = new java.util.LinkedHashMap<>();
            cols.put("id", ids);
            cols.put("x", xs);
            equal(db.load(t, cols), 50_000L, "load primitive arrays");
            equal(db.query("SELECT count(*), sum(x), min(name) FROM " + t + " WHERE id >= 100").rows().get(0),
                    List.of(50_000L, 49_999.0 * 50_000 / 4, "none"), "the loaded columns");
            check(error(() -> db.load(t, Map.of("id", List.of(7), "big", List.of("no")))).getMessage().contains("\"big\""), "a bad value");
            equal(error(() -> db.load(t, Map.of("id", new long[] {1L << 40}))).code(), "22003", "out of range");
            equal(errorCode(() -> db.query("COPY " + t + " FROM STDIN")), "57014", "COPY without data");
            equal(db.query("SELECT 1").rows().get(0).get(0), 1, "after");
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
        run("token", NucleusTest::token);
        run("directory", NucleusTest::directory);
        run("cursors", NucleusTest::cursors);
        run("columnar", NucleusTest::columnar);
        run("copy and load", NucleusTest::copyAndLoad);
        check(passed == 12, "all tests ran");
        System.out.println(passed + " tests passed");
    }
}
