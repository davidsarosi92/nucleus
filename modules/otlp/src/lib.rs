//! OpenTelemetry for nucleus (PLAN.md §M14.18): a *push* exporter — the
//! process's metrics ([`nucleus_platform::metrics`]) sent to a collector
//! every interval over OTLP/HTTP, JSON encoded (`POST <endpoint>/v1/metrics`).
//!
//! The OpenTelemetry metrics data model, cumulative temporality: a
//! counter is a monotonic `sum`, a gauge a `gauge`, a histogram a
//! `histogram` with explicit bounds (its buckets' counts each its own, not
//! cumulative as Prometheus has them).  The resource is `service.name =
//! nucleus` and its version.  Configured as the OpenTelemetry SDKs are:
//! `nucleus server --otlp <endpoint>` or `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_HEADERS` (`key=value,…`, an API key),
//! `OTEL_METRIC_EXPORT_INTERVAL` (milliseconds, 60000 by default).  Over
//! TLS to anything but a loopback address ([`nucleus_http`]).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nucleus_platform::metrics::{Exporter, Registry, Snapshot, Value};
use serde_json::{Value as Json, json};

/// The interval when none is given (the SDKs' default).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// The exporter.
#[derive(Clone, Debug)]
pub struct Otlp {
    /// Where the collector listens (`https://host:4318`); `/v1/metrics`
    /// is added.
    pub endpoint: String,
    /// Headers sent with each request (an API key).
    pub headers: Vec<(String, String)>,
    /// How often the metrics are sent.
    pub interval: Duration,
}

impl Otlp {
    /// One sending to `endpoint` every [`DEFAULT_INTERVAL`].
    pub fn new(endpoint: impl Into<String>) -> Self {
        Otlp {
            endpoint: endpoint.into(),
            headers: Vec::new(),
            interval: DEFAULT_INTERVAL,
        }
    }

    /// One configured by the environment as the OpenTelemetry SDKs are,
    /// if `OTEL_EXPORTER_OTLP_ENDPOINT` is set (`endpoint` overrides it).
    pub fn from_env(endpoint: Option<String>) -> Result<Option<Self>, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let Some(endpoint) = endpoint.or_else(|| var("OTEL_EXPORTER_OTLP_ENDPOINT")) else {
            return Ok(None);
        };
        let mut o = Otlp::new(endpoint);
        if let Some(h) = var("OTEL_EXPORTER_OTLP_HEADERS") {
            o.headers = parse_headers(&h)?;
        }
        if let Some(ms) = var("OTEL_METRIC_EXPORT_INTERVAL") {
            let ms: u64 = ms
                .parse()
                .map_err(|_| format!("OTEL_METRIC_EXPORT_INTERVAL: not milliseconds: {ms}"))?;
            o.interval = Duration::from_millis(ms.max(100));
        }
        Ok(Some(o))
    }

    /// Sends one snapshot now.
    pub fn push(&self, snap: &Snapshot, start: SystemTime) -> Result<(), String> {
        let body = encode(snap, start, SystemTime::now()).to_string();
        let url = format!("{}/v1/metrics", self.endpoint.trim_end_matches('/'));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        headers.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let r = nucleus_http::send(&nucleus_http::Request {
            method: "POST",
            url: &url,
            headers: &headers,
            body: Some(body.as_bytes()),
            timeout: Duration::from_secs(10),
            total: Some(Duration::from_secs(30)),
            limit: 1 << 20,
            agent: concat!("nucleus/", env!("CARGO_PKG_VERSION")),
        })
        .map_err(|e| e.to_string())?;
        if (200..300).contains(&r.status) {
            Ok(())
        } else {
            let text = String::from_utf8_lossy(&r.body);
            Err(format!(
                "{url}: {} {}",
                r.status,
                text.chars().take(200).collect::<String>()
            ))
        }
    }
}

impl Exporter for Otlp {
    fn name(&self) -> &str {
        "otlp"
    }

    fn start(self: Box<Self>, registry: &'static Registry) -> std::io::Result<()> {
        let start = SystemTime::now();
        std::thread::Builder::new()
            .name("nucleus otlp".into())
            .spawn(move || {
                // A failure logged once, and when it is over.
                let mut failing = false;
                loop {
                    std::thread::sleep(self.interval);
                    match self.push(&registry.snapshot(), start) {
                        Ok(()) if failing => {
                            failing = false;
                            nucleus_platform::log!("nucleus: otlp: sending metrics again");
                        }
                        Ok(()) => {}
                        Err(e) if !failing => {
                            failing = true;
                            nucleus_platform::log!("nucleus: otlp: {e}");
                        }
                        Err(_) => {}
                    }
                }
            })?;
        Ok(())
    }
}

