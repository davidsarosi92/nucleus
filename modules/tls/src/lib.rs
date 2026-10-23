//! TLS for nucleus (PLAN.md §M14.25), over [rustls]: a module, so the core
//! carries no TLS library.
//!
//! - [`ServerTls`] — `nucleus_server::Tls`, for [`nucleus_server::Config`]'s
//!   `tls`: the front ends offer it to clients that ask (`ncp` by starting
//!   a handshake, PostgreSQL's protocol by `SSLRequest`).
//! - [`ClientTls`] — `nucleus_client::Tls`, for the Rust client's `Config`:
//!   the server's certificate checked against Mozilla's roots
//!   ([`ClientTls::new`]) or a CA of one's own ([`ClientTls::with_ca_pem`]),
//!   or not at all ([`ClientTls::without_verification`]: encrypted, not
//!   authenticated).
//!
//! TLS 1.2 and 1.3, with rustls's safe defaults and ring's cryptography.

use std::io;
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

fn certificates(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("bad certificate PEM: {e}")))?;
    if certs.is_empty() {
        return Err(invalid("no certificate in the PEM"));
    }
    Ok(certs)
}

fn read(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// Runs a handshake to its end, so a failure shows here and not at the
/// first message.
fn handshake<C, D>(s: &mut rustls::StreamOwned<C, TcpStream>) -> io::Result<()>
where
    C: std::ops::DerefMut<Target = rustls::ConnectionCommon<D>>,
    D: rustls::SideData,
{
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock)?;
    }
    Ok(())
}

// ----------------------------------------------------------------- server

/// The server side: a certificate chain and its key, and — optionally —
/// the CA whose client certificates it accepts.
#[derive(Clone)]
pub struct ServerTls {
    chain: Vec<CertificateDer<'static>>,
    key: Arc<PrivateKeyDer<'static>>,
    client_roots: Option<Arc<rustls::RootCertStore>>,
    config: Arc<rustls::ServerConfig>,
    /// SCRAM's channel binding for this certificate.
    end_point: Option<Vec<u8>>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls")
            .field("certificates", &self.chain.len())
            .field("client_certificates", &self.client_roots.is_some())
            .finish_non_exhaustive()
    }
}

fn roots(pem: &[u8]) -> io::Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    for c in certificates(pem)? {
        roots
            .add(c)
            .map_err(|e| invalid(format!("bad CA certificate: {e}")))?;
    }
    Ok(roots)
}

fn private_key(pem: &[u8]) -> io::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem).map_err(|e| invalid(format!("bad private key PEM: {e}")))
}

impl ServerTls {
    /// From PEM: the certificate chain (the server's own first) and its
    /// private key (PKCS #8, PKCS #1 or SEC1).
    pub fn from_pem(chain: &[u8], key: &[u8]) -> io::Result<Self> {
        Self::build(certificates(chain)?, Arc::new(private_key(key)?), None)
    }

    /// From PEM files.
    pub fn from_pem_files(chain: &Path, key: &Path) -> io::Result<Self> {
        Self::from_pem(&read(chain)?, &read(key)?)
    }

    /// The same, also asking clients for a certificate of the CA in `pem`:
    /// one is not required, but one that is sent must verify, and its
    /// subject's common name is [`nucleus_server::TlsInfo::client_name`].
    pub fn accepting_client_certificates(self, pem: &[u8]) -> io::Result<Self> {
        Self::build(self.chain, self.key, Some(Arc::new(roots(pem)?)))
    }

    /// The same, the CA from a file.
    pub fn accepting_client_certificates_file(self, path: &Path) -> io::Result<Self> {
        self.accepting_client_certificates(&read(path)?)
    }

    fn build(
        chain: Vec<CertificateDer<'static>>,
        key: Arc<PrivateKeyDer<'static>>,
        client_roots: Option<Arc<rustls::RootCertStore>>,
    ) -> io::Result<Self> {
        let builder = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?;
        let builder = match &client_roots {
            Some(r) => builder.with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::clone(r),
                    provider(),
                )
                .allow_unauthenticated()
                .build()
                .map_err(invalid)?,
            ),
            None => builder.with_no_client_auth(),
        };
        let config = builder
            .with_single_cert(chain.clone(), key.clone_key())
            .map_err(invalid)?;
        Ok(ServerTls {
            end_point: der::server_end_point(&chain[0]),
            chain,
            key,
            client_roots,
            config: Arc::new(config),
        })
    }
}

fn version_name(v: Option<rustls::ProtocolVersion>) -> String {
    match v {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3".into(),
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2".into(),
        Some(v) => format!("{v:?}"),
        None => String::new(),
    }
}

