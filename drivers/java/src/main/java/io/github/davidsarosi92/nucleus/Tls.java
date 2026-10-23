package io.github.davidsarosi92.nucleus;

import java.io.IOException;
import java.io.InputStream;
import java.net.Socket;
import java.nio.file.Files;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
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
            if (!config.tlsVerify) {
                ctx = SSLContext.getInstance("TLS");
                ctx.init(null, new TrustManager[] {new TrustAll()}, null);
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
                ctx.init(null, tmf.getTrustManagers(), null);
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
