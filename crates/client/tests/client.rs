//! The client against an in-process nucleus server.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use nucleus_client::{Client, Config, Error, Reply, TxnStatus};
use nucleus_kv::{Database, Kv, Options};
use nucleus_platform::sim::SimPlatform;
use nucleus_server::serve;

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
    let config = nucleus_server::Config {
        password: password.map(String::from),
        ..Default::default()
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
fn queries_commands_and_errors() {
    let mut c = Client::connect(config(start(None))).unwrap();
    assert!(c.parameter("server_version").unwrap().contains("nucleus"));
    let r = c
        .query("CREATE TABLE t (id INT PRIMARY KEY, name TEXT); INSERT INTO t VALUES (1, 'a'), (2, NULL)")
        .unwrap();
    assert_eq!(
        r,
        vec![
            Ok(Reply::Command("CREATE TABLE".into())),
            Ok(Reply::Command("INSERT 0 2".into()))
        ]
    );
    let r = c.query("SELECT id, name FROM t ORDER BY id").unwrap();
    let Ok(Reply::Rows { columns, rows, tag }) = &r[0] else {
        panic!("{r:?}")
    };
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "name"]
    );
    assert_eq!(columns[0].type_oid, 20);
    assert_eq!(
        rows,
        &vec![
            vec![Some("1".into()), Some("a".into())],
            vec![Some("2".into()), None]
        ]
    );
    assert_eq!(tag, "SELECT 2");
    // The first error ends the statements.
    let r = c.query("SELECT 1; SELECT * FROM nosuch; SELECT 2").unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].as_ref().unwrap_err().code, "42P01");
    assert_eq!(c.query("").unwrap(), vec![Ok(Reply::Empty)]);
    c.query("BEGIN").unwrap();
    assert_eq!(c.status(), TxnStatus::InBlock);
    c.query("SELECT 1/0").unwrap();
    assert_eq!(c.status(), TxnStatus::Failed);
    c.query("ROLLBACK").unwrap();
    assert_eq!(c.status(), TxnStatus::Idle);
    c.close().unwrap();
}

#[test]
fn passwords() {
    let addr = start(Some("pw"));
    match Client::connect(config(addr)) {
        Err(Error::Protocol(m)) => assert!(m.contains("password"), "{m}"),
        other => panic!("{other:?}"),
    }
    let wrong = Client::connect(Config {
        password: Some("nope".into()),
        ..config(addr)
    });
    assert!(matches!(wrong, Err(Error::Server(e)) if e.code == "28P01"));
    let mut c = Client::connect(Config {
        password: Some("pw".into()),
        ..config(addr)
    })
    .unwrap();
    assert!(c.query("SELECT 1").unwrap()[0].is_ok());
}
