//! Metrics (PLAN.md §M14.18): one registry of named instruments that every
//! layer records into, and an interface that hands their values to
//! exporters — Prometheus, OpenTelemetry, `nc_catalog.metrics` — which
//! are modules (principle 12).
//!
//! - **Instruments** are statics: a [`Counter`], a [`Gauge`] or a
//!   [`Histogram`] (fixed logarithmic buckets), kept in atomics, so that
//!   recording is one relaxed atomic add — no lock, no allocation.  A
//!   [`CounterFamily`] has a label whose values are known only at run time
//!   (a front end's name); each value's counter is found once and kept.
//! - **Collectors** are read when a snapshot is taken: what a layer
//!   already counts (the page cache's hits, the pages in use, the open
//!   sessions) is not counted twice, only read.
//! - The **registry** ([`global`]) holds both; [`Registry::snapshot`]
//!   reads them all.  It is the process's: two databases open in one
//!   process add up (a server has one).
//! - An [`Exporter`] is given the registry and sends or serves snapshots.
//!
//! Prior art: Prometheus's client libraries (the instruments and the
//! histogram's cumulative buckets), ClickHouse's `ProfileEvents` (static
//! counters in every layer), the OpenTelemetry metrics data model
//! (cumulative temporality).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What kind of instrument a family is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up (from the process's start).
    Counter,
    /// Goes up and down.
    Gauge,
    /// A distribution: counts in buckets, a sum and a count.
    Histogram,
}

impl Kind {
    /// `counter`, `gauge` or `histogram`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// A sample's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A counter's.
    Counter(u64),
    /// A gauge's.
    Gauge(f64),
    /// A histogram's: each bucket's upper bound and the count of values at
    /// or below it (cumulative, the last bound infinite), the values' sum
    /// and their count.
    Histogram {
        /// `(upper bound, cumulative count)`.
        buckets: Vec<(f64, u64)>,
        /// The values' sum.
        sum: f64,
        /// How many values.
        count: u64,
    },
}

impl Value {
    fn add(&mut self, other: &Value) {
        match (self, other) {
            (Value::Counter(a), Value::Counter(b)) => *a += b,
            (Value::Gauge(a), Value::Gauge(b)) => *a += b,
            (
                Value::Histogram {
                    buckets,
                    sum,
                    count,
                },
                Value::Histogram {
                    buckets: b,
                    sum: s,
                    count: c,
                },
            ) if buckets.len() == b.len() => {
                for (x, y) in buckets.iter_mut().zip(b) {
                    x.1 += y.1;
                }
                *sum += s;
                *count += c;
            }
            _ => {}
        }
    }
}

/// One series: its labels and value.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// `(name, value)`, in the order the instrument gives them.
    pub labels: Vec<(String, String)>,
    /// The value.
    pub value: Value,
}

/// The series of one name.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    /// What it measures, in a sentence.
    pub help: String,
    /// Its kind.
    pub kind: Kind,
    /// Its series.
    pub samples: Vec<Sample>,
}

/// Every instrument's values at one moment, by name — each instrument's
/// read at once (a histogram's buckets, sum and count may be a value
/// apart).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The families, by name.
    pub families: BTreeMap<String, Family>,
}

impl Snapshot {
    /// Adds a sample of `name`; one with the same labels adds up (two
    /// databases in one process).
    pub fn add(&mut self, name: &str, help: &str, labels: &[(&str, &str)], value: Value) {
        let kind = match value {
            Value::Counter(_) => Kind::Counter,
            Value::Gauge(_) => Kind::Gauge,
            Value::Histogram { .. } => Kind::Histogram,
        };
        let family = self
            .families
            .entry(name.to_string())
            .or_insert_with(|| Family {
                help: help.to_string(),
                kind,
                samples: Vec::new(),
            });
        if family.kind != kind {
            return;
        }
        if let Some(s) = family.samples.iter_mut().find(|s| {
            s.labels.len() == labels.len()
                && s.labels
                    .iter()
                    .zip(labels)
                    .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
        }) {
            s.value.add(&value);
            return;
        }
        family.samples.push(Sample {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value,
        });
    }

    /// Declares family `name` without a sample (yet): exporters show it
    /// with none, so a series that is sometimes absent (no session open)
    /// has its family there all the same.
    pub fn declare(&mut self, name: &str, help: &str, kind: Kind) {
        self.families
            .entry(name.to_string())
            .or_insert_with(|| Family {
                help: help.to_string(),
                kind,
                samples: Vec::new(),
            });
    }

