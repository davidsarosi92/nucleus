//! LDAP checks against a small directory in the test (it speaks enough of
//! RFC 4511: bind, search, unbind), and logins through the server — the
//! password only over TLS.  The module was also checked against OpenLDAP
//! 2.6 (bind, search as an account, anonymous search).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use nucleus_client::{Client, Config, Error, Value};
use nucleus_kv::{Database, Kv, Options};
use nucleus_ldap::Ldap;
use nucleus_server::Directory;

// ------------------------------------------------------------ a directory

fn tlv(tag: u8, c: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if c.len() < 0x80 {
        out.push(c.len() as u8);
    } else {
        out.extend([0x82, (c.len() >> 8) as u8, c.len() as u8]);
    }
    out.extend_from_slice(c);
    out
}

fn split(b: &[u8]) -> (u8, &[u8], &[u8]) {
    let (len, at) = if b[1] < 0x80 {
        (usize::from(b[1]), 2)
    } else {
        let n = usize::from(b[1] & 0x7f);
        (
            b[2..2 + n]
                .iter()
                .fold(0, |a, &x| (a << 8) | usize::from(x)),
            2 + n,
        )
    };
    (b[0], &b[at..at + len], &b[at + len..])
}

fn result(tag: u8, code: u8) -> Vec<u8> {
    tlv(
        tag,
        &[tlv(0x0a, &[code]), tlv(4, b""), tlv(4, b"")].concat(),
    )
}

/// A directory: entries by DN, with a password and attributes.
#[derive(Default)]
struct Directory_ {
    entries: BTreeMap<String, (String, BTreeMap<String, String>)>,
    /// What it was asked: (operation, DN or value).
    log: Mutex<Vec<(String, String)>>,
}

fn serve(dir: Arc<Directory_>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let dir = Arc::clone(&dir);
            std::thread::spawn(move || conversation(s.unwrap(), &dir));
        }
    });
    addr
}

fn conversation(mut s: TcpStream, dir: &Directory_) {
    loop {
        let mut head = [0u8; 2];
        if s.read_exact(&mut head).is_err() {
            return;
        }
        let mut len = usize::from(head[1]);
        let mut lenbytes = vec![];
        if head[1] & 0x80 != 0 {
            lenbytes = vec![0u8; usize::from(head[1] & 0x7f)];
            s.read_exact(&mut lenbytes).unwrap();
            len = lenbytes.iter().fold(0, |a, &x| (a << 8) | usize::from(x));
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).unwrap();
        let (_, id, rest) = split(&body);
        let id = tlv(2, id);
        let (op, contents, _) = split(rest);
        let reply = |ops: Vec<Vec<u8>>, s: &mut TcpStream| {
            for o in ops {
                s.write_all(&tlv(0x30, &[id.clone(), o].concat())).unwrap();
            }
        };
        match op {
            0x60 => {
                let (_, _version, rest) = split(contents);
                let (_, dn, rest) = split(rest);
                let (_, password, _) = split(rest);
                let (dn, password) = (
                    String::from_utf8_lossy(dn).into_owned(),
                    String::from_utf8_lossy(password).into_owned(),
                );
                dir.log.lock().unwrap().push(("bind".into(), dn.clone()));
                let ok = dir.entries.get(&dn).is_some_and(|(pw, _)| *pw == password);
                reply(vec![result(0x61, if ok { 0 } else { 49 })], &mut s);
            }
            0x63 => {
                let (_, base, rest) = split(contents);
                let base = String::from_utf8_lossy(base).into_owned();
                let mut rest = rest;
                for _ in 0..5 {
                    rest = split(rest).2;
                }
                let (tag, filter, _) = split(rest);
                assert_eq!(tag, 0xa3, "an equality match");
                let (_, attr, more) = split(filter);
                let (_, value, _) = split(more);
                let (attr, value) = (
                    String::from_utf8_lossy(attr).into_owned(),
                    String::from_utf8_lossy(value).into_owned(),
                );
                dir.log
                    .lock()
                    .unwrap()
                    .push(("search".into(), value.clone()));
                let mut ops: Vec<Vec<u8>> = dir
                    .entries
                    .iter()
                    .filter(|(dn, (_, attrs))| {
                        dn.ends_with(&base) && attrs.get(&attr) == Some(&value)
                    })
                    .map(|(dn, _)| tlv(0x64, &[tlv(4, dn.as_bytes()), tlv(0x30, b"")].concat()))
                    .collect();
                ops.push(result(0x65, 0));
                reply(ops, &mut s);
            }
            _ => return, // unbind
        }
    }
}

fn directory() -> Arc<Directory_> {
    let mut d = Directory_::default();
    let entry = |pw: &str, uid: &str| {
        (
            pw.to_string(),
            BTreeMap::from([("uid".to_string(), uid.to_string())]),
        )
    };
    d.entries.insert(
        "cn=alice,ou=users,dc=test".into(),
        entry("alice-pw", "alice"),
    );
    d.entries
        .insert("cn=service,dc=test".into(), entry("service-pw", "service"));
    // Two entries with the same uid: ambiguous.
    d.entries
        .insert("cn=twin1,ou=users,dc=test".into(), entry("x", "twin"));
    d.entries
        .insert("cn=twin2,ou=users,dc=test".into(), entry("x", "twin"));
    Arc::new(d)
}

// ------------------------------------------------------------------ tests

