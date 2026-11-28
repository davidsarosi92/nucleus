package io.github.davidsarosi92.nucleus;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicLong;

/**
 * A live query ({@link NucleusClient#subscribe}, nucleus DOCS.md §8.47): {@link #rows} its result
 * as of the last batch, kept on this side — the first in the query's order, rows that come after
 * appended, rows gone taken out; {@link #changes} the next batch, applied; {@link #close}.
 */
public final class Subscription implements AutoCloseable {
    /** One row's change in a batch: {@code diff} copies of {@code row} came (+n) or went (-n). */
    public record Change(long diff, List<Object> row) {}

    private static final AtomicLong NEXT = new AtomicLong();

    private final NucleusClient client;
    private final String name;
    private final List<Column> columns;
    private final List<List<Object>> rows = new ArrayList<>();
    private long at;

    Subscription(NucleusClient client, String sql, Object... params) {
        this.client = client;
        this.name = "nc_live_" + NEXT.incrementAndGet();
        Result r = client.query("SUBSCRIBE " + name + " AS " + sql, params);
        this.columns = List.copyOf(r.columns().subList(2, r.columns().size()));
        apply(r.rows());
    }

    /** Its name in the session ({@code CHANGES} and {@code UNSUBSCRIBE} take it). */
    public String name() {
        return name;
    }

    /** The query's columns. */
    public List<Column> columns() {
        return columns;
    }

    /** The result as of the last batch. */
    public List<List<Object>> rows() {
        return Collections.unmodifiableList(rows);
    }

    /** The rows as maps by column name. */
    public List<Map<String, Object>> records() {
        List<Map<String, Object>> out = new ArrayList<>();
        for (List<Object> r : rows) {
            Map<String, Object> m = new LinkedHashMap<>();
            for (int i = 0; i < columns.size(); i++) {
                m.put(columns.get(i).name(), r.get(i));
            }
            out.add(m);
        }
        return out;
    }

    /** The commit the last batch was read at ({@code nc_ts}). */
    public long at() {
        return at;
    }

    /**
     * The next batch ({@code CHANGES … WAIT}), applied to {@link #rows}: waiting up to {@code
     * timeoutMs} for a commit that changes the result; empty when none came in time. An error ends
     * the subscription on the server (its table dropped, a privilege taken back).
     */
    public List<Change> changes(int timeoutMs) {
        return apply(client.query("CHANGES " + name + " WAIT $1", (long) timeoutMs).rows());
    }

    /** Ends it ({@code UNSUBSCRIBE}). */
    @Override
    public void close() {
        client.query("UNSUBSCRIBE " + name);
    }

    private static boolean same(List<Object> a, List<Object> b) {
        return Arrays.deepEquals(a.toArray(), b.toArray());
    }

    private List<Change> apply(List<List<Object>> batch) {
        List<Change> out = new ArrayList<>();
        for (List<Object> r : batch) {
            at = ((Number) r.get(0)).longValue();
            long diff = ((Number) r.get(1)).longValue();
            List<Object> row = new ArrayList<>(r.subList(2, r.size()));
            if (diff > 0) {
                for (long i = 0; i < diff; i++) {
                    rows.add(row);
                }
            } else {
                for (long i = 0; i < -diff; i++) {
                    int found = -1;
                    for (int j = 0; j < rows.size(); j++) {
                        if (same(rows.get(j), row)) {
                            found = j;
                            break;
                        }
                    }
                    if (found < 0) {
                        throw new IllegalStateException("a live query's batch took out a row it did not hold");
                    }
                    rows.remove(found);
                }
            }
            out.add(new Change(diff, row));
        }
        return out;
    }
}
