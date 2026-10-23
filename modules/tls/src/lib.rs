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

/// The server side: a certificate chain and its key.
#[derive(Debug, Clone)]
pub struct ServerTls {
    config: Arc<rustls::ServerConfig>,
}

impl ServerTls {
    /// From PEM: the certificate chain (the server's own first) and its
    /// private key (PKCS #8, PKCS #1 or SEC1).
    pub fn from_pem(chain: &[u8], key: &[u8]) -> io::Result<Self> {
        let key = PrivateKeyDer::from_pem_slice(key)
            .map_err(|e| invalid(format!("bad private key PEM: {e}")))?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_no_client_auth()
            .with_single_cert(certificates(chain)?, key)
            .map_err(invalid)?;
        Ok(ServerTls {
            config: Arc::new(config),
        })
    }

    /// From PEM files.
    pub fn from_pem_files(chain: &Path, key: &Path) -> io::Result<Self> {
        Self::from_pem(&read(chain)?, &read(key)?)
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
        let info = nucleus_server::TlsInfo {
            version: version_name(s.conn.protocol_version()),
            cipher: s
                .conn
                .negotiated_cipher_suite()
                .and_then(|c| c.suite().as_str())
                .unwrap_or_default()
                .to_string(),
            client_certificates: s
                .conn
                .peer_certificates()
                .map(|c| c.iter().map(|d| d.to_vec()).collect())
                .unwrap_or_default(),
        };
        Ok(nucleus_server::Secured {
            stream: Box::new(s),
            info,
        })
    }
}

// ----------------------------------------------------------------- client

/// The client side.
#[derive(Debug, Clone)]
pub struct ClientTls {
    config: Arc<rustls::ClientConfig>,
}

impl ClientTls {
    /// Checks the server's certificate against Mozilla's root certificates,
    /// and that it names the host connected to.
    pub fn new() -> Self {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Self::with_roots(roots).expect("the safe defaults")
    }

    /// Checks the server's certificate against the CA certificates in
    /// `pem` (and the host name).
    pub fn with_ca_pem(pem: &[u8]) -> io::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        for c in certificates(pem)? {
            roots
                .add(c)
                .map_err(|e| invalid(format!("bad CA certificate: {e}")))?;
        }
        Self::with_roots(roots)
    }

    /// The same, from a file.
    pub fn with_ca_file(path: &Path) -> io::Result<Self> {
        Self::with_ca_pem(&read(path)?)
    }

    fn with_roots(roots: rustls::RootCertStore) -> io::Result<Self> {
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(ClientTls {
            config: Arc::new(config),
        })
    }

    /// Encrypts without checking who the server is: anyone in the middle
    /// can pose as it.  For tests and networks one trusts anyway.
    pub fn without_verification() -> Self {
        let p = provider();
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&p))
            .with_safe_default_protocol_versions()
            .expect("the safe defaults")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification(p)))
            .with_no_client_auth();
        ClientTls {
            config: Arc::new(config),
        }
    }
}

impl Default for ClientTls {
    fn default() -> Self {
        Self::new()
    }
}

impl nucleus_client::Tls for ClientTls {
    fn connect(&self, tcp: TcpStream, host: &str) -> io::Result<Box<dyn nucleus_client::Stream>> {
        let name = ServerName::try_from(host.to_string())
            .map_err(|_| invalid(format!("not a host name: {host}")))?;
        let conn =
            rustls::ClientConnection::new(Arc::clone(&self.config), name).map_err(invalid)?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        handshake(&mut s)?;
        Ok(Box::new(s))
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
