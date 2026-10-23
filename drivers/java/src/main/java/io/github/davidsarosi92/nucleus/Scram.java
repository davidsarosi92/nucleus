package io.github.davidsarosi92.nucleus;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.Base64;
import java.util.HashMap;
import java.util.Map;
import javax.crypto.Mac;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.PBEKeySpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * The client's side of a SCRAM-SHA-256 exchange (RFC 5802); over TLS bound to the session
 * (SCRAM-SHA-256-PLUS) when the server offers it, else saying it could have been ("y"), so a
 * stripped offer is caught.
 */
final class Scram {
    private final String password;
    private final String bare;
    private final String gs2;
    private final byte[] binding;
    private byte[] auth;
    private byte[] serverKey;

    /** {@code binding}: the TLS session's tls-server-end-point value, or null. */
    Scram(String password, byte[] binding, boolean offered) {
        this.password = password;
        byte[] nonce = new byte[18];
        new SecureRandom().nextBytes(nonce);
        bare = "n=,r=" + Base64.getEncoder().encodeToString(nonce);
        gs2 = binding == null ? "n,," : offered ? "p=tls-server-end-point,," : "y,,";
        this.binding = binding != null && offered ? binding : new byte[0];
    }

    /**
     * RFC 5929's tls-server-end-point: the server's certificate hashed by its signature's hash
     * (SHA-256 for MD5 and SHA-1); null when it names none.
     */
    static byte[] serverEndPoint(java.security.cert.Certificate cert) {
        if (!(cert instanceof java.security.cert.X509Certificate x)) return null;
        String alg = x.getSigAlgName().toUpperCase(java.util.Locale.ROOT);
        String hash;
        if (alg.startsWith("SHA256WITH") || alg.startsWith("SHA1WITH") || alg.startsWith("MD5WITH")) {
            hash = "SHA-256";
        } else if (alg.startsWith("SHA384WITH")) {
            hash = "SHA-384";
        } else if (alg.startsWith("SHA512WITH")) {
            hash = "SHA-512";
        } else {
            return null;
        }
        try {
            return MessageDigest.getInstance(hash).digest(x.getEncoded());
        } catch (GeneralSecurityException e) {
            return null;
        }
    }

    String first() {
        return gs2 + bare;
    }

    String respond(String serverFirst) {
        Map<String, String> attrs = new HashMap<>();
        for (String p : serverFirst.split(",")) attrs.put(p.substring(0, 1), p.substring(2));
        String nonce = attrs.get("r");
        String own = bare.substring(5);
        if (nonce == null || !nonce.startsWith(own) || nonce.length() == own.length()) {
            throw new NucleusException("malformed SCRAM message", (String) null);
        }
        try {
            byte[] salt = Base64.getDecoder().decode(attrs.get("s"));
            int iterations = Integer.parseInt(attrs.get("i"));
            byte[] salted = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256")
                    .generateSecret(new PBEKeySpec(password.toCharArray(), salt, iterations, 256))
                    .getEncoded();
            byte[] clientKey = hmac(salted, "Client Key".getBytes(StandardCharsets.UTF_8));
            byte[] stored = MessageDigest.getInstance("SHA-256").digest(clientKey);
            serverKey = hmac(salted, "Server Key".getBytes(StandardCharsets.UTF_8));
            byte[] header = gs2.getBytes(StandardCharsets.UTF_8);
            byte[] c = java.util.Arrays.copyOf(header, header.length + binding.length);
            System.arraycopy(binding, 0, c, header.length, binding.length);
            String withoutProof = "c=" + Base64.getEncoder().encodeToString(c) + ",r=" + nonce;
            auth = (bare + "," + serverFirst + "," + withoutProof).getBytes(StandardCharsets.UTF_8);
            byte[] signature = hmac(stored, auth);
            byte[] proof = new byte[clientKey.length];
            for (int i = 0; i < proof.length; i++) proof[i] = (byte) (clientKey[i] ^ signature[i]);
            return withoutProof + ",p=" + Base64.getEncoder().encodeToString(proof);
        } catch (GeneralSecurityException | IllegalArgumentException e) {
            throw new NucleusException("malformed SCRAM message", e);
        }
    }

    void verify(String serverFinal) {
        try {
            byte[] want = hmac(serverKey, auth);
            byte[] got = Base64.getDecoder().decode(serverFinal.substring(2));
            if (!MessageDigest.isEqual(want, got)) {
                throw new NucleusException("the server could not prove it knows the password", (String) null);
            }
        } catch (GeneralSecurityException | IllegalArgumentException e) {
            throw new NucleusException("malformed SCRAM message", e);
        }
    }

    private static byte[] hmac(byte[] key, byte[] data) throws GeneralSecurityException {
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(key, "HmacSHA256"));
        return mac.doFinal(data);
    }
}
