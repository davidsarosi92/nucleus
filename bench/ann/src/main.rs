//! The HNSW prototypes (PLAN.md §M14.12 V4): one core ([`hnsw`]), the
//! storage models it is measured on, and the measurements.
//!
//! - `ann random out=DIR [n=10000] [dim=128] [queries=200] [seed=1]` —
//!   a dataset of random vectors (uniform in [-1, 1)) with its exact
//!   truth, in `bench/ann/data.py`'s format.
//! - `ann recall data=DIR [m=16] [efc=64] [slack=1] [k=10] [threads=10]
//!   [batch=0] [cap=1000] [join=1] [rows=all] [queries=all]` — the graph built in memory (one row at a
//!   time in key order, or `batch` > 0: [`hnsw::insert_batch`]) and
//!   searched: recall@k, latency on one thread, queries per
//!   second on `threads`, hops and distances per search, for `ef_search`
//!   10–400.  `bench/ann/hnswlib_ref.py` gives hnswlib's on the same data.
//! - `ann model data=DIR db=DIR [bulk=90%] [rows=all] [threads=10]
//!   [fold=10000] [seal=0] [auto=0] [page=64] [chunk=4096] [compact=0.1]
//!   [epochs=1] [vacuum=1] [table=0] [churn=0] [batch=0.02] [cap=1000]
//!   [join=1] [m=16] [efc=64] [slack=1] [k=10] [queries=1000]` — the
//!   model chosen, (C) ([`paged`]), on the KV in `db` (a new directory):
//!   the index built over the first `bulk` rows in one transaction, its
//!   recall curve; the other rows inserted by `threads` writers, a
//!   transaction each, while one thread keeps the model up (`fold` rows
//!   a fold, `seal` an epoch, or with `auto` sizes chosen by measured
//!   costs, these the caps; a VACUUM after a fold with `vacuum`) and one
//!   searches; the curve again.  With `table` the rows are kept in a table
//!   beside the index and checked against it, and `churn` rows are then
//!   deleted and as many updated the same way.  `ANN_EFS` (a comma list)
//!   sets the curve's `ef`s; `ANN_MEM` adds the same graph in memory;
//!   `ANN_TRACE` prints each fold.  `bench/ann/pgvector_ref.py` measures
//!   pgvector the same way.

mod data;
mod hnsw;
mod mem;
mod model;
mod paged;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use hnsw::{Metric, Node, Params, Scratch};
use model::Model;
use nucleus_kernels::mix;
use nucleus_kv::{Database, Kv, Options};

/// `key=value` arguments.
struct Args(HashMap<String, String>);

impl Args {
    fn parse(args: &[String]) -> Self {
        Self(
            args.iter()
                .map(|a| match a.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => panic!("expected key=value, got {a}"),
                })
                .collect(),
        )
    }

    fn num(&self, key: &str, default: usize) -> usize {
        self.0.get(key).map_or(default, |v| {
            v.parse().unwrap_or_else(|_| panic!("{key}={v}"))
        })
    }

    fn real(&self, key: &str, default: f64) -> f64 {
        self.0.get(key).map_or(default, |v| {
            v.parse().unwrap_or_else(|_| panic!("{key}={v}"))
        })
    }

    fn text(&self, key: &str) -> &str {
        self.0
            .get(key)
            .unwrap_or_else(|| panic!("{key}= is needed"))
    }

    fn batching(&self, default_ratio: f64) -> hnsw::Batching {
        hnsw::Batching {
            ratio: self.real("batch", default_ratio),
            cap: self.num("cap", 1000),
            join: self.num("join", 1) == 1,
            threads: self.num("threads", 10),
        }
    }

    fn params(&self, metric: Metric) -> Params {
        Params {
            m: self.num("m", 16),
            ef_construction: self.num("efc", 64),
            metric,
            slack: self.num("slack", 1),
        }
    }
}

