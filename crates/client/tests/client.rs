//! The Rust driver against an in-process nucleus server: types both ways,
//! parameters, errors, transactions, and the slt suite over the nucleus
//! protocol (PLAN.md §M14.7 "done when").

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use nucleus_client::{Client, Config, Error, Format, TxnStatus, Type, Value};
use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_server::serve;
use nucleus_testkit::slt::{Cell, Connection, SqlError, run_script};

fn start(password: Option<&str>) -> SocketAddr {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = match password {
        Some(pw) => nucleus_server::Config::password(nucleus_server::DEFAULT_USER, pw),
        // The defaults on both sides: user and password `nucleus`.
        None => nucleus_server::Config::default(),
    };
    std::thread::spawn(move || serve(listener, db, config));
    addr
}

fn config(addr: SocketAddr) -> Config {
    Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ..Config::default()
    }
}

#[test]
fn typed_values_both_ways() {
    let mut c = Client::connect(config(start(None))).unwrap();
    assert!(c.server_version().starts_with("nucleus "));
    c.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, n NUMERIC(10, 2), d DATE, ts TIMESTAMPTZ, iv INTERVAL, tags TEXT[], ok BOOLEAN, f FLOAT)",
        &[],
    )
    .unwrap();
    let params = [
        Value::Int(1),
        Value::Numeric {
            mantissa: 12345,
            scale: 2,
        },
        Value::Date(8766), // 2024-01-01
        Value::TimestampTz(757_382_400_000_000),
        Value::Interval {
            months: 1,
            days: 2,
            micros: 3_000_000,
        },
        Value::Array(vec![Value::from("a"), Value::Null]),
        Value::Bool(true),
        Value::Float(0.5),
    ];
    let r = c
        .execute(
            "INSERT INTO t VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &params,
        )
        .unwrap();
    assert_eq!((r.command.as_str(), r.count), ("INSERT", 1));
    let r = c.execute("SELECT * FROM t", &[]).unwrap();
    assert_eq!(r.columns[1].ty, Type::Numeric);
    assert_eq!(r.columns[5].ty, Type::Array(Box::new(Type::Text)));
    assert_eq!(r.rows, vec![params.to_vec()]);
    assert_eq!(r.rows[0][2].to_string(), "2024-01-01");
    assert_eq!(r.rows[0][1].to_string(), "123.45");
    // Text parameters are parsed where a type is needed.
    let rows = c
        .rows(
            "SELECT d + 1, n * 2 FROM t WHERE d = $1",
            &[Value::from("2024-01-01")],
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Date(8767),
            Value::Numeric {
                mantissa: 24690,
                scale: 2
            }
        ]]
    );
    // Text results.
    let r = c.query("SELECT ts, iv FROM t", &[], Format::Text).unwrap();
    let rows = &r[0].as_ref().unwrap().rows;
    assert_eq!(
        rows[0],
        vec![
            Value::from("2024-01-01 00:00:00+00"),
            Value::from("1 mon 2 days 00:00:03")
        ]
    );
}

#[test]
fn errors_statements_and_transactions() {
    let mut c = Client::connect(config(start(None))).unwrap();
    let r = c
        .query(
            "SELECT 1; SELECT * FROM nosuch; SELECT 2",
            &[],
            Format::Typed,
        )
        .unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].as_ref().unwrap_err().code, "42P01");
    match c.execute("SELECT 1/0", &[]) {
        Err(Error::Server(e)) => assert_eq!(e.code, "22012"),
        other => panic!("{other:?}"),
    }
    assert!(c.execute("SELECT $1; SELECT 2", &[Value::Int(1)]).is_err());
    c.execute("BEGIN", &[]).unwrap();
    assert_eq!(c.status(), TxnStatus::InBlock);
    let _ = c.execute("SELECT 1/0", &[]);
    assert_eq!(c.status(), TxnStatus::Failed);
    c.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(c.status(), TxnStatus::Idle);
    c.close().unwrap();
}

#[test]
fn passwords() {
    let addr = start(Some("pw"));
    assert!(matches!(Client::connect(config(addr)), Err(Error::Server(e)) if e.code == "28P01"));
    let wrong_user = Client::connect(Config {
        user: "someone".into(),
        password: Some("pw".into()),
        ..config(addr)
    });
    assert!(matches!(wrong_user, Err(Error::Server(e)) if e.code == "28P01"));
    let mut c = Client::connect(Config {
        password: Some("pw".into()),
        ..config(addr)
    })
    .unwrap();
    assert_eq!(c.rows("SELECT 1", &[]).unwrap(), vec![vec![Value::Int(1)]]);
}

