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
use nucleus_sql::slt::{Cell, Connection, SqlError, run_script};

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
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sql/tests/slt");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|f| f.extension().is_some_and(|e| e == "slt"))
        .collect();
    files.sort();
    let mut failures = Vec::new();
    for f in &files {
        let addr = start(None);
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let script = std::fs::read_to_string(f).unwrap();
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Ncp(Client::connect(config(addr)).unwrap())) };
        failures.extend(run_script(&script, &name, &mut connect));
    }
    assert!(files.len() >= 19);
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
