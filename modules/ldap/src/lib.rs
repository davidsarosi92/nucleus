//! LDAP for nucleus (PLAN.md §M14.25): passwords checked by a directory.
//!
//! An [`Ldap`] is a `nucleus_server::Directory`: for a role without a
//! password of its own, the server takes the password the client sends
//! (only over TLS) and asks the directory, by an LDAP simple bind:
//!
//! - **bind** ([`Ldap::bind`]): as the DN a template makes of the user
//!   name (`uid={user},ou=people,dc=example,dc=com`);
//! - **search and bind** ([`Ldap::search`]): first find the one entry whose
//!   attribute (`uid`) is the user name under a base — anonymously or as a
//!   search account — then bind as its DN.
//!
//! The user name is escaped in the DN (RFC 4514) and is a value, never
//! filter text, in the search, so no name can change what is asked; an
//! empty password is never sent (a bind with one is anonymous, and
//! succeeds).  `ldaps://` speaks TLS from the start, `starttls` upgrades an
//! `ldap://` connection; the directory's certificate is checked against
//! Mozilla's roots or a CA of one's own.

mod ber;

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ber::{BOOLEAN, ENUMERATED, Element, Elements, INTEGER, OCTET_STRING, SEQUENCE};

const TIMEOUT: Duration = Duration::from_secs(10);
const STARTTLS: &[u8] = b"1.3.6.1.4.1.1466.20037";
/// resultCode success, invalidCredentials (RFC 4511 §4.1.9).
const SUCCESS: i64 = 0;
const INVALID_CREDENTIALS: i64 = 49;

/// How the user's entry is found.
#[derive(Clone, Debug)]
enum Mode {
    Bind {
        template: String,
    },
    Search {
        base: String,
        attribute: String,
        account: Option<(String, String)>,
    },
}

/// A directory.
#[derive(Clone)]
pub struct Ldap {
    host: String,
    port: u16,
    ldaps: bool,
    starttls: bool,
    roots: Arc<rustls::RootCertStore>,
    mode: Mode,
}

impl std::fmt::Debug for Ldap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ldap")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("ldaps", &self.ldaps)
            .field("starttls", &self.starttls)
            .finish_non_exhaustive()
    }
}

impl Ldap {
    fn new(url: &str, mode: Mode) -> Result<Ldap, String> {
        let (ldaps, rest) = if let Some(r) = url.strip_prefix("ldaps://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("ldap://") {
            (false, r)
        } else {
            return Err(format!("{url}: not an ldap:// or ldaps:// URL"));
        };
        let authority = rest.split('/').next().unwrap_or_default();
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') || h.ends_with(']') => {
                (h, p.parse().map_err(|_| format!("{url}: a bad port"))?)
            }
            _ => (authority, if ldaps { 636 } else { 389 }),
        };
        if host.is_empty() {
            return Err(format!("{url}: no host"));
        }
        Ok(Ldap {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port,
            ldaps,
            starttls: false,
            roots: Arc::new(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            }),
            mode,
        })
    }

    /// Binds as the DN `template` makes of the user name (`{user}`, escaped
    /// as RFC 4514 says).
    pub fn bind(url: &str, template: &str) -> Result<Ldap, String> {
        if !template.contains("{user}") {
            return Err("the DN template needs {user}".into());
        }
        Ldap::new(
            url,
            Mode::Bind {
                template: template.into(),
            },
        )
    }

    /// Finds the entry under `base` whose `attribute` is the user name,
    /// then binds as it.
    pub fn search(url: &str, base: &str, attribute: &str) -> Result<Ldap, String> {
        Ldap::new(
            url,
            Mode::Search {
                base: base.into(),
                attribute: attribute.into(),
                account: None,
            },
        )
    }

    /// Searches as this account (else anonymously).
    pub fn with_search_account(mut self, dn: &str, password: &str) -> Ldap {
        if let Mode::Search { account, .. } = &mut self.mode {
            *account = Some((dn.into(), password.into()));
        }
        self
    }

    /// Upgrades an `ldap://` connection to TLS (StartTLS) before binding.
    pub fn starttls(mut self) -> Ldap {
        self.starttls = true;
        self
    }

    /// Checks the directory's certificate against the CA certificates in
    /// `pem` (else Mozilla's roots).
    pub fn with_ca_pem(mut self, pem: &[u8]) -> Result<Ldap, String> {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject;
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(pem) {
            roots
                .add(c.map_err(|e| format!("bad CA PEM: {e}"))?)
                .map_err(|e| format!("bad CA certificate: {e}"))?;
        }
        if roots.is_empty() {
            return Err("no CA certificate in the PEM".into());
        }
        self.roots = Arc::new(roots);
        Ok(self)
    }

    /// The same, from a file.
    pub fn with_ca_file(self, path: &Path) -> Result<Ldap, String> {
        let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.with_ca_pem(&pem)
    }

    fn connect(&self) -> Result<Conn, String> {
        let fail = |e: &dyn std::fmt::Display| format!("{}:{}: {e}", self.host, self.port);
        let mut last = None;
        let mut tcp = None;
        for a in (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| fail(&e))?
        {
            match TcpStream::connect_timeout(&a, TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| fail(&last.map_or("no address".into(), |e| e.to_string())))?;
        tcp.set_read_timeout(Some(TIMEOUT)).map_err(|e| fail(&e))?;
        tcp.set_write_timeout(Some(TIMEOUT)).map_err(|e| fail(&e))?;
        let mut conn = Conn {
            stream: Box::new(tcp.try_clone().map_err(|e| fail(&e))?),
            next: 1,
        };
        if self.ldaps {
            conn.stream = self.tls(tcp)?;
        } else if self.starttls {
            let result =
                conn.request(&ber::sequence(0x77, &[ber::octets(0x80, STARTTLS)]), 0x78)?;
            if result.code != SUCCESS {
                return Err(fail(&format!("StartTLS refused: {}", result.message)));
            }
            conn.stream = self.tls(tcp)?;
        }
        Ok(conn)
    }

    fn tls(&self, tcp: TcpStream) -> Result<Box<dyn ReadWrite>, String> {
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(Arc::clone(&self.roots))
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(self.host.clone())
            .map_err(|e| e.to_string())?;
        let conn =
            rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn
                .complete_io(&mut s.sock)
                .map_err(|e| format!("TLS with the directory: {e}"))?;
        }
        Ok(Box::new(s))
    }
}

