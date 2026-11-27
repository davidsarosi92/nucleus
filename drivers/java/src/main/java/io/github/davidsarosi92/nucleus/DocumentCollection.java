package io.github.davidsarosi92.nucleus;

import java.util.ArrayList;
import java.util.List;

/**
 * A collection's documents ({@link NucleusClient#collection}): MongoDB's requests with filter
 * documents. Filters, documents, updates, projections and sorts are JSON texts (null or empty:
 * none; a sort's fields in their order); documents come back as {@link Json}. The driver brings
 * no JSON library: hand the texts to the one the application uses.
 */
public final class DocumentCollection {
    /** What an update or a replacement did. */
    public record UpdateResult(long matched, long modified, Object upsertedId) {}

    private final NucleusClient client;
    private final String name;

    DocumentCollection(NucleusClient client, String name) {
        this.client = client;
        this.name = name;
    }

    /** The documents {@code filter} matches. */
    public List<Json> find(String filter) {
        return find(filter, null, null, 0, 0);
    }

    /**
     * The documents {@code filter} matches: {@code projection} of each, in {@code sort}'s order,
     * {@code skip} skipped, at most {@code limit} (0: all).
     */
    public List<Json> find(String filter, String projection, String sort, long skip, long limit) {
        Result r = client.document(1, name, filter, projection, sort, skip, limit, false, false);
        List<Json> out = new ArrayList<>(r.rows().size());
        for (List<Object> row : r.rows()) out.add((Json) row.get(0));
        return out;
    }

    /** The first document {@code filter} matches, or null. */
    public Json findOne(String filter) {
        List<Json> found = find(filter, null, null, 0, 1);
        return found.isEmpty() ? null : found.get(0);
    }

    /** How many documents {@code filter} matches. */
    public long countDocuments(String filter) {
        return (Long) client.document(2, name, filter, null, null, 0, 0, false, false).rows().get(0).get(0);
    }

    /** Inserts {@code document}; its {@code _id} (made where it has none). */
    public Object insertOne(String document) {
        return client.document(3, name, null, document, null, 0, 0, false, false).rows().get(0).get(0);
    }

    /** Inserts {@code documents}, in their order; their {@code _id}s. */
    public List<Object> insertMany(List<String> documents) {
        Result r = client.document(3, name, null, "[" + String.join(", ", documents) + "]", null, 0, 0, false, false);
        List<Object> ids = new ArrayList<>(r.rows().size());
        for (List<Object> row : r.rows()) ids.add(row.get(0));
        return ids;
    }

    private UpdateResult updated(int op, String filter, String body, boolean many, boolean upsert) {
        List<Object> row = client.document(op, name, filter, body, null, 0, 0, many, upsert).rows().get(0);
        return new UpdateResult((Long) row.get(0), (Long) row.get(1), row.get(2));
    }

    /**
     * Updates the first document {@code filter} matches by {@code update}'s operators ({@code
     * {"$set": …}}); with {@code upsert}, inserts one where none matches.
     */
    public UpdateResult updateOne(String filter, String update, boolean upsert) {
        return updated(4, filter, update, false, upsert);
    }

    /** Updates every document {@code filter} matches. */
    public UpdateResult updateMany(String filter, String update, boolean upsert) {
        return updated(4, filter, update, true, upsert);
    }

    /** Replaces the first document {@code filter} matches (its {@code _id} kept). */
    public UpdateResult replaceOne(String filter, String document, boolean upsert) {
        return updated(5, filter, document, false, upsert);
    }

    /** Deletes the first document {@code filter} matches; how many (0 or 1). */
    public long deleteOne(String filter) {
        return client.document(6, name, filter, null, null, 0, 0, false, false).count();
    }

    /** Deletes every document {@code filter} matches; how many. */
    public long deleteMany(String filter) {
        return client.document(6, name, filter, null, null, 0, 0, true, false).count();
    }
}
