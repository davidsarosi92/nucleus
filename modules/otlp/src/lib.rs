//! OpenTelemetry for nucleus (PLAN.md §M14.18): a *push* exporter — the
//! process's metrics ([`nucleus_platform::metrics`]) and statements'
//! spans ([`nucleus_platform::trace`]) sent to a collector over
//! OTLP/HTTP, JSON encoded (`POST <endpoint>/v1/metrics`, `/v1/traces`).
//!
//! The OpenTelemetry metrics data model, cumulative temporality: a
//! counter is a monotonic `sum`, a gauge a `gauge`, a histogram a
//! `histogram` with explicit bounds (its buckets' counts each its own, not
//! cumulative as Prometheus has them).  A span per statement (server
//! kind), the database semantic conventions' attributes.  The resource is
//! `service.name = nucleus` and its version.  Configured as the
//! OpenTelemetry SDKs are: `nucleus server --otlp <endpoint>` or
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`
//! (`key=value,…`, an API key), `OTEL_METRIC_EXPORT_INTERVAL`
//! (milliseconds, 60000 by default), `OTEL_TRACES_EXPORTER=none` (no
//! spans), `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` (the share
//! of traces begun here that are kept; a client's own trace keeps its
//! flag).  Over TLS to anything but a loopback address ([`nucleus_http`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nucleus_platform::metrics::{Exporter, Registry, Snapshot, Value};
use nucleus_platform::trace::{self, Attr, Span, SpanSink};
use serde_json::{Value as Json, json};

/// The metrics' interval when none is given (the SDKs' default).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// How often spans are sent (the SDKs' batch span processor's default).
pub const SPAN_DELAY: Duration = Duration::from_secs(5);

/// Spans kept waiting at most; more are dropped (and counted).
pub const SPAN_QUEUE: usize = 2048;

/// The exporter.
#[derive(Clone, Debug)]
pub struct Otlp {
    /// Where the collector listens (`https://host:4318`); `/v1/metrics`
    /// and `/v1/traces` are added.
    pub endpoint: String,
    /// Headers sent with each request (an API key).
    pub headers: Vec<(String, String)>,
    /// How often the metrics are sent.
    pub interval: Duration,
    /// The share of traces begun here that are kept (0 to 1); `None`: no
    /// spans at all.
    pub traces: Option<f64>,
}