    /// A counter's sample.
    pub fn counter(&mut self, name: &str, help: &str, labels: &[(&str, &str)], v: u64) {
        self.add(name, help, labels, Value::Counter(v));
    }

    /// A gauge's sample.
    pub fn gauge(&mut self, name: &str, help: &str, labels: &[(&str, &str)], v: f64) {
        self.add(name, help, labels, Value::Gauge(v));
    }

    /// The value of `name`'s series with exactly `labels`, as a number (a
    /// histogram's count) — for tests and `nucleus status`.
    pub fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let s = self.families.get(name)?.samples.iter().find(|s| {
            s.labels.len() == labels.len()
                && s.labels
                    .iter()
                    .zip(labels)
                    .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
        })?;
        Some(match &s.value {
            Value::Counter(v) => *v as f64,
            Value::Gauge(v) => *v,
            Value::Histogram { count, .. } => *count as f64,
        })
    }
}

/// Something the registry reads at snapshot time.
pub trait Instrument: Sync {
    /// Adds its samples.
    fn collect(&self, out: &mut Snapshot);
}

/// A count that only goes up.
#[derive(Debug)]
pub struct Counter {
    name: &'static str,
    help: &'static str,
    labels: &'static [(&'static str, &'static str)],
    v: AtomicU64,
}

impl Counter {
    /// A counter called `name` (`nucleus_…_total`).
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self::labeled(name, help, &[])
    }

    /// A counter with fixed labels — one series of a family whose label
    /// values are known when it is written (`kind="read"`).
    pub const fn labeled(
        name: &'static str,
        help: &'static str,
        labels: &'static [(&'static str, &'static str)],
    ) -> Self {
        Counter {
            name,
            help,
            labels,
            v: AtomicU64::new(0),
        }
    }

    /// Adds one.
    #[inline]
    pub fn inc(&self) {
        self.v.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds `n`.
    #[inline]
    pub fn add(&self, n: u64) {
        self.v.fetch_add(n, Ordering::Relaxed);
    }

    /// Its value.
    pub fn get(&self) -> u64 {
        self.v.load(Ordering::Relaxed)
    }
}

impl Instrument for Counter {
    fn collect(&self, out: &mut Snapshot) {
        out.counter(self.name, self.help, self.labels, self.get());
    }
}

/// A value that goes up and down.
#[derive(Debug)]
pub struct Gauge {
    name: &'static str,
    help: &'static str,
    v: AtomicI64,
}

impl Gauge {
    /// A gauge called `name`.
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Gauge {
            name,
            help,
            v: AtomicI64::new(0),
        }
    }

    /// Sets it.
    pub fn set(&self, v: i64) {
        self.v.store(v, Ordering::Relaxed);
    }

    /// Adds `n` (negative to take away).
    #[inline]
    pub fn add(&self, n: i64) {
        self.v.fetch_add(n, Ordering::Relaxed);
    }

    /// Its value.
    pub fn get(&self) -> i64 {
        self.v.load(Ordering::Relaxed)
    }
}

impl Instrument for Gauge {
    fn collect(&self, out: &mut Snapshot) {
        out.gauge(self.name, self.help, &[], self.get() as f64);
    }
}

/// A histogram's buckets: their upper bounds are powers of 4 microseconds,
/// 1 µs to about 16.8 s, then one without a bound.
pub const BUCKETS: usize = 14;

/// The upper bound of bucket `i`, in seconds (infinite for the last).
pub fn bucket_bound(i: usize) -> f64 {
    if i + 1 >= BUCKETS {
        f64::INFINITY
    } else {
        (1u64 << (2 * i)) as f64 * 1e-6
    }
}

/// The bucket of a duration of `nanos`: the first whose bound is at or
/// above it.
#[inline]
pub fn bucket_of(nanos: u64) -> usize {
    let micros = nanos.div_ceil(1000);
    if micros <= 1 {
        return 0;
    }
    // ceil(log4(micros)) = ceil(bits(micros - 1) / 2).
    let bits = 64 - (micros - 1).leading_zeros() as usize;
    bits.div_ceil(2).min(BUCKETS - 1)
}

