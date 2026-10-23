package io.github.davidsarosi92.nucleus;

import java.io.IOException;
import java.io.InputStream;
import java.net.Socket;
import java.nio.file.Files;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.security.spec.PKCS8EncodedKeySpec;
import java.util.Base64;
import javax.net.ssl.KeyManager;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLParameters;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/** TLS over a connected socket (JSSE). */
final class Tls {
    private Tls() {}

    static Socket wrap(Socket tcp, Config config) throws IOException {
        try {
            SSLContext ctx;
            KeyManager[] keys = config.tlsCert == null ? null : keyManagers(config);
            if (!config.tlsVerify) {
                ctx = SSLContext.getInstance("TLS");
                ctx.init(keys, new TrustManager[] {new TrustAll()}, null);
            } else if (config.tlsCa != null) {
                KeyStore roots = KeyStore.getInstance(KeyStore.getDefaultType());
                roots.load(null, null);
                try (InputStream pem = Files.newInputStream(config.tlsCa)) {
                    int i = 0;
                    for (Certificate c : CertificateFactory.getInstance("X.509").generateCertificates(pem)) {
                        roots.setCertificateEntry("ca" + i++, c);
                    }
                }
                TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
                tmf.init(roots);
                ctx = SSLContext.getInstance("TLS");
                ctx.init(keys, tmf.getTrustManagers(), null);
            } else if (keys != null) {
                ctx = SSLContext.getInstance("TLS");
                ctx.init(keys, null, null);
            } else {
                ctx = SSLContext.getDefault();
            }
            SSLSocket s = (SSLSocket) ctx.getSocketFactory().createSocket(tcp, config.host, config.port, true);
            if (config.tlsVerify) {
                // The certificate must name the host connected to.
                SSLParameters p = s.getSSLParameters();
                p.setEndpointIdentificationAlgorithm("HTTPS");
                s.setSSLParameters(p);
            }
            s.startHandshake();
            return s;
        } catch (GeneralSecurityException e) {
            throw new IOException("TLS: " + e.getMessage(), e);
        }
    }

    /** The client certificate chain and its key, from PEM (the key in PKCS #8). */
    private static KeyManager[] keyManagers(Config config) throws IOException, GeneralSecurityException {
        Certificate[] chain;
        try (InputStream pem = Files.newInputStream(config.tlsCert)) {
            chain = CertificateFactory.getInstance("X.509").generateCertificates(pem).toArray(new Certificate[0]);
        }
        String text = Files.readString(config.tlsKey);
        int begin = text.indexOf("-----BEGIN PRIVATE KEY-----");
        int end = text.indexOf("-----END PRIVATE KEY-----");
        if (chain.length == 0 || begin < 0 || end < begin) {
            throw new GeneralSecurityException("a PEM certificate and a PKCS #8 key (BEGIN PRIVATE KEY) are needed");
        }
        byte[] der = Base64.getMimeDecoder().decode(text.substring(begin + 27, end));
        PrivateKey key = null;
        for (String algorithm : new String[] {"RSA", "EC", "Ed25519"}) {
            try {
                key = KeyFactory.getInstance(algorithm).generatePrivate(new PKCS8EncodedKeySpec(der));
                break;
            } catch (GeneralSecurityException e) {
                // another algorithm
            }
        }
        if (key == null) throw new GeneralSecurityException("unsupported private key");
        KeyStore store = KeyStore.getInstance("PKCS12");
        store.load(null, null);
        store.setKeyEntry("client", key, new char[0], chain);
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(store, new char[0]);
        return kmf.getKeyManagers();
    }

    /** Accepts any certificate: encrypted, not authenticated. */
    private static final class TrustAll implements X509TrustManager {
        @Override
        public void checkClientTrusted(X509Certificate[] chain, String auth) {}

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String auth) {}

        @Override
        public X509Certificate[] getAcceptedIssuers() {
            return new X509Certificate[0];
        }
    }
}
