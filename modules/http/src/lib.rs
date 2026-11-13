//! Just enough HTTP/1.1 for nucleus's modules that call out (PLAN.md
//! §M14.22): a request over TLS (rustls, Mozilla's roots) — plain HTTP
//! only to a loopback address — and its response.  One request a
//! connection.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// A request.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    /// `GET`, `POST`, …
    pub method: &'a str,
    /// `https://host[:port]/path` (or `http://` to a loopback address).
    pub url: &'a str,
    /// Headers besides `Host`, `Content-Length`, `Connection` and
    /// `User-Agent`, which are set.
    pub headers: &'a [(&'a str, &'a str)],
    /// The body.
    pub body: Option<&'a [u8]>,
    /// How long connecting, and each read, may take.
    pub timeout: Duration,
    /// How long the whole exchange may take, from connecting to the
    /// response's last byte (`None`: each read's `timeout` alone) — so a
    /// server that sends a byte now and then cannot hold a call.
    pub total: Option<Duration>,
    /// The most bytes a response may have.
    pub limit: usize,
    /// The `User-Agent`.
    pub agent: &'a str,
}

impl Default for Request<'_> {
    fn default() -> Self {
        Request {
            method: "GET",
            url: "",
            headers: &[],
            body: None,
            timeout: Duration::from_secs(30),
            total: Some(Duration::from_secs(300)),
            limit: 16 << 20,
            agent: "nucleus",
        }
    }
}

/// A response: any status (the caller decides what it means).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The headers, names as sent.
    pub headers: Vec<(String, String)>,
    /// The body (de-chunked).
    pub body: Vec<u8>,
}

impl Response {
    /// The first header called `name` (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `Retry-After`, when it gives seconds.
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")?
            .trim()
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
    }

    /// The body as text (lossy), at most `max` characters — for messages.
    pub fn body_text(&self, max: usize) -> String {
        let t = String::from_utf8_lossy(&self.body);
        let t = t.trim();
        match t.char_indices().nth(max) {
            Some((i, _)) => format!("{}…", &t[..i]),
            None => t.to_string(),
        }
    }
}

/// Why a request failed (no response).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What happened (never a header's value).
    pub message: String,
    /// Whether the same request may succeed later (no connection, a
    /// time-out, a cut response) — not a bad URL or a refused address.
    pub transient: bool,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static C: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
        )
    })
    .clone()
}

/// Sends `r` and reads its response.
pub fn send(r: &Request<'_>) -> Result<Response, Error> {
    let deadline = r.total.map(|t| Instant::now() + t);
    let url = r.url;
    // Spaces and control characters would go into the request line as
    // they are: another header, or another request.
    if url.chars().any(|c| c <= ' ' || c == '\x7f') {
        return Err(Error {
            message: format!("{}: a URL with spaces or control characters", r.method),
            transient: false,
        });
    }
    let fail = |e: &dyn std::fmt::Display, transient: bool| Error {
        message: format!("{} {url}: {e}", r.method),
        transient,
    };
    let (tls, rest) = if let Some(x) = url.strip_prefix("https://") {
        (true, x)
    } else if let Some(x) = url.strip_prefix("http://") {
        (false, x)
    } else {
        return Err(fail(&"not an http(s) URL", false));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err(fail(&"credentials in a URL are not taken", false));
    }
    if let Some((k, _)) = r
        .headers
        .iter()
        .find(|(k, v)| k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']))
    {
        return Err(fail(&format!("a bad header {k}"), false));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse::<u16>() {
            Ok(p) => (h, p),
            Err(_) => (authority, if tls { 443 } else { 80 }),
        },
        _ => (authority, if tls { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| fail(&e, true))?
        .collect();
    if !tls && !addrs.iter().all(|a| a.ip().is_loopback()) {
        return Err(fail(
            &"plain HTTP only to a loopback address; use https",
            false,
        ));
    }
    let mut last = None;
    let mut tcp = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, within(r.timeout, deadline)) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        fail(
            &last.map_or("no address".to_string(), |e| e.to_string()),
            true,
        )
    })?;
    tcp.set_read_timeout(Some(within(r.timeout, deadline)))
        .and_then(|()| tcp.set_write_timeout(Some(within(r.timeout, deadline))))
        .map_err(|e| fail(&e, true))?;
    // The same socket, to shorten each read's wait as the deadline nears.
    let ctl = tcp.try_clone().map_err(|e| fail(&e, true))?;
    let read = |s: &mut dyn Read, raw: &mut Vec<u8>| {
        read_all(s, raw, r.limit, deadline, || {
            ctl.set_read_timeout(Some(within(r.timeout, deadline)))
        })
    };
    let _ = tcp.set_nodelay(true);
    let mut head = format!(
        "{} {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {}\r\nConnection: close\r\n",
        r.method, r.agent
    );
    for (k, v) in r.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = r.body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    if let Some(b) = r.body {
        request.extend_from_slice(b);
    }
    let mut raw = Vec::new();
    if tls {
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| fail(&e, false))?;
        let conn =
            rustls::ClientConnection::new(tls_config(), name).map_err(|e| fail(&e, false))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        s.write_all(&request).map_err(|e| fail(&e, true))?;
        read(&mut s, &mut raw).map_err(|e| fail(&e, true))?;
    } else {
        let mut s = tcp;
        s.write_all(&request).map_err(|e| fail(&e, true))?;
        read(&mut s, &mut raw).map_err(|e| fail(&e, true))?;
    }
    parse(&raw).map_err(|e| fail(&e, true))
}

