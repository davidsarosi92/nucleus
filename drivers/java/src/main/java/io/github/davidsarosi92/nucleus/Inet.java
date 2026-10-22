package io.github.davidsarosi92.nucleus;

import java.net.InetAddress;
import java.net.UnknownHostException;

/**
 * An {@code inet}: an address and its mask length (32 or 128: a host). {@link #toString()} is
 * nucleus's text: dotted IPv4, IPv6 with the longest run of zero groups as {@code ::}.
 */
public record Inet(InetAddress address, int bits) {
    /** A host address: a full mask. */
    public Inet(InetAddress address) {
        this(address, address.getAddress().length * 8);
    }

    /** Parses {@code "10.1.2.3/8"}, {@code "::1"} (a literal address, never a host name). */
    public static Inet parse(String text) {
        String[] parts = text.trim().split("/", 2);
        InetAddress a = literal(parts[0]);
        return parts.length == 2 ? new Inet(a, Integer.parseInt(parts[1])) : new Inet(a);
    }

    static InetAddress literal(String s) {
        // A literal only (so getByName never looks a name up).
        boolean literal = s.contains(":")
                ? s.matches("[0-9a-fA-F:.]+")
                : s.matches("\\d{1,3}(\\.\\d{1,3}){3}");
        if (!literal) {
            throw new NucleusException("invalid address: " + s, "22P02");
        }
        try {
            InetAddress a = InetAddress.getByName(s); // a literal: no lookup
            if (s.contains(":") && a.getAddress().length == 4) {
                // An IPv4-mapped IPv6 address stays IPv6.
                byte[] v6 = new byte[16];
                v6[10] = (byte) 0xff;
                v6[11] = (byte) 0xff;
                System.arraycopy(a.getAddress(), 0, v6, 12, 4);
                return java.net.Inet6Address.getByAddress(null, v6, -1);
            }
            return a;
        } catch (UnknownHostException e) {
            throw new NucleusException("invalid address: " + s, "22P02");
        }
    }

    /** The address alone, as nucleus writes it. */
    public String host() {
        return text(address.getAddress());
    }

    @Override
    public String toString() {
        return bits == address.getAddress().length * 8 ? host() : host() + "/" + bits;
    }

    static String text(byte[] b) {
        if (b.length == 4) {
            return (b[0] & 0xff) + "." + (b[1] & 0xff) + "." + (b[2] & 0xff) + "." + (b[3] & 0xff);
        }
        int[] w = new int[8];
        for (int i = 0; i < 8; i++) w[i] = (b[2 * i] & 0xff) << 8 | (b[2 * i + 1] & 0xff);
        int best = -1, len = 0;
        for (int i = 0; i < 8; ) {
            if (w[i] != 0) {
                i++;
                continue;
            }
            int start = i;
            while (i < 8 && w[i] == 0) i++;
            if (i - start > len) {
                best = start;
                len = i - start;
            }
        }
        if (len < 2) best = -1;
        if (best == 0 && (len == 6 || (len == 5 && w[5] == 0xffff))) {
            return (len == 5 ? "::ffff:" : "::")
                    + (b[12] & 0xff) + "." + (b[13] & 0xff) + "." + (b[14] & 0xff) + "." + (b[15] & 0xff);
        }
        StringBuilder out = new StringBuilder();
        for (int i = 0; i < 8; ) {
            if (i == best) {
                out.append("::");
                i += len;
                continue;
            }
            if (out.length() > 0 && out.charAt(out.length() - 1) != ':') out.append(':');
            out.append(Integer.toHexString(w[i]));
            i++;
        }
        return out.toString();
    }
}
