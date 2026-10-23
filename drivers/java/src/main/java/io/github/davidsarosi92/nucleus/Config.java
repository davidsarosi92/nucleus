package io.github.davidsarosi92.nucleus;

/**
 * Where and as whom to connect. The defaults reach a local server as it starts: {@code
 * 127.0.0.1:20035}, user and password {@code nucleus}.
 */
public final class Config {
    String host = "127.0.0.1";
    int port = NucleusClient.DEFAULT_PORT;
    String user = "nucleus";
    String password = "nucleus";
    String database = "nucleus";
    String clientName = "nucleus-java";
    int timeoutMillis = 10_000;
    boolean tls = false;
    java.nio.file.Path tlsCa = null;
    boolean tlsVerify = true;

    public Config host(String host) { this.host = host; return this; }
    public Config port(int port) { this.port = port; return this; }
    public Config user(String user) { this.user = user; return this; }
    public Config password(String password) { this.password = password; return this; }
    public Config database(String database) { this.database = database; return this; }
    public Config clientName(String name) { this.clientName = name; return this; }
    public Config timeoutMillis(int ms) { this.timeoutMillis = ms; return this; }
    /** TLS, the server's certificate checked against Java's CA certificates. */
    public Config tls(boolean tls) { this.tls = tls; return this; }
    /** TLS, the certificate checked against the CA certificates in this PEM file. */
    public Config tlsCa(java.nio.file.Path pem) { this.tlsCa = pem; this.tls = true; return this; }
    /** {@code false}: encrypt without checking the certificate (anyone can pose as the server). */
    public Config tlsVerify(boolean verify) { this.tlsVerify = verify; return this; }
}
