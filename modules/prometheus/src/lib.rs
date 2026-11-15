//! Prometheus for nucleus (PLAN.md §M14.18): a *pull* exporter — the
//! process's metrics ([`nucleus_platform::metrics`]) served as `GET
//! /metrics` in Prometheus's text exposition format, or OpenMetrics when
//! the scraper asks for it — and the checks an orchestrator probes:
//!
//! - `GET /health/live`: 200 while the process answers at all.
//! - `GET /health/ready`: 200 once the server is ready to serve (recovery
//!   done, the store open, the listeners up), 503 before and while it
//!   shuts down ([`nucleus_platform::metrics::ready`]).
//!
//! `nucleus server --metrics [addr]` starts it on a listener of its own
//! (127.0.0.1:20037 by default), before the database is opened, so a
//! probe sees a long recovery as alive and not ready.  Over TLS when the
//! server has a certificate.  One request a connection; HTTP/1.1 over the
//! standard library.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nucleus_platform::metrics::{Exporter, Kind, Registry, Snapshot, Value};
use nucleus_server::transport::{Stream, Tls};

/// Where it listens unless told otherwise.
pub const DEFAULT_ADDR: &str = "127.0.0.1:20037";

/// The text exposition format's content type.
pub const TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// OpenMetrics' content type.
pub const OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// A request's head, at most.
const MAX_HEAD: usize = 8 << 10;

/// How long a connection may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections served at once; more are closed at once.
const MAX_CONNECTIONS: usize = 16;

/// The exporter: a listener, and TLS if the server has a certificate.
pub struct Prometheus {
    listener: TcpListener,
    tls: Option<Arc<dyn Tls>>,
}

impl Prometheus {
    /// One serving on `listener`, over `tls` if given.
    pub fn new(listener: TcpListener, tls: Option<Arc<dyn Tls>>) -> Self {
        Prometheus { listener, tls }
    }
}

impl Exporter for Prometheus {
    fn name(&self) -> &str {
        "prometheus"
    }

    fn start(self: Box<Self>, registry: &'static Registry) -> io::Result<()> {
        let Prometheus { listener, tls } = *self;
        std::thread::Builder::new()
            .name("nucleus metrics".into())
            .spawn(move || serve(listener, tls, registry))?;
        Ok(())
    }
}

/// Serves `listener` until it fails, a thread per connection.
fn serve(listener: TcpListener, tls: Option<Arc<dyn Tls>>, registry: &'static Registry) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if open.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let (open, tls) = (Arc::clone(&open), tls.clone());
        let spawned = std::thread::Builder::new()
            .name("nucleus metrics connection".into())
            .spawn(move || {
                let _ = handle(stream, tls.as_deref(), registry);
                open.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            nucleus_platform::log!("nucleus: metrics: no thread for a connection");
        }
    }
}

fn handle(tcp: TcpStream, tls: Option<&dyn Tls>, registry: &Registry) -> io::Result<()> {
    tcp.set_read_timeout(Some(READ_TIMEOUT))?;
    tcp.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut stream: Box<dyn Stream> = match tls {
        Some(t) => t.accept(tcp)?.stream,
        None => Box::new(tcp),
    };
    let head = match read_head(&mut stream)? {
        Some(h) => h,
        None => return Ok(()),
    };
    let (status, kind, body) = answer(&head, registry);
    let head_only = head.starts_with("HEAD ");
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body.as_bytes());
    }
    stream.write_all(&out)?;
    stream.flush()
}

/// The request's head (to the blank line), or `None` if the connection
/// closed first.
fn read_head(stream: &mut dyn Stream) -> io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too long"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// The answer to a request's head: status line, content type and body.
pub fn answer(head: &str, registry: &Registry) -> (&'static str, &'static str, String) {
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    const PLAIN: &str = "text/plain; charset=utf-8";
    if method != "GET" && method != "HEAD" {
        return ("405 Method Not Allowed", PLAIN, "GET only\n".into());
    }
    match path {
        "/metrics" => {
            let openmetrics = lines.any(|l| {
                l.split_once(':').is_some_and(|(k, v)| {
                    k.eq_ignore_ascii_case("accept") && v.contains("application/openmetrics-text")
                })
            });
            let snap = registry.snapshot();
            if openmetrics {
                ("200 OK", OPENMETRICS, render(&snap, true))
            } else {
                ("200 OK", TEXT, render(&snap, false))
            }
        }
        "/health/live" => ("200 OK", PLAIN, "live\n".into()),
        "/health/ready" if nucleus_platform::metrics::ready() => {
            ("200 OK", PLAIN, "ready\n".into())
        }
        "/health/ready" => ("503 Service Unavailable", PLAIN, "not ready\n".into()),
        _ => (
            "404 Not Found",
            PLAIN,
            "not found: /metrics, /health/live, /health/ready\n".into(),
        ),
    }
}

