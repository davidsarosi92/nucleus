//! The S3 repository against a stand-in object store (PLAN.md §M14.18
//! O3b): every request's signature checked as S3 checks it, listings in
//! pages, then backups — a full one, an incremental one — written there
//! and restored from there.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

use nucleus_s3::{S3, sign};
use nucleus_sql::ext::backups::{Target, Until, register_target, restore_to};
use nucleus_sql::ext::{Output, Session, Value};

const KEY_ID: &str = "test-key";
const SECRET: &str = "test-secret";

type Objects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// One request: answered, its signature checked first.
fn serve(mut c: TcpStream, objects: &Objects) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 65536];
    let head_end = loop {
        let n = c.read(&mut buf).unwrap();
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8(raw[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let (method, target) = (first.next().unwrap(), first.next().unwrap());
    let headers: Vec<(String, String)> = lines
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k.trim().to_ascii_lowercase(), v.trim().to_string())
        })
        .collect();
    let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let len: usize = header("content-length").map_or(0, |l| l.parse().unwrap());
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < len {
        let n = c.read(&mut buf).unwrap();
        body.extend_from_slice(&buf[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap();
            (decode(k), decode(v))
        })
        .collect();
    let q: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let signed: Vec<(&str, String)> = ["x-amz-content-sha256", "x-amz-date"]
        .into_iter()
        .map(|k| (k, header(k).unwrap()))
        .collect();
    let signed: Vec<(&str, &str)> = signed.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let payload = header("x-amz-content-sha256").unwrap();
    let want = sign(
        method,
        &header("host").unwrap(),
        path,
        &q,
        &signed,
        &payload,
        "us-east-1",
        KEY_ID,
        SECRET,
    );
    let hash: String = nucleus_sql::ext::sha256(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (status, out) =
        if header("authorization").as_deref() != Some(want.as_str()) || hash != payload {
            (
                403,
                b"<Error><Code>SignatureDoesNotMatch</Code></Error>".to_vec(),
            )
        } else {
            let key = decode(path.strip_prefix("/bkt/").unwrap_or(""));
            let mut o = objects.lock().unwrap();
            match method {
                "PUT" => {
                    o.insert(key, body);
                    (200, Vec::new())
                }
                "DELETE" => {
                    o.remove(&key);
                    (204, Vec::new())
                }
                "GET" if !key.is_empty() => match o.get(&key) {
                    Some(v) => (200, v.clone()),
                    None => (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                },
                _ => {
                    // A listing, two names a page.
                    let p = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                    let prefix = p("prefix").unwrap_or_default();
                    let after = p("continuation-token").unwrap_or_default();
                    let mut names: Vec<(String, bool)> = Vec::new();
                    for k in o.keys().filter(|k| k.starts_with(&prefix)) {
                        let rest = &k[prefix.len()..];
                        let e = match rest.find('/') {
                            Some(i) => (format!("{prefix}{}/", &rest[..i]), true),
                            None => (k.clone(), false),
                        };
                        if names.last() != Some(&e) {
                            names.push(e);
                        }
                    }
                    let page: Vec<_> = names.into_iter().filter(|(n, _)| *n > after).collect();
                    let more = page.len() > 2;
                    let mut xml = format!("<ListBucketResult><Prefix>{}</Prefix>", escape(&prefix));
                    for (n, dir) in page.iter().take(2) {
                        if *dir {
                            xml += &format!(
                                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                                escape(n)
                            );
                        } else {
                            xml += &format!("<Contents><Key>{}</Key></Contents>", escape(n));
                        }
                    }
                    xml += &format!("<IsTruncated>{more}</IsTruncated>");
                    if more {
                        xml += &format!(
                            "<NextContinuationToken>{}</NextContinuationToken>",
                            escape(&page[1].0)
                        );
                    }
                    xml += "</ListBucketResult>";
                    (200, xml.into_bytes())
                }
            }
        };
    let _ = write!(
        c,
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        out.len()
    );
    let _ = c.write_all(&out);
}

/// The stand-in store, started once: its address and objects.
fn store() -> &'static (String, Objects) {
    static STORE: OnceLock<(String, Objects)> = OnceLock::new();
    STORE.get_or_init(|| {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let objects: Objects = Arc::default();
        let o = Arc::clone(&objects);
        std::thread::spawn(move || {
            for c in l.incoming() {
                let o = Arc::clone(&o);
                std::thread::spawn(move || serve(c.unwrap(), &o));
            }
        });
        (addr, objects)
    })
}

fn env(k: &str) -> Option<String> {
    match k {
        "AWS_ACCESS_KEY_ID" => Some(KEY_ID.into()),
        "AWS_SECRET_ACCESS_KEY" => Some(SECRET.into()),
        "AWS_ENDPOINT_URL" => Some(store().0.clone()),
        _ => None,
    }
}

fn open(url: &str) -> Result<Box<dyn Target>, String> {
    S3::new(url, env).map(|s| Box::new(s) as Box<dyn Target>)
}

#[test]
fn objects_written_read_listed_and_deleted() {
    let s = S3::new("s3://bkt/files", env).unwrap();
    for n in [
        "000001-full/part-000000",
        "000001-full/manifest",
        "000002-incr/manifest",
        "a&b",
    ] {
        s.put(n, n.as_bytes()).unwrap();
    }
    assert_eq!(
        s.get("000001-full/manifest").unwrap().unwrap(),
        b"000001-full/manifest"
    );
    assert_eq!(s.get("nothing").unwrap(), None);
    // Pages of two: all of them, the "directories" once.
    assert_eq!(s.list("").unwrap(), ["000001-full", "000002-incr", "a&b"]);
    assert_eq!(s.list("000001-full").unwrap(), ["manifest", "part-000000"]);
    assert_eq!(s.list("none").unwrap(), Vec::<String>::new());
    s.delete("a&b").unwrap();
    s.delete("a&b").unwrap();
    assert_eq!(s.list("").unwrap(), ["000001-full", "000002-incr"]);
    // A wrong secret: refused, said so.
    let bad = S3::new("s3://bkt/files", |k| match k {
        "AWS_SECRET_ACCESS_KEY" => Some("wrong".into()),
        _ => env(k),
    })
    .unwrap();
    let e = bad.get("000001-full/manifest").unwrap_err().to_string();
    assert!(e.contains("403 SignatureDoesNotMatch"), "{e}");
}

fn rows(s: &mut Session, sql: &str) -> Vec<String> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(Value::render).collect::<Vec<_>>().join(" "))
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn backups_kept_in_the_store_restore() {
    register_target("s3", open);
    let open_db = || -> Arc<dyn nucleus_kv::Kv> {
        Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
    };
    let db = open_db();
    let mut s = Session::new(Arc::clone(&db));
    s.execute(
        "CREATE TABLE t (id int PRIMARY KEY, v text); CREATE INDEX t_v ON t (v); \
         INSERT INTO t SELECT g, 'v' || g FROM generate_series(1, 5000) g",
    )
    .unwrap();
    let first = rows(&mut s, "BACKUP TO 's3://bkt/db'");
    assert!(first[0].ends_with("full 000001-full 0"), "{first:?}");
    let want_first = rows(&mut s, "SELECT count(*), sum(id) FROM t");
    s.execute("DELETE FROM t WHERE id > 4000; UPDATE t SET v = 'x' WHERE id < 10")
        .unwrap();
    let second = rows(&mut s, "BACKUP TO 's3://bkt/db'");
    assert!(
        second[0].ends_with("incremental 000002-incr 0"),
        "{second:?}"
    );
    let want = rows(
        &mut s,
        "SELECT count(*), sum(id), sum(CASE WHEN v = 'x' THEN 1 ELSE 0 END) FROM t",
    );
    let restored = open_db();
    let r = restore_to(&restored, s.registry(), "s3://bkt/db", &Until::Latest).unwrap();
    assert_eq!((r.backups, r.deleted), (2, 1000), "{r:?}");
    let mut t = Session::new(restored);
    assert_eq!(
        rows(
            &mut t,
            "SELECT count(*), sum(id), sum(CASE WHEN v = 'x' THEN 1 ELSE 0 END) FROM t"
        ),
        want
    );
    let earlier = open_db();
    restore_to(
        &earlier,
        s.registry(),
        "s3://bkt/db",
        &Until::Name("000001-full".into()),
    )
    .unwrap();
    let mut u = Session::new(earlier);
    assert_eq!(rows(&mut u, "SELECT count(*), sum(id) FROM t"), want_first);
}