impl nucleus_server::Tls for ServerTls {
    fn accept(&self, tcp: TcpStream) -> io::Result<nucleus_server::Secured> {
        let conn = rustls::ServerConnection::new(Arc::clone(&self.config)).map_err(invalid)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        handshake(&mut s)?;
        // A certificate is there only if it verified (rustls refuses the
        // handshake otherwise).
        let client_certificates: Vec<Vec<u8>> = s
            .conn
            .peer_certificates()
            .map(|c| c.iter().map(|d| d.to_vec()).collect())
            .unwrap_or_default();
        let info = nucleus_server::TlsInfo {
            version: version_name(s.conn.protocol_version()),
            cipher: s
                .conn
                .negotiated_cipher_suite()
                .and_then(|c| c.suite().as_str())
                .unwrap_or_default()
                .to_string(),
            client_name: client_certificates
                .first()
                .and_then(|c| der::common_name(c)),
            client_certificates,
            channel_binding: self.end_point.clone(),
        };
        Ok(nucleus_server::Secured {
            stream: Box::new(s),
            info,
        })
    }
}

// ----------------------------------------------------------------- client

/// The client side: how it checks the server, and — optionally — the
/// certificate it shows.
#[derive(Clone)]
pub struct ClientTls {
    /// `None`: no check.
    roots: Option<Arc<rustls::RootCertStore>>,
    identity: Option<(Vec<CertificateDer<'static>>, Arc<PrivateKeyDer<'static>>)>,
    config: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTls")
            .field("verifies", &self.roots.is_some())
            .field("certificate", &self.identity.is_some())
            .finish_non_exhaustive()
    }
}

impl ClientTls {
    /// Checks the server's certificate against Mozilla's root certificates,
    /// and that it names the host connected to.
    pub fn new() -> Self {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Self::build(Some(Arc::new(roots)), None).expect("the safe defaults")
    }

    /// Checks the server's certificate against the CA certificates in
    /// `pem` (and the host name).
    pub fn with_ca_pem(pem: &[u8]) -> io::Result<Self> {
        Self::build(Some(Arc::new(roots(pem)?)), None)
    }

    /// The same, from a file.
    pub fn with_ca_file(path: &Path) -> io::Result<Self> {
        Self::with_ca_pem(&read(path)?)
    }

    /// Encrypts without checking who the server is: anyone in the middle
    /// can pose as it.  For tests and networks one trusts anyway.
    pub fn without_verification() -> Self {
        Self::build(None, None).expect("the safe defaults")
    }

    /// The same, showing the server a certificate (PEM: the chain, the
    /// client's own first, and its private key) — mutual TLS; a server
    /// that accepts it logs its common name in without a password.
    pub fn with_client_certificate(self, chain: &[u8], key: &[u8]) -> io::Result<Self> {
        Self::build(
            self.roots,
            Some((certificates(chain)?, Arc::new(private_key(key)?))),
        )
    }

    /// The same, from files.
    pub fn with_client_certificate_files(self, chain: &Path, key: &Path) -> io::Result<Self> {
        self.with_client_certificate(&read(chain)?, &read(key)?)
    }

    fn build(
        roots: Option<Arc<rustls::RootCertStore>>,
        identity: Option<(Vec<CertificateDer<'static>>, Arc<PrivateKeyDer<'static>>)>,
    ) -> io::Result<Self> {
        let p = provider();
        let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&p))
            .with_safe_default_protocol_versions()
            .map_err(invalid)?;
        let builder = match &roots {
            Some(r) => builder.with_root_certificates(Arc::clone(r)),
            None => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification(p))),
        };
        let config = match &identity {
            Some((chain, key)) => builder
                .with_client_auth_cert(chain.clone(), key.clone_key())
                .map_err(invalid)?,
            None => builder.with_no_client_auth(),
        };
        Ok(ClientTls {
            roots,
            identity,
            config: Arc::new(config),
        })
    }
}

impl Default for ClientTls {
    fn default() -> Self {
        Self::new()
    }
}

impl nucleus_client::Tls for ClientTls {
    fn connect(&self, tcp: TcpStream, host: &str) -> io::Result<nucleus_client::Secured> {
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| invalid(format!("not a host name: {host}")))?;
        let conn =
            rustls::ClientConnection::new(Arc::clone(&self.config), name).map_err(invalid)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        handshake(&mut s)?;
        let channel_binding = s
            .conn
            .peer_certificates()
            .and_then(|c| c.first())
            .and_then(|c| der::server_end_point(c));
        Ok(nucleus_client::Secured {
            stream: Box::new(s),
            channel_binding,
        })
    }
}