/// `n` vectors of `dim` components uniform in [-1, 1), from `seed`, as
/// little-endian `f32`s.
fn random_vectors(n: usize, dim: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * dim * 4);
    let mut x = mix(seed);
    for _ in 0..n * dim {
        x = mix(x.wrapping_add(0x9e37_79b9_7f4a_7c15));
        let f = (x >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn random(a: &Args) {
    let (n, dim, queries) = (a.num("n", 10_000), a.num("dim", 128), a.num("queries", 200));
    let seed = a.num("seed", 1) as u64;
    let out = Path::new(a.text("out"));
    data::Dataset::write(
        dim,
        Metric::L2,
        random_vectors(n, dim, seed),
        random_vectors(queries, dim, seed ^ 0xdead_beef),
        out,
    );
    println!("{}: {n} × {dim}, {queries} queries, l2", out.display());
}

/// The share of `truth` that `got` found.
fn recall(got: &[u32], truth: &[u32]) -> f64 {
    let hits = got.iter().filter(|g| truth.contains(g)).count();
    hits as f64 / truth.len() as f64
}

/// The recall–speed curve of `search` (the rows found for a query at an
/// `ef`): recall@k, µs a search on one thread, searches a second on
/// `threads`, hops and distances a search.
fn curve<F>(queries: &[&[u8]], truth: &[Vec<u32>], k: usize, threads: usize, search: F)
where
    F: Fn(&[u8], usize, &mut Scratch) -> Vec<u32> + Sync,
{
    let nq = queries.len();
    let efs: Vec<usize> = std::env::var("ANN_EFS")
        .map(|v| v.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![10, 20, 40, 80, 120, 200, 400]);
    println!("ef_search  recall@{k}  µs/search  QPS({threads})  hops  distances");
    for ef in efs {
        let mut s = Scratch::default();
        let mut total = 0.0;
        let t = Instant::now();
        for (q, want) in queries.iter().zip(truth) {
            total += recall(&search(q, ef, &mut s), &want[..k]);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / nq as f64;
        let t = Instant::now();
        std::thread::scope(|sc| {
            for part in queries.chunks(nq.div_ceil(threads)) {
                let search = &search;
                sc.spawn(move || {
                    let mut s = Scratch::default();
                    for q in part {
                        std::hint::black_box(search(q, ef, &mut s));
                    }
                });
            }
        });
        let qps = nq as f64 / t.elapsed().as_secs_f64();
        println!(
            "{ef:>9}  {:>9.4}  {us:>9.1}  {qps:>8.0}  {:>4}  {:>9}",
            total / nq as f64,
            s.hops / nq as u64,
            s.distances / nq as u64
        );
    }
}

/// Each query's exact `k` nearest among the first `rows` vectors, on
/// `threads` threads.
fn exact_truth(
    d: &data::Dataset,
    rows: usize,
    queries: &[&[u8]],
    k: usize,
    threads: usize,
) -> Vec<Vec<u32>> {
    let w = d.width();
    let train = &d.train[..rows * w];
    let mut out = vec![Vec::new(); queries.len()];
    std::thread::scope(|sc| {
        for (qs, outs) in queries
            .chunks(queries.len().div_ceil(threads))
            .zip(out.chunks_mut(queries.len().div_ceil(threads)))
        {
            sc.spawn(move || {
                for (q, o) in qs.iter().zip(outs) {
                    *o = hnsw::exact(d.metric, train, w, q, k)
                        .iter()
                        .map(|x| x.n)
                        .collect();
                }
            });
        }
    });
    out
}

fn recall_curve(a: &Args) {
    let d = data::Dataset::read(Path::new(a.text("data")));
    let (k, threads) = (a.num("k", 10), a.num("threads", 10));
    let p = a.params(d.metric);
    let width = d.width();
    let n = a.num("rows", d.rows()).min(d.rows());

    let b = a.batching(0.0);
    let t = Instant::now();
    let mut g = mem::Mem::new(p.metric, width);
    let mut s = Scratch::default();
    if b.ratio > 0.0 {
        let nodes: Vec<(Node, &[u8])> = d
            .train
            .chunks_exact(width)
            .take(n)
            .enumerate()
            .map(|(i, v)| (i as Node, v))
            .collect();
        hnsw::insert_batch(&mut g, &p, &nodes, &b, &mut s);
    } else {
        for (i, v) in d.train.chunks_exact(width).take(n).enumerate() {
            hnsw::insert(&mut g, &p, i as Node, v, &mut s);
        }
    }
    let build = t.elapsed().as_secs_f64();
    println!(
        "memory ({}): n {n}, dim {}, M {}, ef_construction {}, slack {}: built in {build:.1} s ({:.1} µs/insert, {} distances/insert), {:.1} MB",
        if b.ratio > 0.0 {
            format!(
                "batches of {} up to {}{} on {threads} threads",
                b.ratio,
                b.cap,
                if b.join { ", joined" } else { "" }
            )
        } else {
            "one at a time".into()
        },
        d.dim,
        p.m,
        p.ef_construction,
        p.slack,
        build * 1e6 / n as f64,
        s.distances / n as u64,
        g.bytes() as f64 / 1e6
    );
    let queries: Vec<&[u8]> = d
        .test
        .chunks_exact(width)
        .take(a.num("queries", usize::MAX))
        .collect();
    let truth = if n < d.rows() {
        exact_truth(&d, n, &queries, k, threads)
    } else {
        d.truth.clone()
    };
    curve(&queries, &truth, k, threads, |q, ef, s| {
        hnsw::search(&g, q, k, ef, s).iter().map(|x| x.n).collect()
    });
}

/// The process's resident memory, in MB (`ps`).
fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

fn model_run(a: &Args) {
    let d = data::Dataset::read(Path::new(a.text("data")));
    let (k, threads) = (a.num("k", 10), a.num("threads", 10));
    let p = a.params(d.metric);
    let width = d.width();
    let rows = a.num("rows", d.rows()).min(d.rows());
    let bulk = a.num("bulk", rows * 9 / 10).min(rows);
    let fold = a.num("fold", 10_000);
    let vacuum = a.num("vacuum", 1) == 1;
    let db = Path::new(a.text("db"));
    assert!(
        !db.exists(),
        "{} exists: give a new directory",
        db.display()
    );
    let kv: Arc<dyn Kv> = Arc::new(Database::open(db, Options::default()).expect("open"));
    let m: Box<dyn Model> = match a.0.get("model").map_or("c", String::as_str) {
        "c" => Box::new(paged::Paged::new(
            kv.clone(),
            paged::Config {
                params: p,
                width,
                page: a.num("page", 64),
                chunk: a.num("chunk", 4096).div_ceil(a.num("page", 64)) * a.num("page", 64),
                compact: a.real("compact", 0.1),
                batch: a.batching(0.02),
                epochs: a.num("epochs", 1) == 1,
                seal: a.num("seal", 0),
                auto: a.num("auto", 0) == 1,
                table: a.num("table", 0) == 1,
            },
        )),
        other => panic!("model={other}: c (A and B were measured and dropped: PLAN.md §M14.12 V4)"),
    };
    let queries: Vec<&[u8]> = d
        .test
        .chunks_exact(width)
        .take(a.num("queries", 1000))
        .collect();
    println!(
        "{}: {} rows ({} bulk, {} inserted by {threads} writers, fold {fold}), dim {}, M {}, ef_construction {}, slack {}",
        m.name(),
        rows,
        bulk,
        rows - bulk,
        d.dim,
        p.m,
        p.ef_construction,
        p.slack
    );
    let search = |q: &[u8], ef: usize, s: &mut Scratch| -> Vec<u32> {
        m.search(q, k, ef, s).iter().map(|h| h.row).collect()
    };

    // The index built over the first rows at once.
    let vectors: Vec<(u32, &[u8])> = d
        .train
        .chunks_exact(width)
        .take(rows)
        .enumerate()
        .map(|(i, v)| (i as u32, v))
        .collect();
    let t = Instant::now();
    m.bulk(&vectors[..bulk]);
    let secs = t.elapsed().as_secs_f64();
    println!(
        "bulk: {bulk} rows in {secs:.1} s ({:.1} µs/row), {:.0} bytes written/row, RSS {:.0} MB; {}",
        secs * 1e6 / bulk as f64,
        m.written() as f64 / bulk as f64,
        rss_mb(),
        m.report()
    );
    let truth = exact_truth(&d, bulk, &queries, k, threads);
    curve(&queries, &truth, k, threads, search);
    if std::env::var("ANN_MEM").is_ok() {
        // The same rows in memory, searched the same way: the reference.
        let mut g = mem::Mem::new(p.metric, width);
        let mut s = Scratch::default();
        hnsw::insert_batch(&mut g, &p, &vectors[..bulk], &a.batching(0.02), &mut s);
        println!("memory, the same build:");
        curve(&queries, &truth, k, threads, |q, ef, s| {
            hnsw::search(&g, q, k, ef, s).iter().map(|x| x.n).collect()
        });
    }

    // The rest inserted concurrently, kept up, searched meanwhile.
    if rows > bulk {
        let before = m.written();
        let next = AtomicUsize::new(bulk);
        let retries = AtomicU64::new(0);
        let done = AtomicBool::new(false);
        let (searches, search_us) = (AtomicU64::new(0), AtomicU64::new(0));
        let t = Instant::now();
        std::thread::scope(|sc| {
            let writers: Vec<_> = (0..threads)
                .map(|_| {
                    sc.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, Relaxed);
                            if i >= rows {
                                break;
                            }
                            retries.fetch_add(u64::from(m.insert(i as u32, vectors[i].1)), Relaxed);
                        }
                    })
                })
                .collect();
            sc.spawn(|| {
                // After an upkeep, a VACUUM (the engine's background one):
                // the delta's deleted keys would cost every scan of it.
                while !done.load(Relaxed) {
                    if m.maintain(fold) {
                        if vacuum {
                            let t = Instant::now();
                            kv.vacuum().expect("vacuum");
                            if std::env::var_os("ANN_TRACE").is_some() {
                                eprintln!("vacuum: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
                            }
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            });
            sc.spawn(|| {
                let mut s = Scratch::default();
                let mut i = 0;
                while !done.load(Relaxed) {
                    let t = Instant::now();
                    std::hint::black_box(m.search(queries[i % queries.len()], k, 40, &mut s));
                    search_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                    searches.fetch_add(1, Relaxed);
                    i += 1;
                }
            });
            let joined: Vec<_> = writers.into_iter().map(|w| w.join()).collect();
            done.store(true, Relaxed);
            for j in joined {
                j.unwrap();
            }
        });
        let secs = t.elapsed().as_secs_f64();
        let n = (rows - bulk) as f64;
        let ns = searches.load(Relaxed).max(1);
        println!(
            "inserts: {} rows in {secs:.1} s ({:.0} rows/s), {} retries, {:.0} bytes written/row; searches meanwhile (ef 40): {ns}, {:.0} µs each; RSS {:.0} MB; {}",
            rows - bulk,
            n / secs,
            retries.load(Relaxed),
            (m.written() - before) as f64 / n,
            search_us.load(Relaxed) as f64 / ns as f64,
            rss_mb(),
            m.report()
        );
        println!("as left (delta unfolded):");
        let truth = exact_truth(&d, rows, &queries, k, threads);
        curve(&queries, &truth, k, threads, search);
        let t = Instant::now();
        while m.maintain(1) {}
        if vacuum {
            kv.vacuum().expect("vacuum");
        }
        println!(
            "after the last upkeep (VACUUM {vacuum}; {:.1} s): {}",
            t.elapsed().as_secs_f64(),
            m.report()
        );
        curve(&queries, &truth, k, threads, search);
    }

    // Deletes and updates (`churn` of each, rows apart), by the writers,
    // kept up and searched meanwhile; the curve against the rows left.
    let churn = a.num("churn", 0).min(rows / 2);
    if churn > 0 {
        let mut live: Vec<Option<Vec<u8>>> = vectors[..rows]
            .iter()
            .map(|(_, v)| Some(v.to_vec()))
            .collect();
        let ops: Vec<(u32, Option<Vec<u8>>)> = (0..2 * churn)
            .map(|i| {
                let row = (i * 999_983 + 12_345) % rows;
                let new = (i >= churn)
                    .then(|| blend(vectors[row].1, vectors[(row * 7919 + 1) % rows].1, d.metric));
                (row as u32, new)
            })
            .collect();
        let before = m.written();
        let next = AtomicUsize::new(0);
        let retries = AtomicU64::new(0);
        let (secs, searches, us) = concurrently(
            m.as_ref(),
            kv.as_ref(),
            threads,
            fold,
            vacuum,
            &queries,
            k,
            || {
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some((row, new)) = ops.get(i) else { break };
                    let old = vectors[*row as usize].1;
                    retries.fetch_add(u64::from(m.change(*row, old, new.as_deref())), Relaxed);
                }
            },
        );
        println!(
            "churn: {churn} deletes and {churn} updates in {secs:.1} s ({:.0} a second), {} retries, {:.0} bytes written each; searches meanwhile (ef 40): {searches}, {us:.0} µs each; RSS {:.0} MB; {}",
            ops.len() as f64 / secs,
            retries.load(Relaxed),
            (m.written() - before) as f64 / ops.len() as f64,
            rss_mb(),
            m.report()
        );
        for (row, new) in &ops {
            live[*row as usize] = new.clone();
        }
        let (rows_left, data): (Vec<u32>, Vec<u8>) = live
            .iter()
            .enumerate()
            .filter_map(|(r, v)| v.as_ref().map(|v| (r as u32, v)))
            .fold((Vec::new(), Vec::new()), |(mut r, mut d), (row, v)| {
                r.push(row);
                d.extend_from_slice(v);
                (r, d)
            });
        let truth = exact_over(d.metric, &rows_left, &data, width, &queries, k, threads);
        println!("as left ({} rows):", rows_left.len());
        curve(&queries, &truth, k, threads, search);
        let t = Instant::now();
        while m.maintain(1) {}
        println!(
            "after the last upkeep ({:.1} s): {}",
            t.elapsed().as_secs_f64(),
            m.report()
        );
        curve(&queries, &truth, k, threads, search);
    }
}

/// A vector near `b`, a quarter of the way to `a` (unit length for
/// cosine): an update's new value, among the data rather than between.
fn blend(a: &[u8], b: &[u8], metric: hnsw::Metric) -> Vec<u8> {
    let f = |x: &[u8]| -> Vec<f32> {
        x.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    };
    let mut v: Vec<f32> = f(a)
        .iter()
        .zip(f(b))
        .map(|(x, y)| 0.25 * x + 0.75 * y)
        .collect();
    if matches!(metric, hnsw::Metric::Cosine) {
        let n = v
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::MIN_POSITIVE);
        v.iter_mut().for_each(|x| *x /= n);
    }
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Each query's exact `k` nearest among `data` (the vectors of `rows`).
fn exact_over(
    metric: hnsw::Metric,
    rows: &[u32],
    data: &[u8],
    w: usize,
    queries: &[&[u8]],
    k: usize,
    threads: usize,
) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); queries.len()];
    let per = queries.len().div_ceil(threads);
    std::thread::scope(|sc| {
        for (qs, outs) in queries.chunks(per).zip(out.chunks_mut(per)) {
            sc.spawn(move || {
                for (q, o) in qs.iter().zip(outs) {
                    *o = hnsw::exact(metric, data, w, q, k)
                        .iter()
                        .map(|x| rows[x.n as usize])
                        .collect();
                }
            });
        }
    });
    out
}