/// `timeout`, or less when `deadline` is nearer (never zero: a zero
/// timeout is no timeout to a socket).
fn within(timeout: Duration, deadline: Option<Instant>) -> Duration {
    let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
    timeout.min(left).max(Duration::from_millis(1))
}

fn read_all(
    r: &mut dyn Read,
    out: &mut Vec<u8>,
    limit: usize,
    deadline: Option<Instant>,
    shorten: impl Fn() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 16384];
    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the response took longer than the call may",
                ));
            }
            shorten()?;
        }
        match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > limit {
                    return Err(std::io::Error::other("the response is too large"));
                }
            }
            // A TLS peer that closes without close_notify: what was read is
            // checked by its length or chunks.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            // A read's timeout (`EAGAIN` on Unix), at the deadline or not.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                let why = if deadline.is_some_and(|d| Instant::now() >= d) {
                    "the response took longer than the call may"
                } else {
                    "the server stopped answering"
                };
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, why));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A response from its bytes.
fn parse(raw: &[u8]) -> Result<Response, String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no HTTP response")?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| "a bad HTTP response")?;
    let body = &raw[end + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("a bad status line: {status_line}"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let body = if header("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut out = Vec::new();
        let mut rest = body;
        loop {
            let i = rest
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or("a bad chunk")?;
            let size = std::str::from_utf8(&rest[..i]).map_err(|_| "a bad chunk")?;
            let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| "a bad chunk")?;
            rest = &rest[i + 2..];
            if size == 0 {
                break out;
            }
            // Compared without adding to `size`, which the server chose.
            if rest.len() < 2 || rest.len() - 2 < size {
                return Err("a truncated chunk".into());
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    } else {
        match header("content-length").map(|l| l.parse::<usize>()) {
            Some(Ok(n)) if body.len() >= n => body[..n].to_vec(),
            Some(_) => return Err("a truncated response".into()),
            None => body.to_vec(),
        }
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"{}"[..]));
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{\"\r\n3;x\r\na\":\r\n2\r\n1}\r\n0\r\n\r\n").unwrap();
        // A chunk size near usize::MAX: refused, not wrapped.
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nab\r\n0\r\n\r\n")
                .is_err()
        );
        assert_eq!(r.body, b"{\"a\":1}");
        let r =
            parse(b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 7\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        assert_eq!(r.status, 429);
        assert_eq!(r.retry_after(), Some(Duration::from_secs(7)));
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
        assert!(parse(b"garbage").is_err());
    }

    #[test]
    fn refusals() {
        let e = send(&Request {
            url: "ftp://x/",
            ..Default::default()
        })
        .unwrap_err();
        assert!(!e.transient);
        let e = send(&Request {
            url: "https://user:pw@example.com/",
            ..Default::default()
        })
        .unwrap_err();
        assert!(!e.transient && e.message.contains("credentials"));
        let e = send(&Request {
            url: "http://127.0.0.1:1/",
            headers: &[("X", "a\r\nInjected: 1")],
            ..Default::default()
        })
        .unwrap_err();
        assert!(!e.transient); // Nor in the URL, which goes into the request line.
        for url in ["http://127.0.0.1:1/a b", "http://127.0.0.1:1/a\r\nX: 1"] {
            let e = send(&Request {
                url,
                ..Default::default()
            })
            .unwrap_err();
            assert!(!e.transient && e.message.contains("control"), "{url}");
        }
    }

    #[test]
    fn a_loopback_round_trip() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 1024];
            while !got.ends_with(b"ping") {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"HTTP/1.1 201 Created\r\nX-Echo: yes\r\nContent-Length: 4\r\n\r\npong")
                .unwrap();
            String::from_utf8(got).unwrap()
        });
        let url = format!("http://{addr}/v1/x");
        let r = send(&Request {
            method: "POST",
            url: &url,
            headers: &[("Content-Type", "text/plain")],
            body: Some(b"ping"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            (r.status, r.header("x-echo"), r.body.as_slice()),
            (201, Some("yes"), &b"pong"[..])
        );
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/x HTTP/1.1\r\n"), "{sent}");
        assert!(sent.contains("Content-Length: 4\r\n"));
    }

    /// A server that sends a byte now and then, each well within a read's
    /// timeout, is cut off at the whole call's.
    #[test]
    fn a_dripping_server_is_cut_off() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            for b in b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"
                .iter()
                .cycle()
                .take(400)
            {
                if s.write_all(&[*b]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let url = format!("http://{addr}/");
        let t = Instant::now();
        let e = send(&Request {
            url: &url,
            timeout: Duration::from_secs(5),
            total: Some(Duration::from_millis(600)),
            ..Default::default()
        })
        .unwrap_err();
        let took = t.elapsed();
        assert!(e.transient && e.message.contains("longer"), "{}", e.message);
        assert!(took < Duration::from_secs(2), "{took:?}");
        drop(server);
    }
}
