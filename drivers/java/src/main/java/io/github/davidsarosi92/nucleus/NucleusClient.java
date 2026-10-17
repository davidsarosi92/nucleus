package io.github.davidsarosi92.nucleus;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.OffsetDateTime;
import java.time.ZoneOffset;
import java.time.temporal.ChronoUnit;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.List;
import java.util.function.Function;

/**
 * The Java driver for nucleus: the nucleus protocol (ncp, DOCS.md section 10). No dependencies.
 *
 * <pre>{@code
 * try (NucleusClient db = NucleusClient.connect(new Config())) {
 *     db.query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)");
 *     db.query("INSERT INTO t VALUES ($1, $2)", 1, "one");
 *     for (List<Object> row : db.query("SELECT id, name FROM t").rows()) System.out.println(row);
 * }
 * }</pre>
 *
 * <p>Values: boolean → Boolean, bigint → Long, double → Double, numeric → BigDecimal, text →
 * String, date → LocalDate, timestamp → LocalDateTime, timestamptz → OffsetDateTime (UTC),
 * interval → {@link Interval}, arrays → List, NULL → null. Parameters take the same types (and
 * Integer, Short, Float, Instant, Duration, arrays and Collections). One thread at a time per
 * connection.
 */
public final class NucleusClient implements AutoCloseable {
    public static final int PROTOCOL_VERSION = 1;
    public static final int DEFAULT_PORT = 20035;

    private static final LocalDate EPOCH_DATE = LocalDate.of(2000, 1, 1);
    private static final LocalDateTime EPOCH = LocalDateTime.of(2000, 1, 1, 0, 0);
    private static final String[] TYPE_NAMES = {
        "unknown", "boolean", "bigint", "double precision", "text", "numeric",
        "date", "timestamp", "timestamptz", "interval", "array",
    };

    /** The transaction status after the last query. */
    public enum Status { IDLE, IN_TRANSACTION, FAILED }

    private final Socket socket;
    private final DataInputStream in;
    private final OutputStream out;
    private Status status = Status.IDLE;
    private String serverVersion = "";
    private long sessionId;

    private NucleusClient(Socket socket) throws IOException {
        this.socket = socket;
        this.in = new DataInputStream(new BufferedInputStream(socket.getInputStream()));
        this.out = new BufferedOutputStream(socket.getOutputStream());
    }

    /** Connects and logs in. */
    public static NucleusClient connect(Config config) {
        try {
            Socket s = new Socket();
            s.connect(new InetSocketAddress(config.host, config.port), config.timeoutMillis);
            s.setTcpNoDelay(true);
            NucleusClient c = new NucleusClient(s);
            Writer hello = new Writer().u16(PROTOCOL_VERSION).str(config.user).str(config.database)
                    .str(config.password == null ? "" : config.password).str(config.clientName);
            c.send('H', hello.bytes());
            for (;;) {
                Message m = c.read();
                Reader r = new Reader(m.body);
                switch (m.kind) {
                    case 'W' -> {
                        r.u16();
                        c.serverVersion = r.str();
                        c.sessionId = r.u32();
                    }
                    case 'R' -> {
                        c.setStatus(m.body);
                        return c;
                    }
                    case 'E' -> {
                        String code = r.str();
                        String message = r.str();
                        s.close();
                        throw new NucleusException(message, code);
                    }
                    default -> throw new NucleusException("unexpected message " + m.kind, (String) null);
                }
            }
        } catch (IOException e) {
            throw new NucleusException("cannot connect to " + config.host + ":" + config.port + ": " + e.getMessage(), e);
        }
    }

    /** Connects with the defaults. */
    public static NucleusClient connect() {
        return connect(new Config());
    }

