//! TLS end to end: a server with a certificate of its own CA, and the Rust
//! client checking it, not checking it, or not asking for TLS at all.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use nucleus_client::{Client, Config, Error, Value};
use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_tls::{ClientTls, ServerTls};

/// A CA, and a server certificate for `localhost` it signed: (the CA's
/// PEM, the server's chain PEM, its key PEM).
fn certificates() -> (String, String, String) {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "nucleus test CA");
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = ca.self_signed(&ca_key).unwrap();
    let server = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = server.signed_by(&key, &ca, &ca_key).unwrap();
    (ca.pem(), cert.pem(), key.serialize_pem())
}

fn start(tls: Option<ServerTls>) -> SocketAddr {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = nucleus_server::Config::default();
    if let Some(t) = tls {
        config = config.with_tls(Arc::new(t));
    }
    std::thread::spawn(move || nucleus_server::serve(listener, db, config));
    addr
}

fn connect(addr: SocketAddr, host: &str, tls: Option<ClientTls>) -> Result<Client, Error> {
    Client::connect(Config {
        host: host.into(),
        port: addr.port(),
        tls: tls.map(|t| Arc::new(t) as Arc<dyn nucleus_client::Tls>),
        ..Config::default()
    })
}

#[test]
fn a_verified_server_serves_over_tls() {
    let (ca, chain, key) = certificates();
    let addr = start(Some(
        ServerTls::from_pem(chain.as_bytes(), key.as_bytes()).unwrap(),
    ));
    let tls = ClientTls::with_ca_pem(ca.as_bytes()).unwrap();
    let mut c = connect(addr, "localhost", Some(tls.clone())).unwrap();
    assert_eq!(
        c.rows("SELECT 1 + 1", &[]).unwrap(),
        vec![vec![Value::Integer(2)]]
    );
    // A large result crosses many TLS records.
    let rows = c
        .rows(
            "SELECT repeat('x', 100000) FROM generate_series(1, 20)",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 20);
    // The certificate names localhost, not 127.0.0.1; Mozilla's roots do
    // not know the CA.
    assert!(connect(addr, "127.0.0.1", Some(tls)).is_err());
    assert!(connect(addr, "localhost", Some(ClientTls::new())).is_err());
    // Without verification it is encrypted all the same; and TLS is
    // offered, not required: a plain client still connects.
    let mut c = connect(addr, "localhost", Some(ClientTls::without_verification())).unwrap();
    assert_eq!(
        c.rows("SELECT 'ok'", &[]).unwrap(),
        vec![vec![Value::from("ok")]]
    );
    assert!(connect(addr, "localhost", None).is_ok());
}

#[test]
fn a_server_without_tls_refuses_a_tls_client() {
    let addr = start(None);
    assert!(connect(addr, "localhost", Some(ClientTls::without_verification())).is_err());
    assert!(connect(addr, "localhost", None).is_ok());
}

#[test]
fn bad_pem_is_refused() {
    let (_, chain, _) = certificates();
    assert!(ServerTls::from_pem(chain.as_bytes(), b"not a key").is_err());
    assert!(ServerTls::from_pem(b"", b"").is_err());
    assert!(ClientTls::with_ca_pem(b"nothing").is_err());
}
