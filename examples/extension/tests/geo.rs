//! The sample module: through SQL, its laws, and over the nucleus protocol
//! of a custom build.

use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;

use nucleus_example_extension::{Geo, geopoint, point};
use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_server::{Config, Ncp, serve_front_end};
use nucleus_sql::ext::{DataType, Output, Registry, Session, Value};

fn registry() -> Arc<Registry> {
    let mut r = Registry::builtin();
    r.add(&Geo).unwrap();
    Arc::new(r)
}

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match s
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .pop()
    {
        Some(Output::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(Value::render).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_type_a_function_and_a_view() {
    let mut s = Session::with_registry(db(), registry());
    s.execute("CREATE TABLE shops (id INT PRIMARY KEY, at geopoint)")
        .unwrap();
    s.execute("INSERT INTO shops VALUES (1, '(47.4979,19.0402)'), (2, '(48.2082,16.3738)')")
        .unwrap();
    assert_eq!(
        rows(
            &mut s,
            "SELECT id FROM shops ORDER BY at <-> '(48.2,16.37)'"
        ),
        [["2"], ["1"]]
    );
    assert_eq!(
        rows(&mut s, "SELECT geohash(at) FROM shops WHERE id = 1"),
        [["u2mw1q8x"]]
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT name FROM geo.cities WHERE at <-> '(0,-78)' < 100"
        ),
        [["Quito"]]
    );
    assert_eq!(
        s.execute("SELECT * FROM shops ORDER BY at")
            .unwrap_err()
            .code,
        "42883"
    );
    assert_eq!(
        rows(
            &mut s,
            "SELECT module FROM nc_catalog.types WHERE name = 'geopoint'"
        ),
        [["geo"]]
    );
}

#[test]
fn it_keeps_the_laws() {
    let _ = registry();
    let mut generate = |g: &mut nucleus_testkit::laws::Gen| {
        let lat = (g.below(18001) as f64 - 9000.0) / 100.0;
        let lon = (g.below(36001) as f64 - 18000.0) / 100.0;
        point(lat, lon)
    };
    let problems = nucleus_testkit::laws::check(DataType::Ext(geopoint()), &mut generate, 2000);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_custom_build_serves_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let kv = db();
    let config = Config::trust().with_registry(registry());
    std::thread::spawn(move || serve_front_end(Arc::new(Ncp), listener, kv, config));
    // A plain protocol client (version 1 gets registered types as text).
    use std::io::{Read, Write};
    let mut sock = std::net::TcpStream::connect(addr).unwrap();
    let frame = |kind: u8, body: &[u8]| {
        let mut f = vec![kind];
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend_from_slice(body);
        f
    };
    let string = |b: &mut Vec<u8>, s: &str| {
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    };
    let mut hello = 1u16.to_le_bytes().to_vec();
    for part in ["nucleus", "", "", "test"] {
        string(&mut hello, part);
    }
    sock.write_all(&frame(b'H', &hello)).unwrap();
    let mut q = Vec::new();
    string(&mut q, "SELECT geohash('(47.4979,19.0402)')");
    q.push(0);
    q.extend_from_slice(&0u16.to_le_bytes());
    sock.write_all(&frame(b'Q', &q)).unwrap();
    let mut readies = 0;
    let mut row = None;
    while readies < 2 {
        let mut head = [0u8; 5];
        sock.read_exact(&mut head).unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(head[1..].try_into().unwrap()) as usize];
        sock.read_exact(&mut body).unwrap();
        match head[0] {
            b'R' => readies += 1,
            b'D' => row = Some(body),
            b'E' => panic!("{}", String::from_utf8_lossy(&body)),
            _ => {}
        }
    }
    assert_eq!(&row.unwrap()[5..], b"u2mw1q8x");
}