impl Otlp {
    /// One sending metrics to `endpoint` every [`DEFAULT_INTERVAL`] and
    /// every span.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Otlp {
            endpoint: endpoint.into(),
            headers: Vec::new(),
            interval: DEFAULT_INTERVAL,
            traces: Some(1.0),
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
        o.traces = sampling(
            var("OTEL_TRACES_EXPORTER").as_deref(),
            var("OTEL_TRACES_SAMPLER").as_deref(),
            var("OTEL_TRACES_SAMPLER_ARG").as_deref(),
        )?;
        Ok(Some(o))
    }

    /// POSTs `body` to `<endpoint><path>`.
    fn post(&self, path: &str, body: &[u8]) -> Result<(), String> {
        let url = format!("{}{path}", self.endpoint.trim_end_matches('/'));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        headers.extend(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let r = nucleus_http::send(&nucleus_http::Request {
            method: "POST",
            url: &url,
            headers: &headers,
            body: Some(body),
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

    /// Sends one snapshot now.
    pub fn push(&self, snap: &Snapshot, start: SystemTime) -> Result<(), String> {
        let body = encode(snap, start, SystemTime::now()).to_string();
        self.post("/v1/metrics", body.as_bytes())
    }

    /// Sends spans now.
    pub fn push_spans(&self, spans: &[Span]) -> Result<(), String> {
        let body = encode_spans(spans).to_string();
        self.post("/v1/traces", body.as_bytes())
    }
}

/// The share of traces kept, from the SDKs' variables: `None` with
/// `OTEL_TRACES_EXPORTER=none` or the sampler `always_off`.
pub fn sampling(
    exporter: Option<&str>,
    sampler: Option<&str>,
    arg: Option<&str>,
) -> Result<Option<f64>, String> {
    if let Some(e) = exporter
        && e != "otlp"
    {
        return if e == "none" {
            Ok(None)
        } else {
            Err(format!("OTEL_TRACES_EXPORTER: otlp or none, not {e}"))
        };
    }
    let ratio = || -> Result<f64, String> {
        arg.map_or(Ok(1.0), |a| {
            a.parse::<f64>()
                .ok()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or_else(|| format!("OTEL_TRACES_SAMPLER_ARG: a ratio from 0 to 1, not {a}"))
        })
    };
    match sampler.unwrap_or("parentbased_always_on") {
        "always_on" | "parentbased_always_on" => Ok(Some(1.0)),
        "always_off" | "parentbased_always_off" => Ok(None),
        "traceidratio" | "parentbased_traceidratio" => ratio().map(Some),
        other => Err(format!(
            "OTEL_TRACES_SAMPLER: {other} is not one nucleus knows"
        )),
    }
}

/// The spans waiting to be sent: the sink the statements record into.
struct Spans {
    queue: Mutex<Vec<Span>>,
    /// A trace begun here is kept if its id's last 8 bytes are below this.
    below: u64,
    dropped: AtomicU64,
}

impl SpanSink for Spans {
    fn record(&self, span: Span) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < SPAN_QUEUE {
            q.push(span);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn sampled(&self, trace_id: &[u8; 16]) -> bool {
        let tail = u64::from_be_bytes(trace_id[8..].try_into().expect("8 bytes"));
        self.below == u64::MAX || tail < self.below
    }
}

impl Exporter for Otlp {
    fn name(&self) -> &str {
        "otlp"
    }

    fn start(self: Box<Self>, registry: &'static Registry) -> std::io::Result<()> {
        let start = SystemTime::now();
        let spans = self.traces.map(|ratio| {
            let s = Arc::new(Spans {
                queue: Mutex::new(Vec::new()),
                below: if ratio >= 1.0 {
                    u64::MAX
                } else {
                    (ratio * u64::MAX as f64) as u64
                },
                dropped: AtomicU64::new(0),
            });
            trace::set_sink(Some(Arc::clone(&s) as Arc<dyn SpanSink>));
            s
        });
        std::thread::Builder::new()
            .name("nucleus otlp".into())
            .spawn(move || {
                // A failure logged once, and when it is over.
                let mut failing = false;
                let mut report = |r: Result<(), String>| match r {
                    Ok(()) if failing => {
                        failing = false;
                        nucleus_platform::log!("nucleus: otlp: sending again");
                    }
                    Ok(()) => {}
                    Err(e) if !failing => {
                        failing = true;
                        nucleus_platform::log!("nucleus: otlp: {e}");
                    }
                    Err(_) => {}
                };
                let tick = if spans.is_some() {
                    self.interval.min(SPAN_DELAY)
                } else {
                    self.interval
                };
                let mut last_metrics = Instant::now();
                loop {
                    std::thread::sleep(tick);
                    if let Some(s) = &spans {
                        let batch = std::mem::take(
                            &mut *s.queue.lock().unwrap_or_else(|e| e.into_inner()),
                        );
                        let dropped = s.dropped.swap(0, Ordering::Relaxed);
                        if dropped > 0 {
                            nucleus_platform::log!(
                                "nucleus: otlp: {dropped} spans dropped (more than {SPAN_QUEUE} waiting)"
                            );
                        }
                        for chunk in batch.chunks(512) {
                            report(self.push_spans(chunk));
                        }
                    }
                    if last_metrics.elapsed() >= self.interval {
                        last_metrics = Instant::now();
                        report(self.push(&registry.snapshot(), start));
                    }
                }
            })?;
        Ok(())
    }
}

/// `spans` as an OTLP `ExportTraceServiceRequest` in JSON (ids in hex).
pub fn encode_spans(spans: &[Span]) -> Json {
    let spans: Vec<Json> = spans
        .iter()
        .map(|s| {
            let attributes: Vec<Json> = s
                .attributes
                .iter()
                .map(|(k, v)| match v {
                    Attr::Text(t) => json!({"key": k, "value": {"stringValue": t}}),
                    Attr::Int(i) => json!({"key": k, "value": {"intValue": i.to_string()}}),
                })
                .collect();
            let mut j = json!({
                "traceId": trace::hex(&s.trace_id),
                "spanId": trace::hex(&s.span_id),
                "name": s.name,
                // SPAN_KIND_SERVER: the database serving a client.
                "kind": 2,
                "startTimeUnixNano": s.start_nanos.to_string(),
                "endTimeUnixNano": s.end_nanos.to_string(),
                "attributes": attributes,
                "status": match &s.error {
                    // STATUS_CODE_ERROR, or unset.
                    Some(m) => json!({"code": 2, "message": m}),
                    None => json!({}),
                },
            });
            if let Some(p) = s.parent {
                j["parentSpanId"] = json!(trace::hex(&p));
            }
            j
        })
        .collect();
    json!({"resourceSpans": [{
        "resource": resource(),
        "scopeSpans": [{
            "scope": {"name": "nucleus", "version": env!("CARGO_PKG_VERSION")},
            "spans": spans,
        }],
    }]})
}

fn resource() -> Json {
    json!({"attributes": [
        {"key": "service.name", "value": {"stringValue": "nucleus"}},
        {"key": "service.version", "value": {"stringValue": env!("CARGO_PKG_VERSION")}},
    ]})
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
        "resource": resource(),
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

    #[test]
    fn samplers() {
        assert_eq!(sampling(None, None, None), Ok(Some(1.0)));
        assert_eq!(sampling(Some("none"), None, None), Ok(None));
        assert_eq!(sampling(None, Some("always_off"), None), Ok(None));
        assert_eq!(
            sampling(None, Some("parentbased_traceidratio"), Some("0.25")),
            Ok(Some(0.25))
        );
        assert!(sampling(None, Some("traceidratio"), Some("2")).is_err());
        assert!(sampling(Some("zipkin"), None, None).is_err());
    }
}