/// The slt connection over the nucleus protocol (text results, as the
/// scripts compare text).
struct Ncp(Client);

impl Connection for Ncp {
    fn run(&mut self, sql: &str) -> Result<Option<Vec<Vec<Cell>>>, SqlError> {
        let mut replies = self.0.query(sql, &[], Format::Text).expect("connection");
        match replies.pop() {
            Some(Ok(r)) if r.has_rows => Ok(Some(
                r.rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|v| match v {
                                Value::Null => None,
                                Value::Text(s) => Some(s),
                                v => Some(v.to_string()),
                            })
                            .collect()
                    })
                    .collect(),
            )),
            Some(Ok(_)) | None => Ok(None),
            Some(Err(e)) => Err(SqlError {
                code: e.code,
                message: e.message,
            }),
        }
    }
}

#[test]
fn sql_scripts_over_the_nucleus_protocol() {
    let mut failures = Vec::new();
    let scripts = nucleus_testkit::core_scripts();
    for (name, script) in scripts.iter().filter(|(n, _)| nucleus_testkit::picked(n)) {
        let addr = start(None);
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Ncp(Client::connect(config(addr)).unwrap())) };
        failures.extend(run_script(script, name, &mut connect));
    }
    assert!(scripts.len() >= 19);
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn uuid_both_ways() {
    let mut c = Client::connect(config(start(None))).unwrap();
    c.execute("CREATE TABLE u (id UUID PRIMARY KEY, n INT)", &[])
        .unwrap();
    let id = [
        0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a,
        0x11,
    ];
    c.execute("INSERT INTO u VALUES ($1, 1)", &[Value::Uuid(id)])
        .unwrap();
    let r = c
        .execute("SELECT id, id::text, gen_random_uuid() FROM u", &[])
        .unwrap();
    assert_eq!(r.columns[0].ty, Type::Uuid);
    assert_eq!(r.rows[0][0], Value::Uuid(id));
    assert_eq!(
        r.rows[0][1],
        Value::from("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")
    );
    assert_eq!(
        r.rows[0][0].to_string(),
        "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
    );
    assert!(matches!(r.rows[0][2], Value::Uuid(b) if b[6] >> 4 == 4));
    // A text parameter is parsed where a uuid is needed.
    let r = c
        .execute(
            "SELECT n FROM u WHERE id = $1",
            &[Value::from("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11")],
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    // A registered type the server does not know is refused.
    let err = c
        .execute(
            "SELECT $1::text",
            &[Value::Other {
                type_name: "nosuch".into(),
                bytes: vec![1],
            }],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Server(e) if e.code == "42704"));
}

/// A version-1 client (before registered types) gets them as text.
#[test]
fn version_1_clients_get_registered_types_as_text() {
    use std::io::{Read, Write};
    let addr = start(None);
    let mut s = std::net::TcpStream::connect(addr).unwrap();
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
    for part in ["nucleus", "", "nucleus", "test"] {
        string(&mut hello, part);
    }
    s.write_all(&frame(b'H', &hello)).unwrap();
    let mut query = Vec::new();
    string(
        &mut query,
        "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid",
    );
    query.push(0); // typed
    query.extend_from_slice(&0u16.to_le_bytes());
    s.write_all(&frame(b'Q', &query)).unwrap();
    // Read until Ready after the query; collect frames.
    let mut frames = Vec::new();
    let mut readies = 0;
    while readies < 2 {
        let mut head = [0u8; 5];
        s.read_exact(&mut head).unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(head[1..].try_into().unwrap()) as usize];
        s.read_exact(&mut body).unwrap();
        if head[0] == b'R' {
            readies += 1;
        }
        frames.push((head[0], body));
    }
    let welcome = &frames.iter().find(|f| f.0 == b'W').unwrap().1;
    assert_eq!(u16::from_le_bytes([welcome[0], welcome[1]]), 1);
    let columns = &frames.iter().find(|f| f.0 == b'C').unwrap().1;
    // u16 count, the string name, then the type tag: text.
    let name_len = u32::from_le_bytes(columns[2..6].try_into().unwrap()) as usize;
    assert_eq!(columns[6 + name_len], 4);
    let row = &frames.iter().find(|f| f.0 == b'D').unwrap().1;
    assert_eq!(row[0], 4);
    assert_eq!(&row[5..], b"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
}
