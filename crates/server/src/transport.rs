//! What a front end talks to a client over: TCP, or TLS over it.
//!
//! TLS is a module's (`nucleus-tls`, PLAN.md §M14.25): the core names only
//! the [`Tls`] interface, so it carries no TLS library.  A front end
//! decides how a client asks for TLS — `ncp` by starting a TLS handshake
//! (§10), PostgreSQL's protocol by its `SSLRequest` — then hands the TCP
//! stream to [`Tls::accept`] and goes on over the stream it returns.
//!
//! A TLS stream cannot be cloned into a reading and a writing half as a
//! TCP stream can, so [`split`] shares one stream between the two halves
//! of a connection's thread.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::rc::Rc;

/// A byte stream to a client.
pub trait Stream: Read + Write + Send {}

impl<T: Read + Write + Send> Stream for T {}

/// What a TLS session agreed on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TlsInfo {
    /// The protocol version (`TLSv1.3`).
    pub version: String,
    /// The cipher suite (`TLS13_AES_128_GCM_SHA256`).
    pub cipher: String,
    /// The client's certificate chain (DER, its own first), if it sent
    /// one, the server asked for it and it verified.
    pub client_certificates: Vec<Vec<u8>>,
    /// That certificate's subject common name: who the client is.
    pub client_name: Option<String>,
    /// The `tls-server-end-point` channel binding (RFC 5929: the hash of
    /// the server's certificate), for SCRAM-SHA-256-PLUS; `None` when the
    /// certificate's signature algorithm defines none.
    pub channel_binding: Option<Vec<u8>>,
}

/// An encrypted stream, and what its handshake agreed on.
pub struct Secured {
    /// The stream.
    pub stream: Box<dyn Stream>,
    /// The session.
    pub info: TlsInfo,
}

/// The server side of TLS: a module provides it (`nucleus-tls`).
pub trait Tls: Send + Sync + std::fmt::Debug {
    /// Runs a handshake on `tcp` (nothing of it read yet) as the server.
    fn accept(&self, tcp: TcpStream) -> io::Result<Secured>;
}

/// The reading half of a shared stream (see [`split`]).
pub struct ReadHalf(Rc<RefCell<Box<dyn Stream>>>);

/// The writing half of a shared stream.
pub struct WriteHalf(Rc<RefCell<Box<dyn Stream>>>);

/// Two halves over one stream, for one thread: each call borrows the
/// stream only while it runs.
pub fn split(stream: Box<dyn Stream>) -> (ReadHalf, WriteHalf) {
    let s = Rc::new(RefCell::new(stream));
    (ReadHalf(Rc::clone(&s)), WriteHalf(s))
}

impl Read for ReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.borrow_mut().read(buf)
    }
}

impl Write for WriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// The first byte of a TLS handshake record: a client that starts with it
/// is starting TLS.
pub const TLS_HANDSHAKE: u8 = 0x16;

/// The client's address: the TCP peer's — or, when the peer is a trusted
/// proxy ([`crate::Config::trusted_proxies`]), the one its PROXY protocol
/// header (version 1 or 2, HAProxy's) names, read off the stream first.
/// A trusted proxy must send one; `LOCAL` / `UNKNOWN` (a health check) keep
/// the peer's.  Nothing is read from anyone else, whose header would be
/// taken as the protocol's first bytes and refused.
pub fn client_address(stream: &mut TcpStream, trusted: &[String]) -> io::Result<Option<IpAddr>> {
    let Some(peer) = stream.peer_addr().ok().map(|a| a.ip()) else {
        return Ok(None);
    };
    let host = nucleus_sql::rules::host(peer);
    let trusted_peer = trusted
        .iter()
        .any(|c| nucleus_sql::inet::parse_cidr(c).is_ok_and(|net| host.within(net)));
    if !trusted_peer {
        return Ok(Some(peer));
    }
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PROXY protocol: {what}"),
        )
    };
    let mut sig = [0u8; 12];
    stream.read_exact(&mut sig[..6])?;
    if &sig[..6] == b"PROXY " {
        // Version 1: one line of text, at most 107 bytes.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            if line.len() > 107 {
                return Err(bad("a line too long"));
            }
            stream.read_exact(&mut b)?;
            line.push(b[0]);
        }
        let text = std::str::from_utf8(&line).map_err(|_| bad("not text"))?;
        let mut parts = text.trim_end().split(' ');
        return match parts.next() {
            Some("TCP4" | "TCP6") => parts
                .next()
                .and_then(|a| a.parse().ok())
                .map(Some)
                .ok_or_else(|| bad("a bad source address")),
            Some("UNKNOWN") => Ok(Some(peer)),
            _ => Err(bad("an unknown protocol")),
        };
    }
    stream.read_exact(&mut sig[6..])?;
    if sig != *b"\r\n\r\n\0\r\nQUIT\n" {
        return Err(bad("a trusted proxy sent no header"));
    }
    // Version 2: version/command, family/protocol, length, addresses.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
    let mut rest = vec![0u8; len];
    stream.read_exact(&mut rest)?;
    if head[0] >> 4 != 2 {
        return Err(bad("an unknown version"));
    }
    if head[0] & 0x0f == 0 {
        return Ok(Some(peer)); // LOCAL
    }
    match head[1] >> 4 {
        1 if rest.len() >= 12 => Ok(Some(IpAddr::from([rest[0], rest[1], rest[2], rest[3]]))),
        2 if rest.len() >= 36 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&rest[..16]);
            Ok(Some(IpAddr::from(a)))
        }
        0 => Ok(Some(peer)), // UNSPEC
        _ => Err(bad("a bad address block")),
    }
}