    /** Runs {@code sql}; the result of each statement. A failing statement throws its error. */
    public List<Result> queryAll(String sql, Object... params) {
        try {
            Writer w = new Writer().str(sql).u8(0).u16(params.length);
            for (Object p : params) w.value(p);
            send('Q', w.bytes());
            List<Result> results = new ArrayList<>();
            NucleusException error = null;
            List<Column> columns = null;
            List<List<Object>> rows = new ArrayList<>();
            for (;;) {
                Message m = read();
                Reader r = new Reader(m.body);
                switch (m.kind) {
                    case 'C' -> {
                        int n = r.u16();
                        columns = new ArrayList<>(n);
                        for (int i = 0; i < n; i++) {
                            String name = r.str();
                            int tag = r.u8();
                            String type = tag == 10 ? typeName(r.u8()) + "[]" : typeName(tag);
                            columns.add(new Column(name, type));
                        }
                        rows = new ArrayList<>();
                    }
                    case 'D' -> {
                        List<Object> row = new ArrayList<>(columns == null ? 0 : columns.size());
                        for (int i = 0; columns != null && i < columns.size(); i++) row.add(r.value());
                        rows.add(row);
                    }
                    case 'K' -> {
                        String command = r.str();
                        long count = r.i64();
                        results.add(new Result(columns == null ? List.of() : columns,
                                columns == null ? List.of() : rows, command, count));
                        columns = null;
                        rows = new ArrayList<>();
                    }
                    case 'E' -> {
                        String code = r.str();
                        error = new NucleusException(r.str(), code);
                    }
                    case 'R' -> {
                        setStatus(m.body);
                        if (error != null) throw error;
                        return results;
                    }
                    default -> throw new NucleusException("unexpected message " + m.kind, (String) null);
                }
            }
        } catch (IOException e) {
            throw new NucleusException("connection lost: " + e.getMessage(), e);
        }
    }

    /** Runs one statement (or several: the last one's result). */
    public Result query(String sql, Object... params) {
        List<Result> all = queryAll(sql, params);
        return all.isEmpty() ? new Result(List.of(), List.of(), "", 0) : all.get(all.size() - 1);
    }

    /** BEGIN, {@code body}, then COMMIT — or ROLLBACK if it throws. */
    public <T> T transaction(Function<NucleusClient, T> body) {
        query("BEGIN");
        try {
            T v = body.apply(this);
            query(status == Status.FAILED ? "ROLLBACK" : "COMMIT");
            return v;
        } catch (RuntimeException e) {
            query("ROLLBACK");
            throw e;
        }
    }

    public Status status() { return status; }
    public String serverVersion() { return serverVersion; }
    public long sessionId() { return sessionId; }

    /** Says Bye and closes. */
    @Override
    public void close() {
        try {
            send('X', new byte[0]);
        } catch (IOException ignored) {
            // closing anyway
        }
        try {
            socket.close();
        } catch (IOException ignored) {
            // closed
        }
    }

    // ------------------------------------------------------------ framing

    private record Message(char kind, byte[] body) {}

    private void send(char kind, byte[] body) throws IOException {
        ByteBuffer h = ByteBuffer.allocate(5).order(ByteOrder.LITTLE_ENDIAN);
        h.put((byte) kind).putInt(body.length);
        out.write(h.array());
        out.write(body);
        out.flush();
    }

    private Message read() throws IOException {
        byte[] h = new byte[5];
        in.readFully(h);
        int n = ByteBuffer.wrap(h, 1, 4).order(ByteOrder.LITTLE_ENDIAN).getInt();
        if (n < 0 || n > (1 << 30)) throw new IOException("message too large");
        byte[] body = new byte[n];
        in.readFully(body);
        return new Message((char) h[0], body);
    }

    private void setStatus(byte[] body) {
        status = body.length > 0 && body[0] == 1 ? Status.IN_TRANSACTION
                : body.length > 0 && body[0] == 2 ? Status.FAILED : Status.IDLE;
    }

    private static String typeName(int tag) {
        return tag >= 0 && tag < TYPE_NAMES.length ? TYPE_NAMES[tag] : "unknown";
    }

    // ------------------------------------------------------------- values

    private static final class Writer {
        private final ByteArrayOutputStream b = new ByteArrayOutputStream();

        Writer u8(int v) { b.write(v); return this; }
        Writer u16(int v) { return raw(ByteBuffer.allocate(2).order(ByteOrder.LITTLE_ENDIAN).putShort((short) v)); }
        Writer i32(int v) { return raw(ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(v)); }
        Writer i64(long v) { return raw(ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(v)); }
        Writer f64(double v) { return raw(ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putDouble(v)); }

        Writer raw(ByteBuffer bb) {
            b.writeBytes(bb.array());
            return this;
        }

        Writer str(String s) {
            byte[] bytes = s.getBytes(StandardCharsets.UTF_8);
            i32(bytes.length);
            b.writeBytes(bytes);
            return this;
        }

