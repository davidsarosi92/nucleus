package io.github.davidsarosi92.nucleus;

import java.math.BigInteger;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Arrays;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * Client-side encrypted columns (nucleus PLAN.md §M14.30 K5, DOCS.md §11): a
 * value the driver encrypts before it is sent and decrypts after it arrives,
 * so the server stores {@code bytea} ciphertext and never holds the key.
 * Equality is queryable through a blind index stored beside it; nothing else
 * is.  Format 1, every driver's ({@code drivers/encrypted-columns.txt} holds
 * its test vectors):
 *
 * <ul>
 *   <li>keys: {@code HMAC-SHA-256(master, "nucleus column 1 encrypt\0" + name)}
 *       and {@code HMAC-SHA-256(master, "nucleus column 1 index\0" + name)};
 *   <li>a value: a type byte (0 bytes, 1 text in UTF-8, 2 an integer: 8 bytes
 *       little-endian) and its bytes;
 *   <li>ciphertext: the byte 1, a random 24-byte nonce, XChaCha20-Poly1305 of
 *       the value (the byte 1 authenticated beside it);
 *   <li>blind index: the first 16 bytes of HMAC-SHA-256 of the value under the
 *       index key.
 * </ul>
 *
 * <pre>{@code
 * ColumnKey key = new ColumnKey(master, "users.email");   // master: 32 bytes you keep
 * db.query("INSERT INTO users VALUES ($1, $2, $3)", 1L, key.encrypt("ann@example.com"),
 *     key.blindIndex("ann@example.com"));
 * Object email = key.decrypt((byte[]) db.query("SELECT email FROM users WHERE email_index = $1",
 *     key.blindIndex("ann@example.com")).rows().get(0).get(0));     // "ann@example.com"
 * }</pre>
 *
 * <p>ChaCha20 and Poly1305 written here, as RFC 8439 writes them (Java's own
 * ChaCha20-Poly1305 is not on every Android); HChaCha20 as
 * draft-irtf-cfrg-xchacha does.
 */
public final class ColumnKey {
    /** The format's version: a ciphertext's first byte. */
    public static final int FORMAT = 1;
    /** A blind index's length. */
    public static final int INDEX_LEN = 16;

    private static final SecureRandom RANDOM = new SecureRandom();
    private static final int[] SIGMA = {0x61707865, 0x3320646e, 0x79622d32, 0x6b206574};
    private static final BigInteger P = BigInteger.ONE.shiftLeft(130).subtract(BigInteger.valueOf(5));
    private static final BigInteger CLAMP = new BigInteger("0ffffffc0ffffffc0ffffffc0fffffff", 16);
    private static final BigInteger MASK128 = BigInteger.ONE.shiftLeft(128).subtract(BigInteger.ONE);

    private final byte[] encryptKey;
    private final byte[] indexKey;

    /** The keys of column {@code name} under {@code master} (32 bytes the
     *  application keeps: a KMS's, a keychain's; never the database's). */
    public ColumnKey(byte[] master, String name) {
        if (master.length != 32) {
            throw new IllegalArgumentException("a master key is 32 bytes");
        }
        encryptKey = hmac(master, concat(("nucleus column 1 encrypt\0").getBytes(StandardCharsets.UTF_8),
                name.getBytes(StandardCharsets.UTF_8)));
        indexKey = hmac(master, concat(("nucleus column 1 index\0").getBytes(StandardCharsets.UTF_8),
                name.getBytes(StandardCharsets.UTF_8)));
    }

    @Override
    public String toString() {
        return "ColumnKey(..)";
    }

    /** {@code value} ({@code byte[]}, a {@code String}, or an integer)
     *  encrypted: the {@code byte[]} to store, a fresh random nonce each
     *  time. */
    public byte[] encrypt(Object value) {
        byte[] nonce = new byte[24];
        RANDOM.nextBytes(nonce);
        return encrypt(value, nonce);
    }

    /** {@link #encrypt(Object)} with a nonce of one's own — for test vectors
     *  only: one used twice gives both values away. */
    public byte[] encrypt(Object value, byte[] nonce) {
        if (nonce.length != 24) {
            throw new IllegalArgumentException("a nonce is 24 bytes");
        }
        byte[] head = {(byte) FORMAT};
        return concat(head, nonce, xseal(encryptKey, nonce, head, encode(value)));
    }

