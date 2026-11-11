//! PostgreSQL's text search (PLAN.md §M14.21 T8): `to_tsvector(cfg, col)
//! @@ maker(cfg, 'q')` rewritten to the text index's `col @@ q` finds the
//! rows the values do — a stored `tsvector` against the same query,
//! computed as PostgreSQL computes it — for random queries of every
//! maker, without the index and through it; and the index serves it.

use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_pg_compat::PgCompat;
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::ext::{Output, Registry, Session};

fn session() -> Session {
    let kv: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(5)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let mut r = Registry::builtin();
    r.add(&PgCompat).unwrap();
    Session::with_registry(kv, Arc::new(r))
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// Words with stems in common, stop words, a hyphenated one.
const WORDS: [&str; 16] = [
    "cat", "cats", "dog", "dogs", "running", "runs", "run", "mouse", "mice", "the", "a", "of",
    "e-mail", "sat", "mat", "Fish",
];

/// Query operands: the words, prefixes, a weight now and then.
const OPERANDS: [&str; 10] = [
    "cat", "dogs", "run:*", "mice", "the", "e-mail", "sat", "ma:*", "fish", "cat:A",
];

fn standard(r: &mut Lcg, depth: u32) -> String {
    if depth == 0 || r.below(3) == 0 {
        return r.pick(&OPERANDS).to_string();
    }
    let (a, b) = (standard(r, depth - 1), standard(r, depth - 1));
    match r.below(6) {
        0 => format!("{a} & {b}"),
        1 => format!("({a} | {b})"),
        2 => format!("!{a}"),
        3 | 4 => format!("({a} <-> {b})"),
        _ => format!("({a} <2> {b})"),
    }
}

fn web(r: &mut Lcg) -> String {
    let n = 1 + r.below(4);
    (0..n)
        .map(|_| match r.below(5) {
            0 => format!("-{}", r.pick(&WORDS)),
            1 => format!("\"{} {}\"", r.pick(&WORDS), r.pick(&WORDS)),
            2 => "or".to_string(),
            _ => r.pick(&WORDS).to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn plain(r: &mut Lcg) -> String {
    let n = 1 + r.below(3);
    (0..n).map(|_| r.pick(&WORDS)).collect::<Vec<_>>().join(" ")
}

fn sql_text(s: &str) -> String {
    s.replace('\'', "''")
}

/// Every random query, by the rewrite and by the stored vectors, alike;
/// how many the rewrite took, found rows, and all.
fn same(s: &mut Session, step: &str) -> (usize, usize, usize) {
    let mut r = Lcg(11);
    let (mut rewritten, mut found, mut all) = (0, 0, 0);
    for k in 0..400 {
        let (maker, q) = match k % 4 {
            0 => ("to_tsquery", standard(&mut r, 3)),
            1 => ("websearch_to_tsquery", web(&mut r)),
            2 => ("plainto_tsquery", plain(&mut r)),
            _ => ("phraseto_tsquery", plain(&mut r)),
        };
        let q = sql_text(&q);
        let by = |v: &str| {
            format!("SELECT id FROM docs WHERE {v} @@ {maker}('english', '{q}') ORDER BY id")
        };
        let rewrite = by("to_tsvector('english', body)");
        let want = rows(s, &by("tv"));
        found += usize::from(!want.is_empty());
        assert_eq!(rows(s, &rewrite), want, "{step}: {maker}('{q}')");
        let plan = rows(s, &format!("EXPLAIN {rewrite}")).join("\n");
        all += 1;
        if !plan.contains("to_tsvector") {
            rewritten += 1;
        }
    }
    (rewritten, found, all)
}

#[test]
fn the_rewrite_finds_what_the_values_do() {
    rewritten_on(&["CREATE TABLE docs (id INT PRIMARY KEY, body TEXT, tv tsvector)"]);
}

/// The same on a `column` table, and on a table partitioned into `row`,
/// `column` and `row,column` partitions (each partition's index serves).
#[test]
fn on_every_kind_of_table() {
    rewritten_on(&[
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT, tv tsvector) WITH (storage = column)",
    ]);
    rewritten_on(&[
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT, tv tsvector) PARTITION BY RANGE (id)",
        "CREATE TABLE docs_a PARTITION OF docs FOR VALUES FROM (MINVALUE) TO (100)",
        "CREATE TABLE docs_b PARTITION OF docs FOR VALUES FROM (100) TO (250) \
         WITH (storage = column)",
        "CREATE TABLE docs_c PARTITION OF docs FOR VALUES FROM (250) TO (MAXVALUE) \
         WITH (storage = 'row,column')",
    ]);
}

fn rewritten_on(ddl: &[&str]) {
    let mut s = session();
    for sql in ddl {
        s.execute(sql).unwrap();
    }
    let mut r = Lcg(3);
    let insert = |s: &mut Session, r: &mut Lcg, ids: std::ops::Range<i64>| {
        let values: Vec<String> = ids
            .map(|id| {
                let body = match id % 40 {
                    7 => "NULL".to_string(),
                    13 => "'the of a'".to_string(),
                    _ => {
                        let n = 1 + r.below(10);
                        let ws: Vec<&str> = (0..n).map(|_| r.pick(&WORDS)).collect();
                        format!("'{}'", ws.join(" "))
                    }
                };
                format!("({id}, {body}, NULL)")
            })
            .collect();
        s.execute(&format!("INSERT INTO docs VALUES {}", values.join(",")))
            .unwrap();
        s.execute("UPDATE docs SET tv = to_tsvector('english', body) WHERE tv IS NULL")
            .unwrap();
    };
    insert(&mut s, &mut r, 0..300);
    let (rewritten, found, all) = same(&mut s, "no index");
    eprintln!("{rewritten} of {all} rewritten, {found} finding rows");
    assert!(rewritten * 2 > all, "{rewritten} of {all} rewritten");
    assert!(found * 2 > all, "{found} of {all} finding rows");
    s.execute("CREATE INDEX docs_body ON docs USING text (body) WITH (analyzer = 'english')")
        .unwrap();
    s.execute("ANALYZE docs").unwrap();
    insert(&mut s, &mut r, 300..400);
    same(&mut s, "index");

    // Served by the index; not where the analyzers differ, nor a weight.
    let explain = |s: &mut Session, sql: &str| rows(s, &format!("EXPLAIN {sql}")).join("\n");
    for (sql, served) in [
        (
            "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'cats & !dog')",
            true,
        ),
        (
            "SELECT id FROM docs WHERE to_tsvector('english', body) @@ phraseto_tsquery('english', 'the cat sat')",
            true,
        ),
        (
            "SELECT id FROM docs WHERE to_tsvector('pg_catalog.english', body) @@ websearch_to_tsquery('english', 'cat or \"dog runs\"')",
            true,
        ),
        (
            "SELECT id FROM docs WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'cats')",
            false,
        ),
        (
            "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'cat:A')",
            false,
        ),
    ] {
        let plan = explain(&mut s, sql);
        assert_eq!(plan.contains("docs_body"), served, "{sql}\n{plan}");
    }
}

/// PostgreSQL's own answers (`bench/text/tsearch_pg.py`): random vectors
/// against random queries — phrases at distances 0 to 3 over operators,
/// negations, prefixes, weights — and the makers' queries, their text
/// forms too; `ts_rank` and `ts_rank_cd`, normalized and weighted, to
/// the bit; `ts_headline`'s headlines, its options too; the binary forms
/// PostgreSQL's wire protocol carries (`tsvectorsend`, `tsquerysend`).
#[test]
fn as_postgresql_answers() {
    let mut s = session();
    let lit = |x: &str| format!("'{}'", sql_text(x));
    let one = |s: &mut Session, sql: &str| rows(s, sql).join(" | ");
    let mut wrong = Vec::new();
    let (mut ranked, mut covered, mut headlined, mut sent) = (0, 0, 0, 0);
    let fixture = include_str!("tsearch_pg.tsv");
    for line in fixture.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "match" => {
                let (v, q) = (lit(f[1]), lit(f[2]));
                let got = one(
                    &mut s,
                    &format!(
                        "SELECT {v}::tsvector::text, {q}::tsquery::text, {v}::tsvector @@ {q}::tsquery"
                    ),
                );
                let want = format!("{} {} {}", f[3], f[4], f[5]);
                if got != want {
                    wrong.push(format!("{} @@ {}: {got}, PostgreSQL {want}", f[1], f[2]));
                }
            }
            "make" => {
                let got = one(
                    &mut s,
                    &format!("SELECT {}('english', {})::text", f[1], lit(f[2])),
                );
                let want = f.get(3).copied().unwrap_or("");
                if got != want {
                    wrong.push(format!(
                        "{}('{}'): {got:?}, PostgreSQL {want:?}",
                        f[1], f[2]
                    ));
                }
            }
            "rank" => {
                let (v, q, n, w) = (
                    format!("{}::tsvector", lit(f[1])),
                    format!("{}::tsquery", lit(f[2])),
                    f[3],
                    format!("{}::real[]", lit(f[4])),
                );
                let got = rows(
                    &mut s,
                    &format!(
                        "SELECT ts_rank({v}, {q}), ts_rank_cd({v}, {q}), ts_rank({v}, {q}, {n}), \
                         ts_rank_cd({v}, {q}, {n}), ts_rank({w}, {v}, {q}), ts_rank_cd({w}, {v}, {q})"
                    ),
                );
                let got: Vec<f32> = got[0].split(' ').map(|x| x.parse().unwrap()).collect();
                let want: Vec<f32> = f[5..11].iter().map(|x| x.parse().unwrap()).collect();
                ranked += usize::from(want[0] > 0.0);
                covered += usize::from(want[1] > 0.0);
                if got
                    .iter()
                    .map(|x| x.to_bits())
                    .ne(want.iter().map(|x| x.to_bits()))
                {
                    wrong.push(format!(
                        "ranks of {} by {} ({n}, {}): {got:?}, PostgreSQL {want:?}",
                        f[1], f[2], f[4]
                    ));
                }
            }
            "headline" => {
                let got = one(
                    &mut s,
                    &format!(
                        "SELECT ts_headline('english', {}, {}::tsquery, {})",
                        lit(f[1]),
                        lit(f[2]),
                        lit(f[3])
                    ),
                );
                let want = f.get(4).copied().unwrap_or("");
                headlined += usize::from(want.contains(['<', '[', '|']));
                if got != want {
                    wrong.push(format!(
                        "ts_headline({:?}, {}, {:?}):\n  {got:?}\n  PostgreSQL {want:?}",
                        f[1], f[2], f[3]
                    ));
                }
            }
            "send" => {
                let got = one(
                    &mut s,
                    &format!(
                        "SELECT tsvectorsend({}::tsvector), tsquerysend({}::tsquery)",
                        lit(f[1]),
                        lit(f[2])
                    ),
                );
                let want = format!("{} {}", f[3], f[4]);
                sent += 1;
                if got != want {
                    wrong.push(format!(
                        "sent {:?}, {:?}: {got}, PostgreSQL {want}",
                        f[1], f[2]
                    ));
                }
            }
            other => panic!("{other}"),
        }
    }
    assert!(
        ranked > 300 && covered > 300,
        "{ranked}, {covered} ranked above 0"
    );
    assert!(headlined > 300, "{headlined} headlines mark a word");
    assert!(sent > 300, "{sent} binary forms");
    assert!(fixture.lines().count() >= 5400);
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        fixture.lines().count(),
        wrong[..wrong.len().min(30)].join("\n")
    );
}
