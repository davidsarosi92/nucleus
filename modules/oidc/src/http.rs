//! Just enough HTTP/1.1 to fetch an issuer's metadata and keys: `GET`, over
//! TLS (rustls, Mozilla's roots) — plain HTTP only to a loopback address.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// The most a response may hold.
const LIMIT: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(10);

/// GETs `url` and parses it as JSON.
pub(crate) fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let body = get(url)?;
    serde_json::from_slice(&body).map_err(|e| format!("{url}: not JSON: {e}"))
}

fn get(url: &str) -> Result<Vec<u8>, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{url}: {e}");
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(fail(&"not an http(s) URL"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
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
        .map_err(|e| fail(&e))?
        .collect();
    if !tls && !addrs.iter().all(|a| a.ip().is_loopback()) {
        return Err(fail(&"plain HTTP only to a loopback address; use https"));
    }
    let mut last = None;
    let mut tcp = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, TIMEOUT) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| fail(&last.map_or("no address".to_string(), |e| e.to_string())))?;
    tcp.set_read_timeout(Some(TIMEOUT)).map_err(|e| fail(&e))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\n\
         User-Agent: nucleus-oidc\r\nConnection: close\r\n\r\n"
    );
    let mut raw = Vec::new();
    if tls {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| fail(&e))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name =
            rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| fail(&e))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| fail(&e))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        s.write_all(request.as_bytes()).map_err(|e| fail(&e))?;
        read_all(&mut s, &mut raw).map_err(|e| fail(&e))?;
    } else {
        let mut s = tcp;
        s.write_all(request.as_bytes()).map_err(|e| fail(&e))?;
        read_all(&mut s, &mut raw).map_err(|e| fail(&e))?;
    }
    parse(&raw).map_err(|e| fail(&e))
}

fn read_all(r: &mut impl Read, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > LIMIT {
                    return Err(std::io::Error::other("the response is too large"));
                }
            }
            // A TLS peer that closes without close_notify: what was read is
            // checked by its length or chunks below.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// The body of a `200` response.
fn parse(raw: &[u8]) -> Result<Vec<u8>, String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no HTTP response")?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| "a bad HTTP response")?;
    let body = &raw[end + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if status.split(' ').nth(1) != Some("200") {
        return Err(format!("the server answered {status}"));
    }
    let header = |name: &str| {
        lines.clone().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    if header("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
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
                return Ok(out);
            }
            if rest.len() < size + 2 {
                return Err("a truncated chunk".into());
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    }
    match header("content-length").map(|l| l.parse::<usize>()) {
        Some(Ok(n)) if body.len() >= n => Ok(body[..n].to_vec()),
        Some(_) => Err("a truncated response".into()),
        None => Ok(body.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn responses() {
        let r = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra";
        assert_eq!(super::parse(r).unwrap(), b"{}");
        let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{\"\r\n3;x\r\na\":\r\n2\r\n1}\r\n0\r\n\r\n";
        assert_eq!(super::parse(r).unwrap(), b"{\"a\":1}");
        assert!(super::parse(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(super::parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
    }
}