/// RFC 4514's escaping of a value in a DN.
fn escape_dn(v: &str) -> String {
    let mut out = String::new();
    let last = v.chars().count().saturating_sub(1);
    for (i, c) in v.chars().enumerate() {
        match c {
            ',' | '+' | '"' | '\\' | '<' | '>' | ';' | '=' => {
                out.push('\\');
                out.push(c);
            }
            '\0' => out.push_str("\\00"),
            ' ' if i == 0 || i == last => out.push_str("\\ "),
            '#' if i == 0 => out.push_str("\\#"),
            _ => out.push(c),
        }
    }
    out
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

struct Conn {
    stream: Box<dyn ReadWrite>,
    next: i64,
}

/// An LDAPResult: its code and diagnostic message.
struct Outcome {
    code: i64,
    message: String,
}

fn result(contents: &[u8]) -> Result<Outcome, String> {
    let mut parts = Elements(contents);
    let code = ber::int(parts.next().ok_or("no result code")??)?;
    let _matched = parts.next();
    let message = parts
        .next()
        .transpose()?
        .map(|e| String::from_utf8_lossy(e.contents).into_owned())
        .unwrap_or_default();
    Ok(Outcome { code, message })
}

impl Conn {
    fn send(&mut self, op: &[u8]) -> Result<i64, String> {
        let id = self.next;
        self.next += 1;
        let msg = ber::sequence(SEQUENCE, &[ber::integer(INTEGER, id), op.to_vec()]);
        self.stream
            .write_all(&msg)
            .and_then(|()| self.stream.flush())
            .map_err(|e| format!("writing to the directory: {e}"))?;
        Ok(id)
    }

    /// The next message for `id`: its operation's tag and contents.
    fn receive(&mut self, id: i64) -> Result<(u8, Vec<u8>), String> {
        loop {
            let io = |e: std::io::Error| format!("reading from the directory: {e}");
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).map_err(io)?;
            if head[0] != SEQUENCE {
                return Err("a malformed LDAP message".into());
            }
            let mut len_bytes = vec![head[1]];
            if head[1] & 0x80 != 0 {
                let n = usize::from(head[1] & 0x7f);
                if n == 0 || n > 4 {
                    return Err("a malformed LDAP message".into());
                }
                let mut more = vec![0u8; n];
                self.stream.read_exact(&mut more).map_err(io)?;
                len_bytes.extend(more);
            }
            let (len, _) = ber::length(&len_bytes).ok_or("a malformed LDAP message")?;
            if len > 1 << 20 {
                return Err("an LDAP message too large".into());
            }
            let mut body = vec![0u8; len];
            self.stream.read_exact(&mut body).map_err(io)?;
            let mut parts = Elements(&body);
            let got = ber::int(parts.next().ok_or("no message id")??)?;
            let op: Element<'_> = parts.next().ok_or("no operation")??;
            if got == id {
                return Ok((op.tag, op.contents.to_vec()));
            }
            // A notice of disconnection (id 0), or another request's.
            if got == 0 {
                return Err("the directory ended the connection".into());
            }
        }
    }

    /// Sends `op`, and reads its result (of tag `want`).
    fn request(&mut self, op: &[u8], want: u8) -> Result<Outcome, String> {
        let id = self.send(op)?;
        let (tag, contents) = self.receive(id)?;
        if tag != want {
            return Err("an unexpected LDAP answer".into());
        }
        result(&contents)
    }

    /// A simple bind: whether `password` is `dn`'s.
    fn bind(&mut self, dn: &str, password: &str) -> Result<bool, String> {
        let op = ber::sequence(
            0x60,
            &[
                ber::integer(INTEGER, 3),
                ber::octets(OCTET_STRING, dn.as_bytes()),
                ber::octets(0x80, password.as_bytes()),
            ],
        );
        let r = self.request(&op, 0x61)?;
        match r.code {
            SUCCESS => Ok(true),
            INVALID_CREDENTIALS => Ok(false),
            code => Err(format!("the directory answered {code}: {}", r.message)),
        }
    }

    /// The DNs of the entries under `base` whose `attribute` is `value`.
    fn find(&mut self, base: &str, attribute: &str, value: &str) -> Result<Vec<String>, String> {
        let op = ber::sequence(
            0x63,
            &[
                ber::octets(OCTET_STRING, base.as_bytes()),
                ber::integer(ENUMERATED, 2), // the whole subtree
                ber::integer(ENUMERATED, 0), // never dereference aliases
                ber::integer(INTEGER, 2),    // two are enough to see too many
                ber::integer(INTEGER, 10),
                ber::tlv(BOOLEAN, &[0]),
                // (attribute=value): an equality match, the value never text
                // of the filter.
                ber::sequence(
                    0xa3,
                    &[
                        ber::octets(OCTET_STRING, attribute.as_bytes()),
                        ber::octets(OCTET_STRING, value.as_bytes()),
                    ],
                ),
                ber::sequence(SEQUENCE, &[ber::octets(OCTET_STRING, b"1.1")]),
            ],
        );
        let id = self.send(&op)?;
        let mut found = Vec::new();
        loop {
            let (tag, contents) = self.receive(id)?;
            match tag {
                0x64 => {
                    let dn = Elements(&contents)
                        .next()
                        .ok_or("an entry without a DN")??;
                    found.push(String::from_utf8_lossy(dn.contents).into_owned());
                }
                0x73 => {} // a referral elsewhere: not followed
                0x65 => {
                    let r = result(&contents)?;
                    // sizeLimitExceeded: more than one, which is refused below.
                    if r.code != SUCCESS && r.code != 4 {
                        return Err(format!("the search failed ({}): {}", r.code, r.message));
                    }
                    return Ok(found);
                }
                _ => return Err("an unexpected LDAP answer".into()),
            }
        }
    }

    fn unbind(mut self) {
        let _ = self.send(&[0x42, 0]);
    }
}