/// `snap` in Prometheus's text exposition format (0.0.4), or in
/// OpenMetrics 1.0 (`openmetrics`: a counter's family named without its
/// `_total`, and `# EOF` at the end).
pub fn render(snap: &Snapshot, openmetrics: bool) -> String {
    let mut out = String::new();
    for (name, f) in &snap.families {
        let family = match (openmetrics, f.kind) {
            (true, Kind::Counter) => name.strip_suffix("_total").unwrap_or(name),
            _ => name.as_str(),
        };
        out.push_str(&format!(
            "# HELP {family} {}\n# TYPE {family} {}\n",
            escape_help(&f.help),
            f.kind.name()
        ));
        for s in &f.samples {
            match &s.value {
                Value::Counter(v) => line(&mut out, name, &s.labels, None, &v.to_string()),
                Value::Gauge(v) => line(&mut out, name, &s.labels, None, &number(*v)),
                Value::Histogram {
                    buckets,
                    sum,
                    count,
                } => {
                    let bucket = format!("{name}_bucket");
                    for (le, n) in buckets {
                        line(
                            &mut out,
                            &bucket,
                            &s.labels,
                            Some(&number(*le)),
                            &n.to_string(),
                        );
                    }
                    line(
                        &mut out,
                        &format!("{name}_sum"),
                        &s.labels,
                        None,
                        &number(*sum),
                    );
                    line(
                        &mut out,
                        &format!("{name}_count"),
                        &s.labels,
                        None,
                        &count.to_string(),
                    );
                }
            }
        }
    }
    if openmetrics {
        out.push_str("# EOF\n");
    }
    out
}

fn line(out: &mut String, name: &str, labels: &[(String, String)], le: Option<&str>, value: &str) {
    out.push_str(name);
    if !labels.is_empty() || le.is_some() {
        out.push('{');
        let all = labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(le.map(|le| ("le", le)));
        for (i, (k, v)) in all.enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape_label(v));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

/// A number as both formats write it: `+Inf`, `-Inf`, `NaN`, else the
/// shortest decimal that reads back the same.
fn number(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        }
    } else {
        v.to_string()
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleus_platform::metrics::{Counter, CounterFamily, Histogram};

    static C: Counter = Counter::new("x_commits_total", "Commits.");
    static F: CounterFamily =
        CounterFamily::new("x_logins_total", "Logins \"by\" front end.", "front_end");
    static H: Histogram = Histogram::labeled("x_seconds", "Durations.", &[("kind", "read")]);
    static R: Registry = Registry::new();

    #[test]
    fn both_formats() {
        R.register(&[&C, &F, &H]);
        C.add(2);
        F.inc("a\"b");
        H.observe(Duration::from_micros(10));
        let _g = R.collector(|s| s.gauge("x_pages", "Pages.\nTwo lines.", &[], 1.5));
        let snap = R.snapshot();
        let text = render(&snap, false);
        assert!(text.contains("# HELP x_commits_total Commits.\n# TYPE x_commits_total counter\nx_commits_total 2\n"), "{text}");
        assert!(
            text.contains("x_logins_total{front_end=\"a\\\"b\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("# HELP x_pages Pages.\\nTwo lines.\n"),
            "{text}"
        );
        assert!(text.contains("x_pages 1.5\n"), "{text}");
        assert!(
            text.contains("x_seconds_bucket{kind=\"read\",le=\"0.000004\"} 0\n"),
            "{text}"
        );
        assert!(
            text.contains("x_seconds_bucket{kind=\"read\",le=\"0.000016\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("x_seconds_bucket{kind=\"read\",le=\"+Inf\"} 1\n"),
            "{text}"
        );
        assert!(
            text.contains("x_seconds_sum{kind=\"read\"} 0.00001\n"),
            "{text}"
        );
        assert!(
            text.contains("x_seconds_count{kind=\"read\"} 1\n"),
            "{text}"
        );
        assert!(!text.contains("# EOF"));
        let om = render(&snap, true);
        assert!(
            om.contains("# TYPE x_commits counter\nx_commits_total 2\n"),
            "{om}"
        );
        assert!(om.ends_with("# EOF\n"));
    }

    #[test]
    fn routes() {
        let ok = |h: &str| answer(h, &R).0;
        assert_eq!(ok("GET /metrics HTTP/1.1\r\n\r\n"), "200 OK");
        assert_eq!(
            answer("GET /metrics HTTP/1.1\r\nAccept: application/openmetrics-text; version=1.0.0\r\n\r\n", &R).1,
            OPENMETRICS
        );
        assert_eq!(ok("GET /health/live HTTP/1.1\r\n\r\n"), "200 OK");
        assert_eq!(
            ok("POST /metrics HTTP/1.1\r\n\r\n"),
            "405 Method Not Allowed"
        );
        assert_eq!(ok("GET /x HTTP/1.1\r\n\r\n"), "404 Not Found");
    }
}
