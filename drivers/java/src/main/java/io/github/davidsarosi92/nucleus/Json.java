package io.github.davidsarosi92.nucleus;

import java.util.Objects;

/**
 * A {@code json} value: its text — as a result, the server's ({@code {"a": 1, "b": [1, 2]}}); as a
 * parameter, any JSON text, which the server checks.  The driver brings no JSON library: hand
 * {@link #text()} to the one the application uses.
 */
public record Json(String text) {
    public Json {
        Objects.requireNonNull(text, "text");
    }

    @Override
    public String toString() {
        return text;
    }
}