    /** The value in {@code sealed} — a {@code byte[]}, a {@code String} or a
     *  {@code Long}; {@code null} stays {@code null}.  An
     *  {@link IllegalArgumentException} when this key did not make it, or it
     *  was changed. */
    public Object decrypt(byte[] sealed) {
        if (sealed == null) {
            return null;
        }
        if (sealed.length < 25 || sealed[0] != FORMAT) {
            throw new IllegalArgumentException("an encrypted value this column's key does not open");
        }
        byte[] plain = xopen(encryptKey, Arrays.copyOfRange(sealed, 1, 25), new byte[] {sealed[0]},
                Arrays.copyOfRange(sealed, 25, sealed.length));
        if (plain == null) {
            throw new IllegalArgumentException("an encrypted value this column's key does not open");
        }
        return decode(plain);
    }

    /** {@code value}'s blind index (16 bytes): equal values have equal ones,
     *  so {@code WHERE email_index = $1} with it finds a value's rows. */
    public byte[] blindIndex(Object value) {
        return Arrays.copyOf(hmac(indexKey, encode(value)), INDEX_LEN);
    }

    // ------------------------------------------------------------ the format

    private static byte[] encode(Object v) {
        if (v instanceof byte[] b) {
            return concat(new byte[] {0}, b);
        }
        if (v instanceof String s) {
            return concat(new byte[] {1}, s.getBytes(StandardCharsets.UTF_8));
        }
        if (v instanceof Long || v instanceof Integer || v instanceof Short || v instanceof Byte) {
            return ByteBuffer.allocate(9).order(ByteOrder.LITTLE_ENDIAN).put((byte) 2)
                    .putLong(((Number) v).longValue()).array();
        }
        throw new IllegalArgumentException("an encrypted column holds bytes, a text or an integer, not "
                + (v == null ? "null" : v.getClass().getSimpleName()));
    }

    private static Object decode(byte[] b) {
        if (b.length >= 1 && b[0] == 0) {
            return Arrays.copyOfRange(b, 1, b.length);
        }
        if (b.length >= 1 && b[0] == 1) {
            try {
                return StandardCharsets.UTF_8.newDecoder()
                        .decode(ByteBuffer.wrap(b, 1, b.length - 1)).toString();
            } catch (java.nio.charset.CharacterCodingException e) {
                throw new IllegalArgumentException("an encrypted text is not UTF-8");
            }
        }
        if (b.length == 9 && b[0] == 2) {
            return ByteBuffer.wrap(b, 1, 8).order(ByteOrder.LITTLE_ENDIAN).getLong();
        }
        throw new IllegalArgumentException("an encrypted value of no type known");
    }

    // ------------------------------------------------- XChaCha20-Poly1305

    private static void rounds(int[] s) {
        for (int i = 0; i < 10; i++) {
            q(s, 0, 4, 8, 12);
            q(s, 1, 5, 9, 13);
            q(s, 2, 6, 10, 14);
            q(s, 3, 7, 11, 15);
            q(s, 0, 5, 10, 15);
            q(s, 1, 6, 11, 12);
            q(s, 2, 7, 8, 13);
            q(s, 3, 4, 9, 14);
        }
    }

    private static void q(int[] s, int a, int b, int c, int d) {
        s[a] += s[b];
        s[d] = Integer.rotateLeft(s[d] ^ s[a], 16);
        s[c] += s[d];
        s[b] = Integer.rotateLeft(s[b] ^ s[c], 12);
        s[a] += s[b];
        s[d] = Integer.rotateLeft(s[d] ^ s[a], 8);
        s[c] += s[d];
        s[b] = Integer.rotateLeft(s[b] ^ s[c], 7);
    }

    private static int[] state(byte[] key, byte[] words16) {
        int[] s = new int[16];
        System.arraycopy(SIGMA, 0, s, 0, 4);
        ByteBuffer k = ByteBuffer.wrap(key).order(ByteOrder.LITTLE_ENDIAN);
        for (int i = 0; i < 8; i++) {
            s[4 + i] = k.getInt(4 * i);
        }
        ByteBuffer w = ByteBuffer.wrap(words16).order(ByteOrder.LITTLE_ENDIAN);
        for (int i = 0; i < 4; i++) {
            s[12 + i] = w.getInt(4 * i);
        }
        return s;
    }

    static byte[] hchacha20(byte[] key, byte[] nonce16) {
        int[] s = state(key, nonce16);
        rounds(s);
        ByteBuffer out = ByteBuffer.allocate(32).order(ByteOrder.LITTLE_ENDIAN);
        for (int i = 0; i < 4; i++) {
            out.putInt(s[i]);
        }
        for (int i = 12; i < 16; i++) {
            out.putInt(s[i]);
        }
        return out.array();
    }