#[test]
fn bind_checks_the_password() {
    let dir = directory();
    let addr = serve(Arc::clone(&dir));
    let ldap = Ldap::bind(&format!("ldap://{addr}"), "cn={user},ou=users,dc=test").unwrap();
    assert_eq!(ldap.check("alice", "alice-pw"), Ok(true));
    assert_eq!(ldap.check("alice", "wrong"), Ok(false));
    assert_eq!(ldap.check("bob", "x"), Ok(false));
    // An empty password is never sent (it would be an anonymous bind).
    let before = dir.log.lock().unwrap().len();
    assert_eq!(ldap.check("alice", ""), Ok(false));
    assert_eq!(dir.log.lock().unwrap().len(), before);
    // A name cannot change the DN.
    assert_eq!(ldap.check("alice,ou=users,dc=test", "alice-pw"), Ok(false));
    assert!(dir.log.lock().unwrap().contains(&(
        "bind".into(),
        "cn=alice\\,ou\\=users\\,dc\\=test,ou=users,dc=test".into()
    )));
}

#[test]
fn search_then_bind() {
    let dir = directory();
    let addr = serve(Arc::clone(&dir));
    let url = format!("ldap://{addr}");
    let ldap = Ldap::search(&url, "dc=test", "uid").unwrap();
    assert_eq!(ldap.check("alice", "alice-pw"), Ok(true));
    assert_eq!(ldap.check("alice", "nope"), Ok(false));
    // None found, or more than one: no login.
    assert_eq!(ldap.check("nobody", "x"), Ok(false));
    assert_eq!(ldap.check("twin", "x"), Ok(false));
    // The name is a value in the search, never filter text.
    assert_eq!(ldap.check("*)(uid=*", "x"), Ok(false));
    assert!(
        dir.log
            .lock()
            .unwrap()
            .contains(&("search".into(), "*)(uid=*".into()))
    );
    // As a search account.
    let ldap = Ldap::search(&url, "dc=test", "uid")
        .unwrap()
        .with_search_account("cn=service,dc=test", "service-pw");
    assert_eq!(ldap.check("alice", "alice-pw"), Ok(true));
    let ldap = Ldap::search(&url, "dc=test", "uid")
        .unwrap()
        .with_search_account("cn=service,dc=test", "wrong");
    assert!(ldap.check("alice", "alice-pw").is_err());
}

// ---------------------------------------------------- logins over the wire

/// TLS for the test's server and client, with rustls directly.
#[derive(Debug)]
struct TestTls {
    server: Arc<rustls::ServerConfig>,
}

impl nucleus_server::Tls for TestTls {
    fn accept(&self, tcp: TcpStream) -> std::io::Result<nucleus_server::Secured> {
        let conn = rustls::ServerConnection::new(Arc::clone(&self.server)).unwrap();
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        Ok(nucleus_server::Secured {
            stream: Box::new(s),
            info: Default::default(),
        })
    }
}

#[derive(Debug)]
struct TestClientTls {
    client: Arc<rustls::ClientConfig>,
}

impl nucleus_client::Tls for TestClientTls {
    fn connect(&self, tcp: TcpStream, host: &str) -> std::io::Result<nucleus_client::Secured> {
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
        let conn = rustls::ClientConnection::new(Arc::clone(&self.client), name).unwrap();
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        Ok(nucleus_client::Secured {
            stream: Box::new(s),
            channel_binding: None,
        })
    }
}

fn tls_pair() -> (TestTls, TestClientTls) {
    let provider = || Arc::new(rustls::crypto::ring::default_provider());
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let der = cert.der().clone();
    let server = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (
        TestTls {
            server: Arc::new(server),
        },
        TestClientTls {
            client: Arc::new(client),
        },
    )
}

#[test]
fn a_directory_user_logs_in_over_tls_only() {
    let addr = serve(directory());
    let ldap = Ldap::bind(&format!("ldap://{addr}"), "cn={user},ou=users,dc=test").unwrap();
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nucleus-ldap-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db: Arc<dyn Kv> = Arc::new(Database::open(&dir, Options::default()).unwrap());
    let (server_tls, client_tls) = tls_pair();
    let client_tls = Arc::new(client_tls);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.local_addr().unwrap();
    let config = nucleus_server::Config::default()
        .with_tls(Arc::new(server_tls))
        .with_directory(Arc::new(ldap));
    std::thread::spawn(move || nucleus_server::serve(listener, db, config));
    let login = |user: &str, password: &str, tls: bool| {
        Client::connect(Config {
            host: "localhost".into(),
            port: server.port(),
            user: user.into(),
            password: Some(password.into()),
            tls: tls.then(|| Arc::clone(&client_tls) as Arc<dyn nucleus_client::Tls>),
            ..Config::default()
        })
    };
    login("nucleus", "nucleus", false)
        .unwrap()
        .execute("CREATE USER alice", &[])
        .unwrap();
    let mut c = login("alice", "alice-pw", true).unwrap();
    assert_eq!(
        c.rows("SELECT current_user", &[]).unwrap(),
        vec![vec![Value::from("alice")]]
    );
    let code = |r: Result<Client, Error>| match r {
        Err(Error::Server(e)) => e.code,
        Err(e) => format!("{e}"),
        Ok(_) => "ok".into(),
    };
    assert_eq!(code(login("alice", "wrong", true)), "28P01");
    assert_eq!(code(login("alice", "", true)), "28P01");
    // Without TLS the server does not ask for the password at all.
    assert_eq!(code(login("alice", "alice-pw", false)), "28000");
    // Claimed, the directory still checks alice's password; a role with a
    // password of its own keeps SCRAM.
    login("nucleus", "nucleus", false)
        .unwrap()
        .execute("CREATE USER bob SUPERUSER PASSWORD 'bob-pw'", &[])
        .unwrap();
    assert!(login("bob", "bob-pw", false).is_ok());
    assert!(login("alice", "alice-pw", true).is_ok());
}
