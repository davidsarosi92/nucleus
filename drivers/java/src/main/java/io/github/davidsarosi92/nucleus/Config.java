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

    public Config host(String host) { this.host = host; return this; }
    public Config port(int port) { this.port = port; return this; }
    public Config user(String user) { this.user = user; return this; }
    public Config password(String password) { this.password = password; return this; }
    public Config database(String database) { this.database = database; return this; }
    public Config clientName(String name) { this.clientName = name; return this; }
    public Config timeoutMillis(int ms) { this.timeoutMillis = ms; return this; }
}