/// `work` on `threads` writers while one thread keeps `m` up (`fold`
/// rows a fold, a VACUUM after if `vacuum`) and one searches at `ef` 40;
/// the seconds, the searches and their mean µs.
#[allow(clippy::too_many_arguments)]
fn concurrently(
    m: &dyn Model,
    kv: &dyn Kv,
    threads: usize,
    fold: usize,
    vacuum: bool,
    queries: &[&[u8]],
    k: usize,
    work: impl Fn() + Sync,
) -> (f64, u64, f64) {
    let done = AtomicBool::new(false);
    let (searches, search_us) = (AtomicU64::new(0), AtomicU64::new(0));
    let t = Instant::now();
    std::thread::scope(|sc| {
        let writers: Vec<_> = (0..threads).map(|_| sc.spawn(&work)).collect();
        sc.spawn(|| {
            while !done.load(Relaxed) {
                if m.maintain(fold) {
                    if vacuum {
                        kv.vacuum().expect("vacuum");
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        sc.spawn(|| {
            let mut s = Scratch::default();
            let mut i = 0;
            while !done.load(Relaxed) {
                let t = Instant::now();
                std::hint::black_box(m.search(queries[i % queries.len()], k, 40, &mut s));
                search_us.fetch_add(t.elapsed().as_micros() as u64, Relaxed);
                searches.fetch_add(1, Relaxed);
                i += 1;
            }
        });
        let joined: Vec<_> = writers.into_iter().map(|w| w.join()).collect();
        done.store(true, Relaxed);
        for j in joined {
            j.unwrap();
        }
    });
    let n = searches.load(Relaxed);
    (
        t.elapsed().as_secs_f64(),
        n,
        search_us.load(Relaxed) as f64 / n.max(1) as f64,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = Args::parse(args.get(1..).unwrap_or_default());
    match args.first().map(String::as_str) {
        Some("random") => random(&rest),
        Some("recall") => recall_curve(&rest),
        Some("model") => model_run(&rest),
        _ => {
            eprintln!(
                "usage: ann random out=DIR [n=10000] [dim=128] [queries=200] [seed=1]\n       ann recall data=DIR [m=16] [efc=64] [slack=1] [k=10] [threads=10] [queries=N]\n       ann model data=DIR db=DIR [bulk=N] [rows=N] [threads=10] [fold=10000] [seal=0] [auto=0] [page=64] [chunk=4096] [compact=0.1] [epochs=1] [vacuum=1] [table=0] [churn=0] [queries=1000]"
            );
            std::process::exit(2);
        }
    }
}