impl nucleus_server::Directory for Ldap {
    fn describe(&self) -> String {
        format!(
            "LDAP {}://{}:{}",
            if self.ldaps { "ldaps" } else { "ldap" },
            self.host,
            self.port
        )
    }

    fn check(&self, user: &str, password: &str) -> Result<bool, String> {
        if password.is_empty() || user.is_empty() {
            return Ok(false);
        }
        let mut conn = self.connect()?;
        let ok = match &self.mode {
            Mode::Bind { template } => {
                conn.bind(&template.replace("{user}", &escape_dn(user)), password)?
            }
            Mode::Search {
                base,
                attribute,
                account,
            } => {
                if let Some((dn, pw)) = account
                    && !conn.bind(dn, pw)?
                {
                    return Err("the directory refused the search account".into());
                }
                match conn.find(base, attribute, user)?.as_slice() {
                    [dn] => conn.bind(dn, password)?,
                    // None, or more than one: no login.
                    _ => false,
                }
            }
        };
        conn.unbind();
        Ok(ok)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dn_values_are_escaped() {
        assert_eq!(super::escape_dn("alice"), "alice");
        assert_eq!(super::escape_dn("a,ou=admins"), "a\\,ou\\=admins");
        assert_eq!(super::escape_dn(" #x "), "\\ #x\\ ");
        assert_eq!(super::escape_dn("#x"), "\\#x");
        assert_eq!(super::escape_dn("a\0b\\c"), "a\\00b\\\\c");
    }

    #[test]
    fn urls() {
        assert!(super::Ldap::bind("ldap://dir.example:3890", "uid={user}").is_ok());
        assert!(super::Ldap::bind("ldaps://dir.example", "uid=x").is_err());
        assert!(super::Ldap::bind("http://dir.example", "uid={user}").is_err());
        let l = super::Ldap::search("ldaps://dir.example", "dc=example", "uid").unwrap();
        assert_eq!((l.port, l.ldaps), (636, true));
    }
}
