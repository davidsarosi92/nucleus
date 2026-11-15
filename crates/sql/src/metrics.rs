//! The SQL layer's metrics (PLAN.md §M14.18): statements, their
//! latencies and rows, embedding models' calls, sessions; and the lower
//! layers' instruments registered with them.  `nc_catalog.metrics` reads
//! the process's registry, as the exporters do.

use std::sync::OnceLock;

use nucleus_platform::metrics::{Collector, Counter, CounterFamily, Histogram, global};

/// Statements that read only.
pub static READS: Counter = Counter::labeled(
    "nucleus_statements_total",
    "Statements run, by kind: read (queries) or write (everything else).",
    &[("kind", "read")],
);
/// Statements that may write.
pub static WRITES: Counter = Counter::labeled(
    "nucleus_statements_total",
    "Statements run, by kind: read (queries) or write (everything else).",
    &[("kind", "write")],
);
/// Statements that failed.
pub static ERRORS: Counter = Counter::new(
    "nucleus_statement_errors_total",
    "Statements that ended in an error.",
);
/// How long reads took.
pub static READ_TIME: Histogram = Histogram::labeled(
    "nucleus_statement_duration_seconds",
    "How long statements took, by kind (waits for a resource budget left out).",
    &[("kind", "read")],
);
/// How long writes took.
pub static WRITE_TIME: Histogram = Histogram::labeled(
    "nucleus_statement_duration_seconds",
    "How long statements took, by kind (waits for a resource budget left out).",
    &[("kind", "write")],
);
/// Rows statements read.
pub static ROWS_READ: Counter = Counter::new(
    "nucleus_rows_read_total",
    "Rows statements read from tables and indexes.",
);
/// Rows statements returned or changed.
pub static ROWS_RETURNED: Counter = Counter::new(
    "nucleus_rows_returned_total",
    "Rows statements returned, or changed (INSERT, UPDATE, DELETE).",
);
/// Calls to embedding models.
pub static MODEL_CALLS: Counter = Counter::new(
    "nucleus_model_calls_total",
    "Calls statements made to embedding models' providers.",
);
/// Calls to embedding models that failed.
pub static MODEL_ERRORS: Counter = Counter::new(
    "nucleus_model_errors_total",
    "Calls to embedding models that failed.",
);
/// Tokens sent to embedding models.
pub static MODEL_TOKENS: Counter = Counter::new(
    "nucleus_model_tokens_total",
    "Tokens sent to embedding models, as the providers count them.",
);
/// Texts answered from the embedding cache.
pub static MODEL_CACHED: Counter = Counter::new(
    "nucleus_model_cached_total",
    "Texts whose embedding came from the cache, without a call.",
);
/// Sessions opened, by front end.
pub static SESSIONS_OPENED: CounterFamily = CounterFamily::new(
    "nucleus_sessions_opened_total",
    "Sessions opened (logins admitted), by front end.",
    "front_end",
);
/// Logins refused, by front end.
pub static LOGINS_REFUSED: CounterFamily = CounterFamily::new(
    "nucleus_logins_refused_total",
    "Logins refused (a wrong password, an unknown role, an access rule), by front end.",
    "front_end",
);
/// Logins refused because the role was locked.
pub static LOGINS_LOCKED: Counter = Counter::new(
    "nucleus_logins_locked_total",
    "Logins refused because the role is locked after too many failures.",
);

/// One statement's run, recorded.
pub(crate) fn statement(
    read: bool,
    ok: bool,
    elapsed: std::time::Duration,
    rows_read: u64,
    rows_returned: u64,
    models: &crate::budget::ModelCost,
) {
    let (n, time) = if read {
        (&READS, &READ_TIME)
    } else {
        (&WRITES, &WRITE_TIME)
    };
    n.inc();
    time.observe(elapsed);
    if !ok {
        ERRORS.inc();
    }
    ROWS_READ.add(rows_read);
    ROWS_RETURNED.add(rows_returned);
    if models.calls > 0 || models.cached > 0 {
        MODEL_CALLS.add(models.calls);
        MODEL_ERRORS.add(models.errors);
        MODEL_TOKENS.add(models.tokens);
        MODEL_CACHED.add(models.cached);
    }
}

/// Adds this layer's instruments, the lower layers', and the sessions'
/// collector to the process's registry (the first call; later ones cost a
/// load).
pub fn register() {
    static SESSIONS: OnceLock<Collector> = OnceLock::new();
    SESSIONS.get_or_init(|| {
        nucleus_kv::register_metrics();
        global().register(&[
            &READS,
            &WRITES,
            &ERRORS,
            &READ_TIME,
            &WRITE_TIME,
            &ROWS_READ,
            &ROWS_RETURNED,
            &MODEL_CALLS,
            &MODEL_ERRORS,
            &MODEL_TOKENS,
            &MODEL_CACHED,
            &SESSIONS_OPENED,
            &LOGINS_REFUSED,
            &LOGINS_LOCKED,
        ]);
        global().collector(|s| {
            s.gauge(
                "nucleus_build_info",
                "Always 1: the version running, as a label.",
                &[("version", env!("CARGO_PKG_VERSION"))],
                1.0,
            );
            s.gauge(
                "nucleus_ready",
                "1 once the server is ready to serve (recovery done, the listeners up), else 0.",
                &[],
                f64::from(u8::from(nucleus_platform::metrics::ready())),
            );
            for (front_end, n) in crate::sessions::by_front_end() {
                s.gauge(
                    "nucleus_sessions",
                    "Sessions open now, by front end.",
                    &[("front_end", &front_end)],
                    n as f64,
                );
            }
        })
    });
}
