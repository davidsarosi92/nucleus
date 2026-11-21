//! The document form measured (PLAN.md §M15 D1): nucleus's `json` form —
//! canonical, keys sorted, offset tables — against BSON's (the `bson`
//! crate's raw documents: fields in the order written, each found by
//! walking the ones before it), on documents as applications keep them,
//! of ~1 KB and ~16 KB:
//!
//! - a field of the top object — the second written, the middle one and
//!   the last (BSON's walk grows with the position; nucleus's binary
//!   search does not);
//! - a path three deep (`address.geo.lat`);
//! - the whole document out as JSON text (BSON's through the crate's
//!   relaxed Extended JSON, as a driver prints one);
//! - the size of each form, beside the text's.
//!
//! Each figure is the median of `ROUNDS` timed runs of ~20 ms each,
//! printed as `@metric` lines for `bench/method.py run`.
//!
//!     forms                  the measurements
//!     forms --emit small N   the top object's keys as written (a JSON
//!                            array), then N documents of the small kind
//!                            as JSON lines (bench/json/forms.py loads
//!                            them into SQL)

use std::time::Instant;

use bson::{Bson, Document, RawDocument, doc};
use nucleus_sql::json::{form, parse, to_text};

const ROUNDS: usize = 15;

/// SplitMix64: the same documents on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn word(&mut self) -> String {
        let len = 3 + self.below(8);
        (0..len)
            .map(|_| (b'a' + self.below(26) as u8) as char)
            .collect()
    }

    fn text(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A user's document: profile fields, an address, preferences, orders and
/// a text — and, for the large kind, 80 more fields and a history.
fn document(r: &mut Rng, large: bool) -> Document {
    let id = format!(
        "{:08x}-{:04x}-7{:03x}-{:04x}-{:012x}",
        r.next() as u32,
        r.below(1 << 16),
        r.below(1 << 12),
        r.below(1 << 16),
        r.next() & 0xffff_ffff_ffff
    );
    let mut d = doc! {
        "_id": id,
        "name": format!("{} {}", r.word(), r.word()),
        "email": format!("{}@{}.com", r.word(), r.word()),
        "age": (18 + r.below(60)) as i64,
        "active": r.below(2) == 0,
        "score": r.below(100_000) as f64 / 100.0 + 0.5,
        "created": format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", 1 + r.below(12), 1 + r.below(28), r.below(24), r.below(60)),
        "tags": (0..5).map(|_| Bson::String(r.word())).collect::<Vec<_>>(),
        "address": {
            "street": format!("{} {}", 1 + r.below(999), r.text(2)),
            "city": r.word(),
            "zip": format!("{:05}", r.below(100_000)),
            // Odd thousandths: written as such, no trailing zero (whose
            // scale jsonb keeps and nucleus's canonical form does not).
            "geo": { "lat": thousandths(r, 90_000), "lon": thousandths(r, 180_000) },
        },
        "prefs": { "theme": r.word(), "lang": r.word(), "notify": { "email": true, "sms": r.below(2) == 0 } },
    };
    let orders = if large { 40 } else { 3 };
    d.insert(
        "orders",
        (0..orders)
            .map(|i| {
                Bson::Document(doc! {
                    "id": (1000 * i + r.below(1000)) as i64,
                    "total": r.below(1_000_000) as f64 / 100.0 + 0.01,
                    "items": (1 + r.below(9)) as i64,
                    "state": r.word(),
                })
            })
            .collect::<Vec<_>>(),
    );
    d.insert("bio", r.text(if large { 120 } else { 30 }));
    if large {
        for (i, key) in extra_keys().into_iter().enumerate() {
            let v = match i % 4 {
                0 => Bson::Int64(r.below(1 << 40) as i64),
                1 => Bson::String(r.text(3)),
                2 => Bson::Double(r.below(1 << 30) as f64 / 7.0),
                _ => Bson::Document(doc! { "k": r.word(), "n": r.below(1000) as i64 }),
            };
            d.insert(key, v);
        }
        d.insert(
            "history",
            (0..200)
                .map(|_| {
                    Bson::Document(doc! {
                        "at": format!("2026-{:02}-{:02}", 1 + r.below(12), 1 + r.below(28)),
                        "kind": r.word(),
                        "value": r.below(10_000) as i64,
                    })
                })
                .collect::<Vec<_>>(),
        );
    }
    d.insert("status", r.word());
    d
}

/// An odd number of thousandths in (-bound, bound), as a double.
fn thousandths(r: &mut Rng, bound: u64) -> f64 {
    (2 * r.below(bound) as i64 + 1 - bound as i64) as f64 / 1000.0
}

/// The large kind's 80 more fields' names: the same in every document.
fn extra_keys() -> Vec<String> {
    let mut r = Rng(99);
    (0..80).map(|i| format!("{}{i:02}", r.word())).collect()
}

/// The document as JSON text (relaxed Extended JSON: plain numbers,
/// strings, booleans — every value here is JSON's).
fn text_of(d: &Document) -> String {
    serde_json::to_string(&Bson::Document(d.clone()).into_relaxed_extjson()).unwrap()
}

/// Nanoseconds an operation: the median of `ROUNDS` runs, each of as many
/// operations as take ~20 ms (found after 50 ms of warming up).
fn ns(mut f: impl FnMut()) -> f64 {
    let s = Instant::now();
    let mut n = 0usize;
    while s.elapsed().as_secs_f64() < 0.05 {
        f();
        n += 1;
    }
    let reps = (n * 2 / 5).max(1);
    let mut t: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let s = Instant::now();
            for _ in 0..reps {
                f();
            }
            s.elapsed().as_secs_f64() * 1e9 / reps as f64
        })
        .collect();
    t.sort_by(f64::total_cmp);
    t[ROUNDS / 2]
}

