package io.github.davidsarosi92.nucleus;

/** A result column: its name and type name ({@code bigint}, {@code numeric}, {@code text[]}, …). */
public record Column(String name, String type) {}