    private static byte[] chacha20(byte[] key, int counter, byte[] nonce12, byte[] data) {
        byte[] words = new byte[16];
        System.arraycopy(nonce12, 0, words, 4, 12);
        int[] init = state(key, words);
        byte[] out = data.clone();
        ByteBuffer block = ByteBuffer.allocate(64).order(ByteOrder.LITTLE_ENDIAN);
        for (int at = 0; at < data.length; at += 64) {
            init[12] = counter + at / 64;
            int[] s = init.clone();
            rounds(s);
            block.rewind();
            for (int i = 0; i < 16; i++) {
                block.putInt(s[i] + init[i]);
            }
            byte[] b = block.array();
            for (int i = 0; i < 64 && at + i < data.length; i++) {
                out[at + i] ^= b[i];
            }
        }
        return out;
    }

    private static BigInteger le(byte[] b, int from, int to) {
        byte[] be = new byte[to - from + 1];
        for (int i = from; i < to; i++) {
            be[to - i] = b[i];
        }
        return new BigInteger(be); // be[0] = 0: positive
    }

    private static byte[] poly1305(byte[] key, byte[] msg) {
        BigInteger r = le(key, 0, 16).and(CLAMP);
        BigInteger s = le(key, 16, 32);
        BigInteger acc = BigInteger.ZERO;
        for (int at = 0; at < msg.length; at += 16) {
            int end = Math.min(at + 16, msg.length);
            BigInteger n = le(msg, at, end).setBit(8 * (end - at));
            acc = acc.add(n).multiply(r).mod(P);
        }
        byte[] be = acc.add(s).and(MASK128).toByteArray();
        byte[] out = new byte[16];
        for (int i = 0; i < 16 && i < be.length; i++) {
            out[i] = be[be.length - 1 - i];
        }
        return out;
    }

    private static byte[] tag(byte[] key, byte[] nonce12, byte[] aad, byte[] ct) {
        byte[] otk = chacha20(key, 0, nonce12, new byte[32]);
        int a = (aad.length + 15) / 16 * 16;
        int c = (ct.length + 15) / 16 * 16;
        ByteBuffer mac = ByteBuffer.allocate(a + c + 16).order(ByteOrder.LITTLE_ENDIAN);
        mac.put(aad);
        mac.position(a);
        mac.put(ct);
        mac.position(a + c);
        mac.putLong(aad.length);
        mac.putLong(ct.length);
        return poly1305(otk, mac.array());
    }

    private static byte[][] xkey(byte[] key, byte[] nonce24) {
        byte[] n = new byte[12];
        System.arraycopy(nonce24, 16, n, 4, 8);
        return new byte[][] {hchacha20(key, Arrays.copyOf(nonce24, 16)), n};
    }

    /** XChaCha20-Poly1305: the ciphertext and its 16-byte tag. */
    static byte[] xseal(byte[] key, byte[] nonce24, byte[] aad, byte[] plaintext) {
        byte[][] kn = xkey(key, nonce24);
        byte[] ct = chacha20(kn[0], 1, kn[1], plaintext);
        return concat(ct, tag(kn[0], kn[1], aad, ct));
    }

    /** What {@link #xseal} made, or {@code null} when it does not open. */
    static byte[] xopen(byte[] key, byte[] nonce24, byte[] aad, byte[] sealed) {
        if (sealed.length < 16) {
            return null;
        }
        byte[][] kn = xkey(key, nonce24);
        byte[] ct = Arrays.copyOf(sealed, sealed.length - 16);
        byte[] t = Arrays.copyOfRange(sealed, sealed.length - 16, sealed.length);
        if (!MessageDigest.isEqual(tag(kn[0], kn[1], aad, ct), t)) {
            return null;
        }
        return chacha20(kn[0], 1, kn[1], ct);
    }

    private static byte[] hmac(byte[] key, byte[] data) {
        try {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(key, "HmacSHA256"));
            return m.doFinal(data);
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException("HMAC-SHA-256 is not available", e);
        }
    }

    private static byte[] concat(byte[]... parts) {
        int n = 0;
        for (byte[] p : parts) {
            n += p.length;
        }
        byte[] out = new byte[n];
        int at = 0;
        for (byte[] p : parts) {
            System.arraycopy(p, 0, out, at, p.length);
            at += p.length;
        }
        return out;
    }
}