/// `key=value,key2=value2`, as `OTEL_EXPORTER_OTLP_HEADERS` has them
/// (values percent-decoded).
pub fn parse_headers(s: &str) -> Result<Vec<(String, String)>, String> {
    s.split(',')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').ok_or_else(|| {
                "OTEL_EXPORTER_OTLP_HEADERS: key=value pairs, comma-separated".to_string()
            })?;
            let (k, v) = (k.trim(), percent_decode(v.trim()));
            if k.is_empty()
                || k.contains(|c: char| c <= ' ' || c == ':')
                || v.contains(['\r', '\n'])
            {
                return Err("OTEL_EXPORTER_OTLP_HEADERS: a bad header".into());
            }
            Ok((k.to_string(), v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn nanos(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

/// `snap` as an OTLP `ExportMetricsServiceRequest` in JSON: cumulative
/// from `start` (the process's start) to `now`.
pub fn encode(snap: &Snapshot, start: SystemTime, now: SystemTime) -> Json {
    let (start, now) = (nanos(start), nanos(now));
    let attrs = |labels: &[(String, String)]| -> Json {
        labels
            .iter()
            .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
            .collect()
    };
    let mut metrics = Vec::new();
    for (name, f) in &snap.families {
        let points = || -> Vec<Json> {
            f.samples
                .iter()
                .map(|s| {
                    let mut p = json!({
                        "attributes": attrs(&s.labels),
                        "startTimeUnixNano": start,
                        "timeUnixNano": now,
                    });
                    let m = p.as_object_mut().expect("an object");
                    match &s.value {
                        // int64 as a string, as OTLP's JSON has it.
                        Value::Counter(v) => {
                            m.insert("asInt".into(), json!(v.to_string()));
                        }
                        Value::Gauge(v) => {
                            m.insert("asDouble".into(), json!(v));
                        }
                        Value::Histogram {
                            buckets,
                            sum,
                            count,
                        } => {
                            let mut prev = 0;
                            let counts: Vec<String> = buckets
                                .iter()
                                .map(|(_, c)| {
                                    let own = c - prev;
                                    prev = *c;
                                    own.to_string()
                                })
                                .collect();
                            let bounds: Vec<f64> = buckets
                                .iter()
                                .map(|(b, _)| *b)
                                .filter(|b| b.is_finite())
                                .collect();
                            m.insert("count".into(), json!(count.to_string()));
                            m.insert("sum".into(), json!(sum));
                            m.insert("bucketCounts".into(), json!(counts));
                            m.insert("explicitBounds".into(), json!(bounds));
                        }
                    }
                    p
                })
                .collect()
        };
        let data = match f.kind {
            nucleus_platform::metrics::Kind::Counter => (
                "sum",
                json!({"aggregationTemporality": 2, "isMonotonic": true, "dataPoints": points()}),
            ),
            nucleus_platform::metrics::Kind::Gauge => ("gauge", json!({"dataPoints": points()})),
            nucleus_platform::metrics::Kind::Histogram => (
                "histogram",
                json!({"aggregationTemporality": 2, "dataPoints": points()}),
            ),
        };
        let unit = if name.ends_with("_seconds") {
            "s"
        } else if name.ends_with("_bytes") || name.ends_with("_bytes_total") {
            "By"
        } else {
            "1"
        };
        metrics.push(json!({
            "name": name,
            "description": f.help,
            "unit": unit,
            data.0: data.1,
        }));
    }
    json!({"resourceMetrics": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "nucleus"}},
            {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}},
        ]},
        "scopeMetrics": [{
            "scope": {"name": "nucleus", "version": env!("CARGO_PKG_VERSION")},
            "metrics": metrics,
        }],
    }]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        assert_eq!(
            parse_headers("api-key=a%20b, x=1").unwrap(),
            [
                ("api-key".to_string(), "a b".to_string()),
                ("x".into(), "1".into())
            ]
        );
        assert!(parse_headers("nope").is_err());
        assert!(parse_headers("a b=1").is_err());
    }
}