        byte[] bytes() { return b.toByteArray(); }

        Writer value(Object v) {
            if (v == null) return u8(0);
            if (v instanceof Boolean x) return u8(1).u8(x ? 1 : 0);
            if (v instanceof Long || v instanceof Integer || v instanceof Short || v instanceof Byte) {
                return u8(2).i64(((Number) v).longValue());
            }
            if (v instanceof BigInteger x) return value(new BigDecimal(x));
            if (v instanceof Double || v instanceof Float) return u8(3).f64(((Number) v).doubleValue());
            if (v instanceof String x) return u8(4).str(x);
            if (v instanceof BigDecimal x) {
                BigDecimal d = x.scale() < 0 ? x.setScale(0) : x;
                if (d.scale() > 38) throw new NucleusException("numeric scale above 38", "22003");
                byte[] be = d.unscaledValue().toByteArray(); // big-endian two's complement
                if (be.length > 16) throw new NucleusException("numeric out of range", "22003");
                byte[] le = new byte[16];
                byte fill = (byte) (be[0] < 0 ? 0xff : 0);
                Arrays.fill(le, fill);
                for (int i = 0; i < be.length; i++) le[i] = be[be.length - 1 - i];
                u8(5);
                b.writeBytes(le);
                return u8(d.scale());
            }
            if (v instanceof LocalDate x) return u8(6).i32((int) ChronoUnit.DAYS.between(EPOCH_DATE, x));
            if (v instanceof LocalDateTime x) return u8(7).i64(ChronoUnit.MICROS.between(EPOCH, x));
            if (v instanceof OffsetDateTime x) {
                return u8(8).i64(ChronoUnit.MICROS.between(EPOCH.atOffset(ZoneOffset.UTC), x));
            }
            if (v instanceof Instant x) return value(x.atOffset(ZoneOffset.UTC));
            if (v instanceof Interval x) return u8(9).i32(x.months()).i32(x.days()).i64(x.micros());
            if (v instanceof Duration x) {
                long micros = x.toNanos() / 1000;
                long days = micros / 86_400_000_000L;
                return u8(9).i32(0).i32((int) days).i64(micros - days * 86_400_000_000L);
            }
            if (v instanceof Collection<?> items) {
                u8(10).i32(items.size());
                for (Object item : items) value(item);
                return this;
            }
            if (v instanceof Object[] items) return value(Arrays.asList(items));
            throw new NucleusException("cannot send a " + v.getClass().getName() + " as a nucleus value", (String) null);
        }
    }

    private static final class Reader {
        private final ByteBuffer b;

        Reader(byte[] body) {
            b = ByteBuffer.wrap(body).order(ByteOrder.LITTLE_ENDIAN);
        }

        int u8() { return b.get() & 0xff; }
        int u16() { return b.getShort() & 0xffff; }
        long u32() { return b.getInt() & 0xffffffffL; }
        int i32() { return b.getInt(); }
        long i64() { return b.getLong(); }

        String str() {
            byte[] s = new byte[(int) u32()];
            b.get(s);
            return new String(s, StandardCharsets.UTF_8);
        }

        Object value() {
            int tag = u8();
            return switch (tag) {
                case 0 -> null;
                case 1 -> u8() != 0;
                case 2 -> i64();
                case 3 -> b.getDouble();
                case 4 -> str();
                case 5 -> {
                    byte[] le = new byte[16];
                    b.get(le);
                    byte[] be = new byte[16];
                    for (int i = 0; i < 16; i++) be[i] = le[15 - i];
                    yield new BigDecimal(new BigInteger(be), u8());
                }
                case 6 -> EPOCH_DATE.plusDays(i32());
                case 7 -> EPOCH.plus(i64(), ChronoUnit.MICROS);
                case 8 -> EPOCH.atOffset(ZoneOffset.UTC).plus(i64(), ChronoUnit.MICROS);
                case 9 -> new Interval(i32(), i32(), i64());
                case 10 -> {
                    long n = u32();
                    List<Object> items = new ArrayList<>();
                    for (long i = 0; i < n; i++) items.add(value());
                    yield items;
                }
                default -> throw new NucleusException("unknown value tag " + tag, (String) null);
            };
        }
    }
}
