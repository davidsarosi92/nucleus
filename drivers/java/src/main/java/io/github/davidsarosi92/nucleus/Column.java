package io.github.davidsarosi92.nucleus;

/** A result column: its name and type name ({@code bigint}, {@code numeric}, {@code text[]}, …). */
public record Column(String name, String type, int tag) {
    /** A column of a type, by name ({@code tag}: the protocol's type tag, 0 when unknown). */
    public Column(String name, String type) {
        this(name, type, 0);
    }
}
