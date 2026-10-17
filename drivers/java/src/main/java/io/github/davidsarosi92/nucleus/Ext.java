package io.github.davidsarosi92.nucleus;

/**
 * A value of a registered type this driver has no Java type for: the type's name and the
 * value's binary (canonical) form.
 */
public record Ext(String typeName, byte[] data) {}
