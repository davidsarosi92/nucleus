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
use std::net::TcpStream;
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