/// Accepts any certificate, but still checks the handshake's signatures
/// (so the session key is the certificate holder's, whoever that is).
#[derive(Debug)]
struct NoVerification(Arc<CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Just enough DER to read a certificate's subject common name.
mod der {
    /// One element: its tag, its contents, and what follows it.
    fn element(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = b.split_first()?;
        let (&first, mut rest) = rest.split_first()?;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 || rest.len() < n {
                return None;
            }
            let (l, r) = rest.split_at(n);
            rest = r;
            l.iter().fold(0usize, |a, &x| (a << 8) | usize::from(x))
        };
        if rest.len() < len {
            return None;
        }
        let (contents, after) = rest.split_at(len);
        Some((tag, contents, after))
    }

    /// RFC 5929's `tls-server-end-point`: the certificate hashed with its
    /// signature's hash (SHA-256 for MD5 and SHA-1); `None` for a signature
    /// that names none (RSASSA-PSS's is in its parameters, Ed25519 has none),
    /// as PostgreSQL has.
    pub fn server_end_point(cert: &[u8]) -> Option<Vec<u8>> {
        let (SEQUENCE, body, _) = element(cert)? else {
            return None;
        };
        let after_tbs = element(body)?.2;
        let (SEQUENCE, algorithm, _) = element(after_tbs)? else {
            return None;
        };
        let (0x06, oid, _) = element(algorithm)? else {
            return None;
        };
        const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01];
        const ECDSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04];
        let hash = match oid {
            [r @ .., last] if r == RSA => match last {
                4 | 5 | 11 => &ring::digest::SHA256,
                12 => &ring::digest::SHA384,
                13 => &ring::digest::SHA512,
                _ => return None,
            },
            [e @ .., 1] if e == ECDSA => &ring::digest::SHA256,
            [e @ .., 3, n] if e == ECDSA => match n {
                2 => &ring::digest::SHA256,
                3 => &ring::digest::SHA384,
                4 => &ring::digest::SHA512,
                _ => return None,
            },
            _ => return None,
        };
        Some(ring::digest::digest(hash, cert).as_ref().to_vec())
    }

    const SEQUENCE: u8 = 0x30;
    const SET: u8 = 0x31;
    /// 2.5.4.3, commonName.
    const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

    /// The subject's (last) common name, if the certificate has one in a
    /// string type.
    pub fn common_name(cert: &[u8]) -> Option<String> {
        let (SEQUENCE, cert, _) = element(cert)? else {
            return None;
        };
        let (SEQUENCE, mut tbs, _) = element(cert)? else {
            return None;
        };
        // [0] version (optional), serial, signature, issuer, validity, subject.
        if tbs.first() == Some(&0xa0) {
            tbs = element(tbs)?.2;
        }
        for _ in 0..4 {
            tbs = element(tbs)?.2;
        }
        let (SEQUENCE, mut name, _) = element(tbs)? else {
            return None;
        };
        let mut found = None;
        while !name.is_empty() {
            let (SET, mut rdn, rest) = element(name)? else {
                return None;
            };
            name = rest;
            while !rdn.is_empty() {
                let (SEQUENCE, atv, rest) = element(rdn)? else {
                    return None;
                };
                rdn = rest;
                let (0x06, oid, value) = element(atv)? else {
                    return None;
                };
                let (tag, text, _) = element(value)?;
                // UTF8String, PrintableString, IA5String, T61String.
                if oid == COMMON_NAME && matches!(tag, 0x0c | 0x13 | 0x16 | 0x14) {
                    found = Some(String::from_utf8(text.to_vec()).ok()?);
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::der;

    fn certificate(alg: &'static rcgen::SignatureAlgorithm, cn: &str) -> Vec<u8> {
        let mut p = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        p.distinguished_name.push(rcgen::DnType::CommonName, cn);
        let key = rcgen::KeyPair::generate_for(alg).unwrap();
        p.self_signed(&key).unwrap().der().to_vec()
    }

    #[test]
    fn common_names() {
        let c = certificate(&rcgen::PKCS_ECDSA_P256_SHA256, "alice");
        assert_eq!(der::common_name(&c).as_deref(), Some("alice"));
        assert_eq!(der::common_name(b"\x30\x03\x02\x01"), None);
        assert_eq!(der::common_name(&c[..c.len() / 2]), None);
    }

    /// The end point is hashed by the signature's hash.
    #[test]
    fn server_end_points() {
        let c = certificate(&rcgen::PKCS_ECDSA_P256_SHA256, "x");
        let want = ring::digest::digest(&ring::digest::SHA256, &c);
        assert_eq!(der::server_end_point(&c).as_deref(), Some(want.as_ref()));
        let c = certificate(&rcgen::PKCS_ECDSA_P384_SHA384, "x");
        let want = ring::digest::digest(&ring::digest::SHA384, &c);
        assert_eq!(der::server_end_point(&c).as_deref(), Some(want.as_ref()));
        // Ed25519 names no hash: no channel binding.
        let c = certificate(&rcgen::PKCS_ED25519, "x");
        assert_eq!(der::server_end_point(&c), None);
    }
}