fn metric(name: &str, value: f64, unit: &str, system: &str) {
    println!(
        "@metric {{\"name\": {name:?}, \"values\": [{value}], \"unit\": {unit:?}, \
         \"system\": {system:?}, \"phase\": \"warm\"}}"
    );
}

/// The bytes of `key`'s value in the form `b`'s top object.
fn field<'a>(b: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let form::Node::Object(o) = form::root(b).ok()? else {
        return None;
    };
    o.get_bytes(key).ok()?
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--emit") {
        let large = args[2] == "large";
        let n: usize = args[3].parse().expect("a count");
        let mut r = Rng(if large { 16 } else { 1 });
        for i in 0..n {
            let d = document(&mut r, large);
            if i == 0 {
                // The keys as written first (the text's are sorted).
                println!(
                    "{}",
                    serde_json::to_string(&d.keys().collect::<Vec<_>>()).unwrap()
                );
            }
            println!("{}", text_of(&d));
        }
        return;
    }
    println!(
        "{:<6} {:>7} {:>7} {:>7} | {:<22} {:>9} {:>9}",
        "kind", "text B", "json B", "bson B", "read", "json ns", "bson ns"
    );
    for (kind, large) in [("1 KB", false), ("16 KB", true)] {
        let mut r = Rng(if large { 16 } else { 1 });
        let d = document(&mut r, large);
        let text = text_of(&d);
        let ours = parse::parse(&text).expect("parses");
        let mut bytes = Vec::new();
        d.to_writer(&mut bytes).unwrap();
        let raw = RawDocument::from_bytes(&bytes).unwrap();
        let keys: Vec<&str> = d.keys().map(String::as_str).collect();
        let reads = [
            ("second field", keys[1]),
            ("middle field", keys[keys.len() / 2]),
            ("last field", keys[keys.len() - 1]),
        ];
        for (sys, size) in [
            ("text", text.len()),
            ("nucleus", ours.len()),
            ("bson", bytes.len()),
        ] {
            metric(&format!("size ({kind})"), size as f64, "B", sys);
        }
        let mut rows: Vec<(String, f64, f64)> = Vec::new();
        for (what, key) in reads {
            assert!(field(&ours, key).is_some() && raw.get(key).unwrap().is_some());
            let a = ns(|| {
                std::hint::black_box(field(std::hint::black_box(&ours), key));
            });
            let b = ns(|| {
                std::hint::black_box(std::hint::black_box(raw).get(key).unwrap());
            });
            rows.push((format!("{what} ({key})"), a, b));
        }
        // Each to the value's bytes (BSON's a typed reference to them).
        fn path(b: &[u8]) -> Option<&[u8]> {
            field(field(field(b, "address")?, "geo")?, "lat")
        }
        assert!(path(&ours).is_some());
        let a = ns(|| {
            std::hint::black_box(path(std::hint::black_box(&ours)));
        });
        let b = ns(|| {
            let raw = std::hint::black_box(raw);
            let geo = raw
                .get_document("address")
                .and_then(|a| a.get_document("geo"))
                .unwrap();
            let lat = geo.get("lat").unwrap().unwrap();
            std::hint::black_box(lat);
        });
        rows.push(("path address.geo.lat".into(), a, b));
        let a = ns(|| {
            std::hint::black_box(to_text(std::hint::black_box(&ours)).unwrap());
        });
        let b = ns(|| {
            let d = Document::try_from(std::hint::black_box(raw)).unwrap();
            let v = Bson::Document(d).into_relaxed_extjson();
            std::hint::black_box(serde_json::to_string(&v).unwrap());
        });
        rows.push(("whole document as text".into(), a, b));
        for (i, (what, a, b)) in rows.iter().enumerate() {
            let name = format!("{} ({kind})", what.split(" (").next().unwrap_or(what));
            metric(&name, *a, "ns", "nucleus");
            metric(&name, *b, "ns", "bson");
            let sizes = if i == 0 {
                format!("{:>7} {:>7} {:>7}", text.len(), ours.len(), bytes.len())
            } else {
                format!("{:>7} {:>7} {:>7}", "", "", "")
            };
            println!("{kind:<6} {sizes} | {what:<22} {a:>9.1} {b:>9.1}");
        }
    }
}
