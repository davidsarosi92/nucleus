package io.github.davidsarosi92.nucleus;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * One statement's result: columns, rows, the command ({@code SELECT}, {@code INSERT}, …) and the
 * number of rows returned or affected.
 */
public record Result(List<Column> columns, List<List<Object>> rows, String command, long count) {
    /** The rows as maps by column name. */
    public List<Map<String, Object>> records() {
        return rows.stream()
                .map(r -> {
                    Map<String, Object> m = new LinkedHashMap<>();
                    for (int i = 0; i < columns.size(); i++) m.put(columns.get(i).name(), r.get(i));
                    return m;
                })
                .toList();
    }
}
