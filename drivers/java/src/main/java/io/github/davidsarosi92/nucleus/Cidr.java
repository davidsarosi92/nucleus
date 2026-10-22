package io.github.davidsarosi92.nucleus;

import java.net.InetAddress;

/** A {@code cidr}: a network (its host bits zero). */
public record Cidr(InetAddress address, int bits) {
    /** Parses {@code "10.0.0.0/8"}. */
    public static Cidr parse(String text) {
        Inet i = Inet.parse(text);
        return new Cidr(i.address(), i.bits());
    }

    @Override
    public String toString() {
        return Inet.text(address.getAddress()) + "/" + bits;
    }
}