/// How long things take: a count per bucket (each atomic), a sum and a
/// count.
#[derive(Debug)]
pub struct Histogram {
    name: &'static str,
    help: &'static str,
    labels: &'static [(&'static str, &'static str)],
    buckets: [AtomicU64; BUCKETS],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    /// A histogram of durations called `name` (`nucleus_…_seconds`).
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self::labeled(name, help, &[])
    }

    /// One with fixed labels (as [`Counter::labeled`]).
    pub const fn labeled(
        name: &'static str,
        help: &'static str,
        labels: &'static [(&'static str, &'static str)],
    ) -> Self {
        Histogram {
            name,
            help,
            labels,
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Records a duration.
    #[inline]
    pub fn observe(&self, d: Duration) {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.buckets[bucket_of(nanos)].fetch_add(1, Ordering::Relaxed);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// How many durations were recorded.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Instrument for Histogram {
    fn collect(&self, out: &mut Snapshot) {
        let mut total = 0;
        let buckets = self
            .buckets
            .iter()
            .enumerate()
            .map(|(i, b)| {
                total += b.load(Ordering::Relaxed);
                (bucket_bound(i), total)
            })
            .collect();
        let value = Value::Histogram {
            buckets,
            sum: self.sum_nanos.load(Ordering::Relaxed) as f64 * 1e-9,
            // The buckets' total: what they were read as (a value recorded
            // meanwhile is in the next snapshot, whole).
            count: total,
        };
        out.add(self.name, self.help, self.labels, value);
    }
}

/// Counters with one label whose values come at run time (a front end's
/// name): each value's counter is made once and kept for the process.
#[derive(Debug)]
pub struct CounterFamily {
    name: &'static str,
    help: &'static str,
    label: &'static str,
    children: Mutex<Vec<(String, &'static AtomicU64)>>,
}

impl CounterFamily {
    /// A family called `name`, its series told apart by `label`.
    pub const fn new(name: &'static str, help: &'static str, label: &'static str) -> Self {
        CounterFamily {
            name,
            help,
            label,
            children: Mutex::new(Vec::new()),
        }
    }

    /// The counter of label value `value` (made at its first use; keep
    /// it rather than look it up on a hot path).
    pub fn with(&self, value: &str) -> &'static AtomicU64 {
        let mut c = lock(&self.children);
        if let Some((_, a)) = c.iter().find(|(v, _)| v == value) {
            return a;
        }
        let a: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        c.push((value.to_string(), a));
        a
    }

    /// Adds one to `value`'s counter.
    pub fn inc(&self, value: &str) {
        self.with(value).fetch_add(1, Ordering::Relaxed);
    }
}

impl Instrument for CounterFamily {
    fn collect(&self, out: &mut Snapshot) {
        let c = lock(&self.children);
        // The family is there, without series, until one is used.
        out.declare(self.name, self.help, Kind::Counter);
        for (v, a) in c.iter() {
            out.counter(
                self.name,
                self.help,
                &[(self.label, v)],
                a.load(Ordering::Relaxed),
            );
        }
    }
}

type CollectFn = Box<dyn Fn(&mut Snapshot) + Send + Sync>;

/// The instruments and collectors of a process ([`global`]).
pub struct Registry {
    instruments: Mutex<Vec<&'static dyn Instrument>>,
    collectors: Mutex<Vec<(u64, Arc<CollectFn>)>>,
    next: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty one (tests; the process has [`global`]).
    pub const fn new() -> Self {
        Registry {
            instruments: Mutex::new(Vec::new()),
            collectors: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Adds instruments; one already there is not added again.
    pub fn register(&self, instruments: &[&'static dyn Instrument]) {
        let mut all = lock(&self.instruments);
        for i in instruments {
            let addr = *i as *const dyn Instrument as *const ();
            if !all
                .iter()
                .any(|j| std::ptr::eq(*j as *const dyn Instrument as *const (), addr))
            {
                all.push(*i);
            }
        }
    }

    /// Adds a collector, read at each snapshot until the handle it returns
    /// is dropped.
    pub fn collector(
        &'static self,
        f: impl Fn(&mut Snapshot) + Send + Sync + 'static,
    ) -> Collector {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.collectors).push((id, Arc::new(Box::new(f))));
        Collector { registry: self, id }
    }

    /// Every instrument's and collector's values now.
    pub fn snapshot(&self) -> Snapshot {
        let mut s = Snapshot::default();
        let instruments = lock(&self.instruments).clone();
        for i in instruments {
            i.collect(&mut s);
        }
        // Run outside the lock: a collector may take locks of its own.
        let collectors: Vec<Arc<CollectFn>> = lock(&self.collectors)
            .iter()
            .map(|(_, f)| Arc::clone(f))
            .collect();
        for f in collectors {
            f(&mut s);
        }
        s
    }
}

/// A collector's registration: dropping it removes the collector.
#[must_use = "the collector is removed when this is dropped"]
pub struct Collector {
    registry: &'static Registry,
    id: u64,
}

impl std::fmt::Debug for Collector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collector").field("id", &self.id).finish()
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        lock(&self.registry.collectors).retain(|(id, _)| *id != self.id);
    }
}

static GLOBAL: Registry = Registry::new();

/// The process's registry.
pub fn global() -> &'static Registry {
    &GLOBAL
}

/// Something that takes the registry's snapshots out of the process: a
/// *pull* exporter serves one when asked (Prometheus scraping
/// `/metrics`), a *push* exporter sends them on a schedule of its own
/// (OTLP to a collector).  Exporters are modules; the server starts the
/// ones it is told to.
pub trait Exporter: Send + Sync {
    /// Its name (`prometheus`, `otlp`).
    fn name(&self) -> &str;

    /// Starts it on threads of its own, reading `registry`.
    fn start(self: Box<Self>, registry: &'static Registry) -> std::io::Result<()>;
}

static READY: AtomicBool = AtomicBool::new(false);

/// Marks the process ready to serve (recovery done, the store open, the
/// listeners up) — or not (shutting down): what a readiness check answers.
pub fn set_ready(ready: bool) {
    READY.store(ready, Ordering::Release);
}

/// Whether the process is ready to serve ([`set_ready`]).
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_powers_of_four_microseconds() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1000), 0);
        assert_eq!(bucket_of(1001), 1);
        assert_eq!(bucket_of(4000), 1);
        assert_eq!(bucket_of(4001), 2);
        assert_eq!(bucket_of(16_000), 2);
        assert_eq!(bucket_of(1_048_576_000), 10);
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        for i in 0..BUCKETS - 1 {
            let bound = (bucket_bound(i) * 1e9).round() as u64;
            assert_eq!(bucket_of(bound), i, "{i}");
            assert_eq!(bucket_of(bound + 1000), i + 1, "{i}");
        }
        assert!(bucket_bound(BUCKETS - 1).is_infinite());
    }

    static C: Counter = Counter::new("t_total", "a counter");
    static L: Counter = Counter::labeled("t_kind_total", "kinds", &[("kind", "read")]);
    static H: Histogram = Histogram::new("t_seconds", "durations");
    static F: CounterFamily = CounterFamily::new("t_by_total", "by name", "name");
    static R: Registry = Registry::new();

    #[test]
    fn a_snapshot_reads_instruments_and_collectors() {
        R.register(&[&C, &L, &H, &F, &C]);
        C.add(3);
        L.inc();
        H.observe(Duration::from_micros(3));
        H.observe(Duration::from_millis(2));
        F.inc("ncp");
        F.inc("ncp");
        F.inc("pg");
        let s = R.snapshot();
        assert_eq!(s.value("t_total", &[]), Some(3.0));
        assert_eq!(s.value("t_kind_total", &[("kind", "read")]), Some(1.0));
        assert_eq!(s.value("t_by_total", &[("name", "ncp")]), Some(2.0));
        assert_eq!(s.value("t_by_total", &[("name", "pg")]), Some(1.0));
        let Value::Histogram {
            buckets,
            sum,
            count,
        } = &s.families["t_seconds"].samples[0].value
        else {
            panic!()
        };
        assert_eq!(*count, 2);
        assert!((sum - 0.002003).abs() < 1e-9);
        assert_eq!(buckets[0].1, 0);
        assert_eq!(buckets[1].1, 1);
        assert_eq!(buckets[BUCKETS - 1].1, 2);
        // Two collectors of the same series add up; a dropped one is gone.
        let a = R.collector(|s| s.gauge("t_pages", "pages", &[], 2.0));
        let b = R.collector(|s| s.gauge("t_pages", "pages", &[], 5.0));
        assert_eq!(R.snapshot().value("t_pages", &[]), Some(7.0));
        drop(a);
        assert_eq!(R.snapshot().value("t_pages", &[]), Some(5.0));
        drop(b);
        assert_eq!(R.snapshot().value("t_pages", &[]), None);
    }
}
