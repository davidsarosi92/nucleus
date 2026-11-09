package io.github.davidsarosi92.nucleus;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.Map;
import java.util.TreeMap;

/**
 * A {@code sparsevec}: its dimensions and its non-zero components — positions from 0,
 * ascending, each with its value (the text form counts from 1: {@code {1:0.5,4:2}/10}).
 */
public record SparseVector(int dims, int[] indices, float[] values) {
    public SparseVector {
        if (indices.length != values.length) {
            throw new IllegalArgumentException("a sparse vector's indices and values differ in number");
        }
    }

    /** From a map of position (from 0) to value; zeros left out. */
    public static SparseVector of(Map<Integer, Float> components, int dims) {
        TreeMap<Integer, Float> sorted = new TreeMap<>();
        components.forEach((k, v) -> {
            if (v != 0) sorted.put(k, v);
        });
        int[] at = new int[sorted.size()];
        float[] vs = new float[sorted.size()];
        int i = 0;
        for (var e : sorted.entrySet()) {
            at[i] = e.getKey();
            vs[i++] = e.getValue();
        }
        return new SparseVector(dims, at, vs);
    }

    /** The dense components. */
    public float[] toArray() {
        float[] out = new float[dims];
        for (int i = 0; i < indices.length; i++) out[indices[i]] = values[i];
        return out;
    }

    /** Its binary form: dimensions, count, positions, values (little-endian). */
    byte[] bytes() {
        ByteBuffer bb = ByteBuffer.allocate(8 + 8 * indices.length).order(ByteOrder.LITTLE_ENDIAN);
        bb.putInt(dims).putInt(indices.length);
        for (int a : indices) bb.putInt(a);
        for (float v : values) bb.putFloat(v);
        return bb.array();
    }

    /** From its binary form; {@code null} if it is not one. */
    static SparseVector of(byte[] data) {
        if (data.length < 8) return null;
        ByteBuffer bb = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN);
        int dims = bb.getInt();
        int n = bb.getInt();
        if (n < 0 || data.length != 8 + 8L * n) return null;
        int[] at = new int[n];
        float[] vs = new float[n];
        for (int i = 0; i < n; i++) at[i] = bb.getInt();
        for (int i = 0; i < n; i++) vs[i] = bb.getFloat();
        return new SparseVector(dims, at, vs);
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof SparseVector s
                && dims == s.dims
                && Arrays.equals(indices, s.indices)
                && Arrays.equals(values, s.values);
    }

    @Override
    public int hashCode() {
        return 31 * (31 * dims + Arrays.hashCode(indices)) + Arrays.hashCode(values);
    }

    @Override
    public String toString() {
        StringBuilder sb = new StringBuilder("{");
        for (int i = 0; i < indices.length; i++) {
            if (i > 0) sb.append(',');
            sb.append(Integer.toUnsignedLong(indices[i]) + 1).append(':');
            float v = values[i];
            sb.append(v == Math.rint(v) && !Float.isInfinite(v) ? Long.toString((long) v) : Float.toString(v));
        }
        return sb.append("}/").append(Integer.toUnsignedLong(dims)).toString();
    }
}
