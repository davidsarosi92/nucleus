//! Traces (PLAN.md §M14.18 O2b): spans the layers make — a statement's —
//! handed to a sink a module installs (the OTLP exporter), as the
//! metrics are handed to exporters.  Nothing is made while no sink is
//! installed: [`enabled`] is one relaxed load.
//!
//! A span belongs to a trace: the W3C Trace Context's `traceparent`
//! (`00-<trace id>-<parent span id>-<flags>`) a client sends puts its
//! statements' spans in the application's own trace; without one each
//! statement starts a trace, kept with the sink's sampling ratio.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// A finished span.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    /// The trace's id.
    pub trace_id: [u8; 16],
    /// This span's id.
    pub span_id: [u8; 8],
    /// The parent span's (in the client's trace), if any.
    pub parent: Option<[u8; 8]>,
    /// Its name (`SELECT`, `INSERT`, …).
    pub name: String,
    /// When it started and ended, in nanoseconds since the Unix epoch.
    pub start_nanos: u64,
    /// When it ended.
    pub end_nanos: u64,
    /// Its attributes: text, integers.
    pub attributes: Vec<(&'static str, Attr)>,
    /// `Some(message)` if it failed.
    pub error: Option<String>,
}

/// An attribute's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    /// A string.
    Text(String),
    /// An integer.
    Int(i64),
}

/// Where finished spans go.
pub trait SpanSink: Send + Sync {
    /// Takes a span (quickly: it is called on the statement's thread).
    fn record(&self, span: Span);

    /// Whether a trace a client did not start is kept: the sampling of
    /// traces begun here, by their id.
    fn sampled(&self, trace_id: &[u8; 16]) -> bool;
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static SINK: RwLock<Option<Arc<dyn SpanSink>>> = RwLock::new(None);

/// Installs the sink spans go to (`None`: none, spans not made).
pub fn set_sink(sink: Option<Arc<dyn SpanSink>>) {
    let on = sink.is_some();
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = sink;
    ENABLED.store(on, Ordering::Release);
}

/// Whether a sink is installed: callers make spans only then.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The installed sink.
pub fn sink() -> Option<Arc<dyn SpanSink>> {
    SINK.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A trace context: from a client's `traceparent`, or begun here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    /// The trace's id.
    pub trace_id: [u8; 16],
    /// The span the next spans are children of (`None`: they are roots).
    pub parent: Option<[u8; 8]>,
    /// Whether the trace is kept.
    pub sampled: bool,
}

/// A W3C `traceparent`: `00-<32 hex>-<16 hex>-<2 hex>`; `None` if it is
/// not one (an all-zero id is invalid).
pub fn parse_traceparent(s: &str) -> Option<Context> {
    let s = s.trim();
    let mut parts = s.split('-');
    let (version, trace, parent, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2
        || version == "ff"
        || trace.len() != 32
        || parent.len() != 16
        || flags.len() != 2
    {
        return None;
    }
    // Version 00 has exactly four fields; later ones may add.
    if version == "00" && parts.next().is_some() {
        return None;
    }
    let hex = |s: &str, out: &mut [u8]| -> Option<()> {
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(())
    };
    let (mut trace_id, mut span) = ([0u8; 16], [0u8; 8]);
    hex(trace, &mut trace_id)?;
    hex(parent, &mut span)?;
    let mut f = [0u8; 1];
    hex(flags, &mut f)?;
    hex(version, &mut [0u8; 1])?;
    if trace_id == [0; 16] || span == [0; 8] {
        return None;
    }
    Some(Context {
        trace_id,
        parent: Some(span),
        sampled: f[0] & 1 == 1,
    })
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random 64 bits (not cryptographic: ids): SplitMix64 over a counter,
/// seeded once from the operating system.
fn next() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    static N: AtomicU64 = AtomicU64::new(0);
    let seed = *SEED.get_or_init(|| {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        u64::from_le_bytes(b)
    });
    let mut z = seed.wrapping_add(
        N.fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15),
    );
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A new span id (never zero).
pub fn span_id() -> [u8; 8] {
    loop {
        let v = next();
        if v != 0 {
            return v.to_be_bytes();
        }
    }
}

/// A new trace, sampled as the sink says.
pub fn new_trace() -> Context {
    let mut trace_id = [0u8; 16];
    trace_id[..8].copy_from_slice(&span_id());
    trace_id[8..].copy_from_slice(&span_id());
    let sampled = sink().is_some_and(|s| s.sampled(&trace_id));
    Context {
        trace_id,
        parent: None,
        sampled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparents() {
        let c =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(hex(&c.trace_id), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(hex(&c.parent.unwrap()), "00f067aa0ba902b7");
        assert!(c.sampled);
        let c =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00").unwrap();
        assert!(!c.sampled);
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473x-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x",
            "00-4bf92f3577b34da6a3ce929d0e0e47é-00f067aa0ba902b7-01",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad}");
        }
        // A later version may add fields.
        assert!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-x")
                .is_some()
        );
    }

    #[test]
    fn ids_differ() {
        let (a, b) = (span_id(), span_id());
        assert_ne!(a, b);
        assert_ne!(new_trace().trace_id, new_trace().trace_id);
    }
}
