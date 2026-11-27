package io.github.davidsarosi92.nucleus;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A stream's entries ({@link NucleusClient#streamOf}, nucleus DOCS.md §8.43): {@link #append} a
 * row, {@link #consume} a group's next entries in the connection's transaction, {@link
 * #position}, {@link #await}, and {@link #process} — a group's next batch handled exactly once.
 */
public final class StreamOf {
    /** The work done on a batch of entries, on the same connection. */
    @FunctionalInterface
    public interface Work {
        void accept(List<Map<String, Object>> entries) throws Exception;
    }

    private final NucleusClient client;
    private final String name;

    StreamOf(NucleusClient client, String name) {
        this.client = client;
        this.name = name;
    }

    private static String ident(String s) {
        return "\"" + s.replace("\"", "\"\"") + "\"";
    }

    /** Appends an entry: its columns' values; its seq and time are the commit's. */
    public void append(Map<String, Object> values) {
        Map<String, Object> ordered = new LinkedHashMap<>(values);
        List<String> names = new ArrayList<>();
        List<String> marks = new ArrayList<>();
        for (String n : ordered.keySet()) {
            names.add(ident(n));
            marks.add("$" + (marks.size() + 1));
        }
        client.query("INSERT INTO " + ident(name) + " (" + String.join(", ", names) + ") VALUES ("
                + String.join(", ", marks) + ")", ordered.values().toArray());
    }

    /**
     * Group {@code group}'s next entries, at most {@code limit} ({@code seq} first); its position
     * moves past them in the connection's transaction — committed with the work done on them, or
     * not at all.
     */
    public List<Map<String, Object>> consume(String group, int limit) {
        return client.query("CONSUME FROM " + ident(name) + " GROUP " + ident(group) + " LIMIT $1",
                (long) limit).records();
    }

    /** Group {@code group}'s position: the last {@code seq} it consumed. */
    public long position(String group) {
        return ((Number) client.query("SELECT stream_position($1, $2)", name, group).rows().get(0).get(0))
                .longValue();
    }

    /** {@link NucleusClient#waitStream} on this stream. */
    public long await(long after, int timeoutMs) {
        return client.waitStream(name, after, timeoutMs);
    }

    /**
     * One batch for {@code group}, exactly once: in a transaction, its next entries (at most
     * {@code limit}, waiting up to {@code timeoutMs} for one when there is none) given to {@code
     * work}, which does its work on this connection; then the commit, the group's position with
     * it. A conflict (another consumer of the group took them) runs it again; an exception of
     * {@code work} rolls all back and is thrown (a checked one wrapped). How many entries were
     * processed (0: none came in time). Not inside a transaction of the caller's.
     */
    public int process(String group, int limit, int timeoutMs, Work work) {
        boolean waited = false;
        while (true) {
            client.query("BEGIN");
            try {
                List<Map<String, Object>> entries = consume(group, limit);
                if (entries.isEmpty()) {
                    long at = position(group);
                    client.query("ROLLBACK");
                    if (waited || timeoutMs == 0) return 0;
                    await(at, timeoutMs);
                    waited = true;
                    continue;
                }
                work.accept(entries);
                client.query("COMMIT");
                return entries.size();
            } catch (NucleusException e) {
                if (client.status() != NucleusClient.Status.IDLE) client.query("ROLLBACK");
                if (!"40001".equals(e.code())) throw e;
            } catch (RuntimeException e) {
                if (client.status() != NucleusClient.Status.IDLE) client.query("ROLLBACK");
                throw e;
            } catch (Exception e) {
                if (client.status() != NucleusClient.Status.IDLE) client.query("ROLLBACK");
                throw new RuntimeException(e);
            }
        }
    }
}
