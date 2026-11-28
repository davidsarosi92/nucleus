package io.github.davidsarosi92.nucleus;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A task queue's tasks ({@link NucleusClient#queueOf}, nucleus DOCS.md §8.45): {@link #add} a
 * task, {@link #take} the next ones under a lease, {@link #finish} one in the connection's
 * transaction (its work's), {@link #fail} one, and {@link #process} — tasks taken and each done
 * exactly once. A task is a map: {@code id}, {@code attempt}, then the queue's columns.
 */
public final class QueueOf {
    /** The work done on a task, on the same connection. */
    @FunctionalInterface
    public interface Work {
        void accept(Map<String, Object> task) throws Exception;
    }

    private final NucleusClient client;
    private final String name;

    QueueOf(NucleusClient client, String name) {
        this.client = client;
        this.name = name;
    }

    private static String ident(String s) {
        return "\"" + s.replace("\"", "\"\"") + "\"";
    }

    private static boolean lost(NucleusException e) {
        return "40001".equals(e.code()) || "55000".equals(e.code());
    }

    /**
     * Adds a task: its columns' values — the queue's own {@code priority} (lower first) and {@code
     * available_at} (a delay) among them if given. In the connection's transaction. Its id.
     */
    public long add(Map<String, Object> values) {
        Map<String, Object> ordered = new LinkedHashMap<>(values);
        Result r;
        if (ordered.isEmpty()) {
            r = client.query("INSERT INTO " + ident(name) + " DEFAULT VALUES RETURNING id");
        } else {
            List<String> names = new ArrayList<>();
            List<String> marks = new ArrayList<>();
            for (String n : ordered.keySet()) {
                names.add(ident(n));
                marks.add("$" + (marks.size() + 1));
            }
            r = client.query("INSERT INTO " + ident(name) + " (" + String.join(", ", names)
                    + ") VALUES (" + String.join(", ", marks) + ") RETURNING id", ordered.values().toArray());
        }
        return ((Number) r.rows().get(0).get(0)).longValue();
    }

    /**
     * The next available tasks, at most {@code limit}, leased for {@code leaseMs} ({@code null}:
     * the queue's lease) — {@code TAKE}, a transaction of its own unless the connection is in one.
     */
    public List<Map<String, Object>> take(int limit, Integer leaseMs) {
        String lease = leaseMs == null ? "" : " LEASE interval '" + leaseMs + " milliseconds'";
        return client.query("TAKE FROM " + ident(name) + " LIMIT " + Math.max(limit, 1) + lease).records();
    }

    /** {@link #take(int, Integer)} under the queue's lease. */
    public List<Map<String, Object>> take(int limit) {
        return take(limit, null);
    }

    /**
     * Finishes {@code task} (deleted, in the connection's transaction — the one its work is in);
     * {@code 55000} if its attempt no longer holds it.
     */
    public void finish(Map<String, Object> task) {
        client.query("FINISH TASK $1 OF " + ident(name) + " ATTEMPT $2", task.get("id"), task.get("attempt"));
    }

    /**
     * Fails {@code task}: available again after {@code retryAfterMs} ({@code null}: the queue's
     * backoff), or — its last attempt — a dead letter. {@code "retry"} or {@code "dead"}.
     */
    public String fail(Map<String, Object> task, String error, Integer retryAfterMs) {
        StringBuilder sql = new StringBuilder("FAIL TASK $1 OF " + ident(name) + " ATTEMPT $2");
        List<Object> params = new ArrayList<>(List.of(task.get("id"), task.get("attempt")));
        if (error != null) {
            sql.append(" ERROR $3");
            params.add(error);
        }
        if (retryAfterMs != null) sql.append(" RETRY AFTER interval '").append(retryAfterMs).append(" milliseconds'");
        return (String) client.query(sql.toString(), params.toArray()).rows().get(0).get(0);
    }

    /**
     * Takes up to {@code limit} tasks (leased for {@code leaseMs}, {@code null}: the queue's) and
     * does each exactly once: in a transaction, {@code work} on this connection, then {@link
     * #finish}, then the commit — the work and the task's end together. A task whose work throws is
     * rolled back and failed with the exception's message (retried after the backoff, or a dead
     * letter); a conflict ({@code 40001}) runs its work again, until its finish says the lease
     * ran out and another took it ({@code 55000}): then it is left to that one. How many were
     * finished. Not inside a transaction of the caller's.
     */
    public int process(int limit, Integer leaseMs, Work work) {
        int finished = 0;
        for (Map<String, Object> task : take(limit, leaseMs)) {
            for (int tries = 0; tries < 10; tries++) {
                client.query("BEGIN");
                try {
                    work.accept(task);
                    finish(task);
                    client.query("COMMIT");
                    finished++;
                    break;
                } catch (Exception e) {
                    if (client.status() != NucleusClient.Status.IDLE) client.query("ROLLBACK");
                    String code = e instanceof NucleusException n ? n.code() : null;
                    if ("40001".equals(code)) continue;
                    if (!"55000".equals(code)) {
                        try {
                            fail(task, String.valueOf(e.getMessage()), null);
                        } catch (NucleusException f) {
                            if (!lost(f)) throw f;
                        }
                    }
                    break;
                }
            }
        }
        return finished;
    }
}
