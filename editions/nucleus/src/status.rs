//! `nucleus status` (PLAN.md §M14.18): a server's health for people —
//! its metrics (`nc_catalog.metrics`, what Prometheus scrapes) read over
//! the connection and summed up in a few lines.

use std::collections::BTreeMap;

use crate::sql::{Backend, Res, Target};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: nucleus status [<dir>] [connection options of nucleus sql]
  a server's (or a database directory's) health: whether it is ready,
  sessions, transactions, statements and their latencies, the page
  cache, the files' sizes, refused logins — from nc_catalog.metrics
";

/// The metrics: `(name, labels)` → value.
type Metrics = BTreeMap<(String, String), f64>;

pub fn run(args: &[String]) -> Result<()> {
    let mut target = Target::new("nucleus status");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        if a == "--help" {
            print!("{USAGE}");
            return Ok(());
        }
        if !target.option(a, &mut value)? {
            return Err(format!("unknown option {a}\n{USAGE}"));
        }
    }
    let mut b = target.open()?;
    let read = read(&mut b);
    b.close()?;
    print!("{}", report(&read?));
    Ok(())
}

fn read(b: &mut Backend) -> Result<Metrics> {
    let sql = "SELECT name, labels, value FROM nc_catalog.metrics";
    let mut out = Metrics::new();
    for r in b.execute(sql)? {
        match r {
            Ok(Res::Rows(t)) => {
                for row in t.rows {
                    let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
                    let v = cell(2).parse().unwrap_or(f64::NAN);
                    out.insert((cell(0), cell(1)), v);
                }
            }
            Ok(Res::Command(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

fn get(m: &Metrics, name: &str, labels: &str) -> f64 {
    m.get(&(name.to_string(), labels.to_string()))
        .copied()
        .unwrap_or(0.0)
}

/// Every series of `name`: labels → value.
fn all<'a>(m: &'a Metrics, name: &'a str) -> impl Iterator<Item = (&'a str, f64)> + 'a {
    m.iter()
        .filter(move |((n, _), _)| n == name)
        .map(|((_, l), v)| (l.as_str(), *v))
}

/// A count with thousands apart: 1,234,567.
fn count(v: f64) -> String {
    let s = format!("{}", v.max(0.0).round() as u64);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Bytes in the largest unit that keeps a number above 1: 12.5 MB.
fn bytes(v: f64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB"];
    let mut v = v.max(0.0);
    let mut i = 0;
    while v >= 1000.0 && i + 1 < units.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{v} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// Seconds as ms or s.
fn time(s: f64) -> String {
    if s < 1.0 {
        format!("{:.2} ms", s * 1000.0)
    } else {
        format!("{s:.2} s")
    }
}

/// The `q` quantile of histogram `name` (summed over `kind`s), as
/// Prometheus's `histogram_quantile` estimates it: linear within the
/// bucket it falls in.
fn quantile(m: &Metrics, name: &str, q: f64) -> Option<f64> {
    let mut buckets: BTreeMap<u64, f64> = BTreeMap::new();
    let bucket = format!("{name}_bucket");
    for (labels, v) in all(m, &bucket) {
        let le = labels.split("le=\"").nth(1)?.trim_end_matches('"');
        let le = if le == "+Inf" {
            f64::INFINITY
        } else {
            le.parse().ok()?
        };
        *buckets.entry(le.to_bits()).or_default() += v;
    }
    let mut bounds: Vec<(f64, f64)> = buckets
        .into_iter()
        .map(|(b, n)| (f64::from_bits(b), n))
        .collect();
    bounds.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total = bounds.last()?.1;
    if total == 0.0 {
        return None;
    }
    let rank = q * total;
    let mut lower = (0.0, 0.0);
    for (bound, n) in bounds {
        if n >= rank {
            if bound.is_infinite() {
                return Some(lower.0);
            }
            let within = if n > lower.1 {
                (rank - lower.1) / (n - lower.1)
            } else {
                1.0
            };
            return Some(lower.0 + (bound - lower.0) * within);
        }
        lower = (bound, n);
    }
    None
}

fn report(m: &Metrics) -> String {
    let version = all(m, "nucleus_build_info")
        .find_map(|(l, _)| {
            l.strip_prefix("version=\"")
                .map(|v| v.trim_end_matches('"').to_string())
        })
        .unwrap_or_else(|| "?".into());
    let ready = get(m, "nucleus_ready", "") == 1.0;
    let mut out = format!(
        "nucleus {version}: {}\n",
        if ready {
            "ready"
        } else {
            "not ready (no server: the directory opened here)"
        }
    );
    let mut line = |what: &str, text: String| out.push_str(&format!("  {what:<15} {text}\n"));
    let sessions: Vec<String> = all(m, "nucleus_sessions")
        .map(|(l, v)| {
            format!(
                "{} {}",
                l.trim_start_matches("front_end=\"").trim_end_matches('"'),
                count(v)
            )
        })
        .collect();
    let open: f64 = all(m, "nucleus_sessions").map(|(_, v)| v).sum();
    line(
        "sessions",
        if sessions.is_empty() {
            "0".into()
        } else {
            format!("{} ({})", count(open), sessions.join(", "))
        },
    );
    line(
        "transactions",
        format!(
            "{} committed, {} rolled back, {} conflicts; {} running",
            count(get(m, "nucleus_commits_total", "")),
            count(get(m, "nucleus_rollbacks_total", "")),
            count(get(m, "nucleus_conflicts_total", "")),
            count(get(m, "nucleus_transactions_running", "")),
        ),
    );
    let (reads, writes) = (
        get(m, "nucleus_statements_total", "kind=\"read\""),
        get(m, "nucleus_statements_total", "kind=\"write\""),
    );
    let latency = match (
        quantile(m, "nucleus_statement_duration_seconds", 0.5),
        quantile(m, "nucleus_statement_duration_seconds", 0.99),
    ) {
        (Some(p50), Some(p99)) => format!("; p50 {}, p99 {}", time(p50), time(p99)),
        _ => String::new(),
    };
    line(
        "statements",
        format!(
            "{} (read {}, write {}), {} errors{latency}",
            count(reads + writes),
            count(reads),
            count(writes),
            count(get(m, "nucleus_statement_errors_total", "")),
        ),
    );
    line(
        "rows",
        format!(
            "{} read, {} returned or changed",
            count(get(m, "nucleus_rows_read_total", "")),
            count(get(m, "nucleus_rows_returned_total", "")),
        ),
    );
    let (hits, misses) = (
        get(m, "nucleus_cache_hits_total", ""),
        get(m, "nucleus_cache_misses_total", ""),
    );
    let (pages, used) = (
        get(m, "nucleus_cache_pages", ""),
        get(m, "nucleus_cache_pages_used", ""),
    );
    line(
        "page cache",
        format!(
            "{} hits; {} pages, {:.0}% in use",
            if hits + misses > 0.0 {
                format!("{:.1}%", 100.0 * hits / (hits + misses))
            } else {
                "no reads yet, no".into()
            },
            count(pages),
            if pages > 0.0 {
                100.0 * used / pages
            } else {
                0.0
            },
        ),
    );
    line(
        "data file",
        format!(
            "{} ({} free); {} {}",
            bytes(get(m, "nucleus_data_file_bytes", "")),
            bytes(get(m, "nucleus_data_file_free_bytes", "")),
            count(get(m, "nucleus_branches", "")),
            if get(m, "nucleus_branches", "") == 1.0 {
                "branch"
            } else {
                "branches"
            },
        ),
    );
    line(
        "WAL",
        format!(
            "{} since the last checkpoint; {} written, {} syncs, {} checkpoints",
            bytes(get(m, "nucleus_wal_since_checkpoint_bytes", "")),
            bytes(get(m, "nucleus_wal_bytes_total", "")),
            count(get(m, "nucleus_wal_syncs_total", "")),
            count(get(m, "nucleus_checkpoints_total", "")),
        ),
    );
    let refused: f64 = all(m, "nucleus_logins_refused_total").map(|(_, v)| v).sum();
    line(
        "logins refused",
        format!(
            "{} ({} while locked)",
            count(refused),
            count(get(m, "nucleus_logins_locked_total", "")),
        ),
    );
    let calls = get(m, "nucleus_model_calls_total", "");
    if calls > 0.0 {
        line(
            "models",
            format!(
                "{} calls, {} errors, {} tokens, {} cached",
                count(calls),
                count(get(m, "nucleus_model_errors_total", "")),
                count(get(m, "nucleus_model_tokens_total", "")),
                count(get(m, "nucleus_model_cached_total", "")),
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_for_people() {
        assert_eq!(count(1234567.0), "1,234,567");
        assert_eq!(count(12.0), "12");
        assert_eq!(bytes(12_500_000.0), "12.5 MB");
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(time(0.00025), "0.25 ms");
    }

    #[test]
    fn quantiles_interpolate() {
        let mut m = Metrics::new();
        let b = "x_bucket".to_string();
        m.insert((b.clone(), "kind=\"read\",le=\"0.001\"".into()), 50.0);
        m.insert((b.clone(), "kind=\"read\",le=\"0.002\"".into()), 100.0);
        m.insert((b.clone(), "kind=\"read\",le=\"+Inf\"".into()), 100.0);
        let p50 = quantile(&m, "x", 0.5).unwrap();
        assert!((p50 - 0.001).abs() < 1e-12, "{p50}");
        let p75 = quantile(&m, "x", 0.75).unwrap();
        assert!((p75 - 0.0015).abs() < 1e-12, "{p75}");
        assert_eq!(quantile(&Metrics::new(), "x", 0.5), None);
    }
}
