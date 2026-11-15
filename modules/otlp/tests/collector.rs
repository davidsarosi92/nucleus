//! A collector receives what the exporter pushes (PLAN.md §M14.18): a
//! listener here stands in for an OpenTelemetry collector's OTLP/HTTP
//! receiver and reads the requests as one would.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use nucleus_platform::metrics::{Counter, Exporter, Histogram, Registry};
use serde_json::Value;

static COMMITS: Counter = Counter::new("t_commits_total", "Commits.");
static TIME: Histogram =
    Histogram::labeled("t_duration_seconds", "Durations.", &[("kind", "read")]);
static REGISTRY: Registry = Registry::new();

/// One request: its line, headers and body.
fn receive(l: &TcpListener) -> (String, Vec<String>, Value) {
    let (s, _) = l.accept().unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let mut headers = Vec::new();
    let mut len = 0;
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        let h = h.trim_end().to_string();
        if h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap();
        }
        headers.push(h);
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).unwrap();
    r.get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
        .unwrap();
    (line, headers, serde_json::from_slice(&body).unwrap())
}

fn metric<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {body}"))
}

#[test]
fn a_collector_receives_the_numbers() {
    REGISTRY.register(&[&COMMITS, &TIME]);
    COMMITS.add(41);
    TIME.observe(Duration::from_micros(10));
    TIME.observe(Duration::from_secs(1));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut o = nucleus_otlp::Otlp::new(format!("http://{}", l.local_addr().unwrap()));
    o.interval = Duration::from_millis(50);
    o.headers = vec![("api-key".into(), "secret".into())];
    Box::new(o).start(&REGISTRY).unwrap();
    let (line, headers, body) = receive(&l);
    assert!(line.starts_with("POST /v1/metrics HTTP/1.1"), "{line}");
    assert!(
        headers
            .iter()
            .any(|h| h == "Content-Type: application/json"),
        "{headers:?}"
    );
    assert!(
        headers.iter().any(|h| h == "api-key: secret"),
        "{headers:?}"
    );
    let resource = &body["resourceMetrics"][0]["resource"]["attributes"][0];
    assert_eq!(resource["value"]["stringValue"], "nucleus");
    let c = metric(&body, "t_commits_total");
    assert_eq!(c["sum"]["isMonotonic"], true);
    assert_eq!(c["sum"]["aggregationTemporality"], 2);
    assert_eq!(c["sum"]["dataPoints"][0]["asInt"], "41");
    let h = &metric(&body, "t_duration_seconds")["histogram"]["dataPoints"][0];
    assert_eq!(h["count"], "2");
    assert_eq!(h["attributes"][0]["key"], "kind");
    let counts: Vec<u64> = h["bucketCounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().parse().unwrap())
        .collect();
    // Each bucket's own count; one more count than bounds.
    assert_eq!(counts.iter().sum::<u64>(), 2);
    assert_eq!(
        counts.len(),
        h["explicitBounds"].as_array().unwrap().len() + 1
    );
    assert_eq!(counts[2], 1);
    // The next push has the numbers as they moved.
    COMMITS.inc();
    let (_, _, body) = receive(&l);
    let n: u64 = metric(&body, "t_commits_total")["sum"]["dataPoints"][0]["asInt"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(n >= 42, "{n}");
}
