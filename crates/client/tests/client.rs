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
        Value::Integer(1),
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
    assert_eq!(
        c.rows("SELECT 1", &[]).unwrap(),
        vec![vec![Value::Integer(1)]]
    );
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
    assert_eq!(r.rows, vec![vec![Value::Integer(1)]]);
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

/// The types of §M14.26, in their native forms (protocol version 3):
/// read back, and sent as parameters.
#[test]
fn core_types_both_ways() {
    use std::net::IpAddr;
    let mut c = Client::connect(config(start(None))).unwrap();
    c.execute("CREATE TYPE mood AS ENUM ('sad', 'happy')", &[])
        .unwrap();
    c.execute(
        "CREATE TABLE k (s SMALLINT, i INTEGER, b BIGINT, r REAL, bin BYTEA, t TIME, \
         ch CHAR(3), ip INET, net CIDR, m mood)",
        &[],
    )
    .unwrap();
    let ip: IpAddr = "10.1.2.3".parse().unwrap();
    let v6: IpAddr = "2001:db8::".parse().unwrap();
    let params = [
        Value::SmallInt(-2),
        Value::Integer(70_000),
        Value::Int(1 << 40),
        Value::Real(0.1),
        Value::Bytes(vec![0, 0xff, 7]),
        Value::Time(45_296_500_000), // 12:34:56.5
        Value::from("ab"),
        Value::Inet { addr: ip, bits: 8 },
        Value::Cidr { addr: v6, bits: 32 },
        Value::from("happy"),
    ];
    c.execute(
        "INSERT INTO k VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        &params,
    )
    .unwrap();
    let r = c.execute("SELECT * FROM k", &[]).unwrap();
    let types: Vec<Type> = r.columns.iter().map(|c| c.ty.clone()).collect();
    assert_eq!(
        types,
        [
            Type::SmallInt,
            Type::Integer,
            Type::Int,
            Type::Real,
            Type::Bytea,
            Type::Time,
            Type::Char,
            Type::Inet,
            Type::Cidr,
            Type::Enum("mood".into()),
        ]
    );
    let mut want = params.to_vec();
    want[6] = Value::from("ab "); // padded
    assert_eq!(r.rows, vec![want]);
    let shown: Vec<String> = r.rows[0].iter().map(|v| v.to_string()).collect();
    assert_eq!(
        shown,
        [
            "-2",
            "70000",
            "1099511627776",
            "0.1",
            "\\x00ff07",
            "12:34:56.5",
            "ab ",
            "10.1.2.3/8",
            "2001:db8::/32",
            "happy"
        ]
    );
    // Text parameters are parsed where these types are needed; arrays.
    let r = c
        .execute(
            "SELECT count(*) FROM k WHERE t = $1 AND ip <<= $2 AND bin = $3 AND m = $4",
            &[
                Value::from("12:34:56.5"),
                Value::from("10.0.0.0/8"),
                Value::from("\\x00ff07"),
                Value::from("happy"),
            ],
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    let r = c
        .execute(
            "SELECT ARRAY[1::smallint, 2], ARRAY[$1::real]",
            &[Value::Real(1.5)],
        )
        .unwrap();
    assert_eq!(
        r.rows[0],
        [
            Value::Array(vec![Value::Integer(1), Value::Integer(2)]),
            Value::Array(vec![Value::Real(1.5)])
        ]
    );
    // A cidr parameter with host bits is refused.
    let err = c
        .execute("SELECT $1::cidr", &[Value::Cidr { addr: ip, bits: 8 }])
        .unwrap_err();
    assert!(matches!(err, Error::Server(e) if e.code == "22P02"));
}

/// One typed query by hand at protocol `version`: the version the server
/// welcomed with, and the bodies of the Columns and the first Row.
fn raw_query(version: u16, sql: &str) -> (u16, Vec<u8>, Vec<u8>) {
    let frames = raw_frames(version, sql);
    let find = |k: u8| frames.iter().find(|f| f.0 == k).unwrap().1.clone();
    let welcome = find(b'W');
    (
        u16::from_le_bytes([welcome[0], welcome[1]]),
        find(b'C'),
        find(b'D'),
    )
}

/// The frames a client of `version` (below 4: the password in Hello) gets
/// for `sql`, through Ready after it.
fn raw_frames(version: u16, sql: &str) -> Vec<(u8, Vec<u8>)> {
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
    let mut hello = version.to_le_bytes().to_vec();
    for part in ["nucleus", "", "nucleus", "test"] {
        string(&mut hello, part);
    }
    s.write_all(&frame(b'H', &hello)).unwrap();
    let mut query = Vec::new();
    string(&mut query, sql);
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
    frames
}

/// Errors carry a hint from protocol version 5 (PLAN.md §M14.10); an older
/// client's `E` is as it was: SQLSTATE and message only.
#[test]
fn hints_from_version_5() {
    let mut c = Client::connect(config(start(None))).unwrap();
    c.execute(
        "CREATE TABLE customers (id INT PRIMARY KEY, email TEXT)",
        &[],
    )
    .unwrap();
    let hint = |c: &mut Client, sql: &str| match c.execute(sql, &[]) {
        Err(Error::Server(e)) => (e.code, e.hint),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        hint(&mut c, "SELECT emial FROM customers"),
        (
            "42703".into(),
            Some("Perhaps you meant column \"email\".".into())
        )
    );
    assert_eq!(
        hint(&mut c, "SELECT * FROM custmers"),
        (
            "42P01".into(),
            Some("Perhaps you meant table \"customers\".".into())
        )
    );
    assert_eq!(
        hint(&mut c, "SELECT * FROM customers WHERE id = 'x'::text").1,
        Some("Compare values of one type: cast one side, as CAST(… AS integer) or …::text.".into())
    );
    // No hint is none, not an empty one.
    assert_eq!(hint(&mut c, "SELECT 'abc'::integer").1, None);
    c.close().unwrap();
    let frames = raw_frames(3, "SELECT 1/0");
    let e = &frames.iter().find(|f| f.0 == b'E').unwrap().1;
    let code_len = u32::from_le_bytes(e[0..4].try_into().unwrap()) as usize;
    let at = 4 + code_len;
    let message_len = u32::from_le_bytes(e[at..at + 4].try_into().unwrap()) as usize;
    assert_eq!(e.len(), at + 4 + message_len, "no hint before version 5");
}

/// A version-1 client (before registered types) gets them as text.
#[test]
fn version_1_clients_get_registered_types_as_text() {
    let (version, columns, row) =
        raw_query(1, "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid");
    assert_eq!(version, 1);
    // u16 count, the string name, then the type tag: text.
    let name_len = u32::from_le_bytes(columns[2..6].try_into().unwrap()) as usize;
    assert_eq!(columns[6 + name_len], 4);
    assert_eq!(row[0], 4);
    assert_eq!(&row[5..], b"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
}

/// A version-2 client (before §M14.26's tags) gets a smallint as a bigint
/// and a time as text.
#[test]
fn version_2_clients_get_the_new_types_as_before() {
    let (version, columns, row) = raw_query(2, "SELECT 7::smallint AS a, time '10:00' AS b");
    assert_eq!(version, 2);
    // Two columns named "a" and "b" (u32 length 1 each), their tags.
    assert_eq!((columns[2 + 4 + 1], columns[2 + 4 + 1 + 1 + 4 + 1]), (2, 4));
    assert_eq!(row[0], 2);
    assert_eq!(i64::from_le_bytes(row[1..9].try_into().unwrap()), 7);
    assert_eq!(row[9], 4);
    assert_eq!(&row[14..], b"10:00:00");
}

/// Version 4 proves the password (SCRAM-SHA-256) and never sends it: a
/// password in Hello counts for nothing; the server's first answer is the
/// mechanism, and a client that does not go through with it is not let in.
#[test]
fn version_4_never_takes_a_password_in_hello() {
    use std::io::{Read, Write};
    let addr = start(None);
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let string = |b: &mut Vec<u8>, s: &str| {
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    };
    let mut hello = 4u16.to_le_bytes().to_vec();
    for part in ["nucleus", "", "nucleus", "test"] {
        string(&mut hello, part);
    }
    let mut frame = vec![b'H'];
    frame.extend_from_slice(&(hello.len() as u32).to_le_bytes());
    frame.extend_from_slice(&hello);
    s.write_all(&frame).unwrap();
    let mut head = [0u8; 5];
    s.read_exact(&mut head).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(head[1..].try_into().unwrap()) as usize];
    s.read_exact(&mut body).unwrap();
    assert_eq!(head[0], b'A');
    assert_eq!(&body[4..], b"SCRAM-SHA-256");
    // Skipping the exchange (a query instead of client-first) fails.
    let mut query = Vec::new();
    string(&mut query, "SELECT 1");
    query.push(0);
    query.extend_from_slice(&0u16.to_le_bytes());
    let mut frame = vec![b'Q'];
    frame.extend_from_slice(&(query.len() as u32).to_le_bytes());
    frame.extend_from_slice(&query);
    s.write_all(&frame).unwrap();
    s.read_exact(&mut head).unwrap();
    assert_eq!(head[0], b'E');
}

/// Logins (PLAN.md §M14.25): an unclaimed database takes the server's
/// credentials; once it has an administrator with a password, its roles
/// decide.
#[test]
fn roles_decide_logins() {
    let addr = start(None);
    let as_user = |user: &str, password: &str| {
        Client::connect(Config {
            user: user.into(),
            password: Some(password.into()),
            ..config(addr)
        })
    };
    let code = |r: Result<Client, Error>| match r {
        Err(Error::Server(e)) => e.code,
        Err(e) => format!("{e}"),
        Ok(_) => "ok".into(),
    };
    // Unclaimed: the defaults (nucleus / nucleus) are the administrator.
    let mut admin0 = as_user("nucleus", "nucleus").unwrap();
    admin0
        .execute("CREATE USER app PASSWORD 'app-pw'", &[])
        .unwrap();
    // Still unclaimed (no administrator with a password): app's password
    // does not count yet, the defaults still do.
    assert_eq!(code(as_user("app", "app-pw")), "28P01");
    admin0
        .execute("CREATE USER root SUPERUSER PASSWORD 'root-pw'", &[])
        .unwrap();
    // Claimed: the catalog decides.
    assert_eq!(code(as_user("nucleus", "nucleus")), "28P01");
    assert_eq!(code(as_user("root", "wrong")), "28P01");
    let mut root = as_user("root", "root-pw").unwrap();
    let mut app = as_user("app", "app-pw").unwrap();
    assert_eq!(
        app.rows("SELECT current_user, session_user", &[]).unwrap(),
        vec![vec![Value::from("app"), Value::from("app")]]
    );
    let err = app.execute("CREATE ROLE x", &[]).unwrap_err();
    assert!(matches!(err, Error::Server(e) if e.code == "42501"));
    // A role that may not log in, and an expired password.
    root.execute("ALTER ROLE app NOLOGIN", &[]).unwrap();
    assert_eq!(code(as_user("app", "app-pw")), "28P01");
    root.execute(
        "ALTER ROLE app LOGIN VALID UNTIL '2001-01-01 00:00:00+00'",
        &[],
    )
    .unwrap();
    assert_eq!(code(as_user("app", "app-pw")), "28P01");
    root.execute("ALTER ROLE app VALID UNTIL 'infinity'", &[])
        .unwrap();
    assert!(as_user("app", "app-pw").is_ok());
    // CONNECT on the database: PUBLIC's until revoked.
    root.execute("REVOKE CONNECT ON DATABASE nucleus FROM PUBLIC", &[])
        .unwrap();
    assert_eq!(code(as_user("app", "app-pw")), "42501");
    root.execute("GRANT CONNECT ON DATABASE nucleus TO app", &[])
        .unwrap();
    assert!(as_user("app", "app-pw").is_ok());
    let err = root
        .execute("GRANT CONNECT ON DATABASE elsewhere TO app", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Server(e) if e.code == "3D000"));
}

/// Tokens (PLAN.md §M14.25): a login name and a secret, proved by SCRAM as
/// any password; the session acts as the token's role under its ceiling.
#[test]
fn tokens_log_in_under_their_ceiling() {
    let addr = start(None);
    let login = |user: &str, password: &str| {
        Client::connect(Config {
            user: user.into(),
            password: Some(password.into()),
            ..config(addr)
        })
    };
    fn code<T>(r: Result<T, Error>) -> String {
        match r {
            Err(Error::Server(e)) => e.code,
            Err(e) => format!("{e}"),
            Ok(_) => "ok".into(),
        }
    }
    let mut admin = login("nucleus", "nucleus").unwrap();
    for sql in [
        "CREATE USER root SUPERUSER PASSWORD 'root-pw'",
        "CREATE TABLE notes (id int PRIMARY KEY, body text)",
        "CREATE TABLE secrets (x int)",
        "INSERT INTO notes VALUES (1, 'a'), (2, 'b')",
        "INSERT INTO secrets VALUES (42)",
        "CREATE ROLE agent",
        "GRANT SELECT, INSERT ON notes TO agent",
        "GRANT SELECT ON secrets TO agent",
        "CREATE ROLE helper",
        "GRANT SELECT ON secrets TO helper",
        "GRANT INSERT ON notes TO helper",
        "GRANT helper TO agent",
    ] {
        admin.execute(sql, &[]).unwrap();
    }
    let token = |c: &mut Client, sql: &str| -> (String, String) {
        let r = c.rows(sql, &[]).unwrap();
        match (&r[0][1], &r[0][2]) {
            (Value::Text(l), Value::Text(s)) => (l.clone(), s.clone()),
            other => panic!("{other:?}"),
        }
    };
    let (rl, rs) = token(
        &mut admin,
        "CREATE TOKEN reader FOR agent READ ONLY ON TABLE notes",
    );
    let (wl, ws) = token(&mut admin, "CREATE TOKEN writer FOR agent");
    // A token acts as its role.
    let mut r = login(&rl, &rs).unwrap();
    assert_eq!(
        r.rows("SELECT current_user, count(*) FROM notes", &[])
            .unwrap(),
        vec![vec![Value::from("agent"), Value::Int(2)]]
    );
    // Its ceiling: read only, one table — through SET ROLE too.
    assert_eq!(
        code(r.execute("INSERT INTO notes VALUES (3, 'c')", &[])),
        "25006"
    );
    assert_eq!(code(r.execute("SELECT * FROM secrets", &[])), "42501");
    assert_eq!(code(r.execute("CREATE TABLE mine (x int)", &[])), "42501");
    r.execute("SET ROLE helper", &[]).unwrap();
    assert_eq!(code(r.execute("SELECT * FROM secrets", &[])), "42501");
    assert_eq!(
        code(r.execute("INSERT INTO notes VALUES (3, 'c')", &[])),
        "25006"
    );
    r.execute("RESET ROLE", &[]).unwrap();
    // No token can make tokens or set passwords.
    let mut w = login(&wl, &ws).unwrap();
    w.execute("INSERT INTO notes VALUES (3, 'c')", &[]).unwrap();
    assert_eq!(code(w.execute("SELECT x FROM secrets", &[])), "ok");
    assert_eq!(code(w.execute("CREATE TOKEN more", &[])), "42501");
    assert_eq!(
        code(w.execute("ALTER ROLE agent PASSWORD 'mine'", &[])),
        "42501"
    );
    // Wrong secrets, revoked and expired tokens do not log in.
    assert!(login(&rl, "wrong").is_err());
    assert!(login(&rl, &ws).is_err());
    assert!(login("nct_0000000000000000", &rs).is_err());
    admin.execute("DROP TOKEN reader", &[]).unwrap();
    assert!(login(&rl, &rs).is_err());
    let (el, es) = token(
        &mut admin,
        "CREATE TOKEN old FOR agent VALID UNTIL '2001-01-01 00:00:00+00'",
    );
    assert!(login(&el, &es).is_err());
    // Claiming the database does not touch tokens; dropping the role does.
    let mut root = login("root", "root-pw").unwrap();
    assert!(login(&wl, &ws).is_ok());
    root.execute("REVOKE ALL ON notes, secrets FROM agent, helper", &[])
        .unwrap();
    root.execute("DROP ROLE agent", &[]).unwrap();
    assert!(login(&wl, &ws).is_err());
}

/// A proxy for tests: each connection to it reaches `server` with a PROXY
/// protocol header naming `source` as the client (version 1 text, or
/// version 2 binary for an IPv4 source when `v2`).
fn proxy(server: SocketAddr, source: &str, v2: bool) -> SocketAddr {
    use std::io::{Read, Write};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let source: std::net::IpAddr = source.parse().unwrap();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let mut client = client.unwrap();
            let mut upstream = std::net::TcpStream::connect(server).unwrap();
            match (source, v2) {
                (std::net::IpAddr::V4(a), true) => {
                    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
                    h.extend([0x21, 0x11, 0, 12]);
                    h.extend(a.octets());
                    h.extend([127, 0, 0, 1, 0x30, 0x39, 0x4e, 0x23]);
                    upstream.write_all(&h).unwrap();
                }
                (a, _) => {
                    let family = if a.is_ipv4() { "TCP4" } else { "TCP6" };
                    let dst = if a.is_ipv4() { "127.0.0.1" } else { "::1" };
                    write!(upstream, "PROXY {family} {a} {dst} 12345 20035\r\n").unwrap();
                }
            }
            let (mut c2, mut u2) = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut u2, &mut c2);
                let _ = c2.shutdown(std::net::Shutdown::Both);
            });
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match client.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if upstream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = upstream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    addr
}

/// Access rules over real sockets (PLAN.md §M14.25 "done when"): the same
/// user with full rights from one address and read only from another,
/// refused from a third; a role held only under its condition; a ceiling
/// of another role's rights; what the session sees of its connection.
#[test]
fn access_rules_decide_by_where_and_how() {
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.local_addr().unwrap();
    let server_config = nucleus_server::Config {
        trusted_proxies: vec!["127.0.0.1/32".into()],
        ..Default::default()
    };
    std::thread::spawn(move || serve(listener, db, server_config));
    let from = |source: &str| proxy(server, source, false);
    let login = |via: SocketAddr, user: &str, password: &str| {
        Client::connect(Config {
            user: user.into(),
            password: Some(password.into()),
            ..config(via)
        })
    };
    fn code<T>(r: Result<T, Error>) -> String {
        match r {
            Err(Error::Server(e)) => e.code,
            Err(e) => format!("{e}"),
            Ok(_) => "ok".into(),
        }
    }
    let admin_via = from("10.0.0.1");
    login(admin_via, "nucleus", "nucleus")
        .unwrap()
        .execute("CREATE USER root SUPERUSER PASSWORD 'root-pw'", &[])
        .unwrap();
    let mut admin = login(admin_via, "root", "root-pw").unwrap();
    for sql in [
        "CREATE USER alice PASSWORD 'alice-pw'",
        "CREATE USER bob PASSWORD 'bob-pw'",
        "CREATE USER carol PASSWORD 'carol-pw'",
        "CREATE ROLE ops",
        "CREATE ROLE readers",
        "CREATE TABLE notes (id int)",
        "CREATE TABLE secret (x int)",
        "INSERT INTO secret VALUES (42)",
        "GRANT SELECT, INSERT ON notes TO alice, bob",
        "GRANT SELECT ON notes TO readers",
        "GRANT SELECT ON secret TO ops",
        "GRANT ops TO alice WHEN (source <<= '10.2.0.0/16' AND auth = 'password')",
        "CREATE ACCESS RULE admins FOR root FROM '10.0.0.0/8' ALLOW",
        "CREATE ACCESS RULE alice_desk FOR alice FROM '10.2.3.1/32' ALLOW",
        "CREATE ACCESS RULE alice_kiosk FOR alice FROM '10.2.3.2/32' ALLOW READ ONLY",
        "CREATE ACCESS RULE alice_home FOR alice FROM '192.168.0.0/16' ALLOW",
        "CREATE ACCESS RULE bob_lab FOR bob FROM '10.3.0.0/16' ALLOW AS ROLE readers",
        "CREATE ACCESS RULE no_carol FOR carol REJECT BEFORE admins",
    ] {
        admin.execute(sql, &[]).unwrap();
    }
    // A bad condition, method or address is refused when made.
    assert_eq!(
        code(admin.execute("GRANT ops TO bob WHEN (nope = 1)", &[])),
        "42703"
    );
    assert_eq!(
        code(admin.execute("GRANT ops TO bob WHEN (auth)", &[])),
        "42804"
    );
    assert_eq!(
        code(admin.execute("CREATE ACCESS RULE x AUTH magic ALLOW", &[])),
        "22023"
    );
    assert_eq!(
        code(admin.execute("CREATE ACCESS RULE x FROM 'nope' ALLOW", &[])),
        "22P02"
    );
    // From the desk: everything alice has, and ops (its condition holds).
    let mut desk = login(from("10.2.3.1"), "alice", "alice-pw").unwrap();
    desk.execute("INSERT INTO notes VALUES (1)", &[]).unwrap();
    assert_eq!(
        desk.rows(
            "SELECT x, current_setting('nucleus.client_addr'), current_setting('nucleus.access_rule') FROM secret",
            &[]
        )
        .unwrap(),
        vec![vec![Value::Integer(42), Value::from("10.2.3.1"), Value::from("alice_desk")]]
    );
    assert_eq!(
        code(desk.execute("SET nucleus.client_addr = '1.2.3.4'", &[])),
        "55P02"
    );
    // From the kiosk: read only.
    let mut kiosk = login(from("10.2.3.2"), "alice", "alice-pw").unwrap();
    assert_eq!(code(kiosk.execute("SELECT count(*) FROM notes", &[])), "ok");
    assert_eq!(
        code(kiosk.execute("INSERT INTO notes VALUES (2)", &[])),
        "25006"
    );
    // From home: alice, but not ops (its condition does not hold).
    let mut home = login(from("192.168.7.7"), "alice", "alice-pw").unwrap();
    home.execute("INSERT INTO notes VALUES (3)", &[]).unwrap();
    assert_eq!(code(home.execute("SELECT x FROM secret", &[])), "42501");
    assert_eq!(code(home.execute("SET ROLE ops", &[])), "42501");
    // From anywhere else: no rule admits her; carol never.
    assert_eq!(
        code(login(from("172.16.0.1"), "alice", "alice-pw")),
        "28000"
    );
    assert_eq!(code(login(from("10.0.0.5"), "carol", "carol-pw")), "28000");
    // From the lab, bob has only what readers have as well.
    let mut lab = login(proxy(server, "10.3.0.9", true), "bob", "bob-pw").unwrap();
    assert_eq!(code(lab.execute("SELECT count(*) FROM notes", &[])), "ok");
    assert_eq!(
        code(lab.execute("INSERT INTO notes VALUES (4)", &[])),
        "42501"
    );
    // nc_catalog.sessions: who, from where, by which rule, under what.
    let rows = admin
        .rows(
            "SELECT user_name, client_addr, access_rule, read_only, as_role, front_end, auth_method
             FROM nc_catalog.sessions WHERE session_user_name IN ('alice', 'bob') ORDER BY client_addr",
            &[],
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::from("alice"),
                Value::from("10.2.3.1"),
                Value::from("alice_desk"),
                Value::Bool(false),
                Value::Null,
                Value::from("ncp"),
                Value::from("password"),
            ],
            vec![
                Value::from("alice"),
                Value::from("10.2.3.2"),
                Value::from("alice_kiosk"),
                Value::Bool(true),
                Value::Null,
                Value::from("ncp"),
                Value::from("password"),
            ],
            vec![
                Value::from("bob"),
                Value::from("10.3.0.9"),
                Value::from("bob_lab"),
                Value::Bool(false),
                Value::from("readers"),
                Value::from("ncp"),
                Value::from("password"),
            ],
            vec![
                Value::from("alice"),
                Value::from("192.168.7.7"),
                Value::from("alice_home"),
                Value::Bool(false),
                Value::Null,
                Value::from("ncp"),
                Value::from("password"),
            ],
        ]
    );
    // alice sees her own sessions only.
    assert_eq!(
        desk.rows(
            "SELECT count(*) FROM nc_catalog.sessions WHERE session_user_name <> 'alice'",
            &[]
        )
        .unwrap(),
        vec![vec![Value::Int(0)]]
    );
    // Without rules, every login is admitted again.
    for r in [
        "no_carol",
        "admins",
        "alice_desk",
        "alice_kiosk",
        "alice_home",
        "bob_lab",
    ] {
        admin
            .execute(&format!("DROP ACCESS RULE {r}"), &[])
            .unwrap();
    }
    assert!(login(from("172.16.0.1"), "carol", "carol-pw").is_ok());
}

/// Two real source addresses, without a proxy: 127.0.0.1 and ::1 (where
/// the host has IPv6).
#[test]
fn access_rules_see_the_real_address() {
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        return; // no IPv6 here
    };
    let db: Arc<dyn Kv> = Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(1)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    );
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let (a4, a6) = (v4.local_addr().unwrap(), v6.local_addr().unwrap());
    let db2 = Arc::clone(&db);
    std::thread::spawn(move || serve(v4, db, nucleus_server::Config::default()));
    std::thread::spawn(move || serve(v6, db2, nucleus_server::Config::default()));
    Client::connect(config(a4))
        .unwrap()
        .execute("CREATE USER root SUPERUSER PASSWORD 'root-pw'", &[])
        .unwrap();
    let mut admin = Client::connect(Config {
        user: "root".into(),
        password: Some("root-pw".into()),
        ..config(a4)
    })
    .unwrap();
    for sql in [
        "CREATE USER dave PASSWORD 'dave-pw'",
        "CREATE TABLE t (x int)",
        "GRANT SELECT, INSERT ON t TO dave",
        "CREATE ACCESS RULE everyone_v4 FROM '127.0.0.1/32' ALLOW",
        "CREATE ACCESS RULE dave_v6 FOR dave FROM '::1/128' ALLOW READ ONLY",
    ] {
        admin.execute(sql, &[]).unwrap();
    }
    let dave = |at: SocketAddr| {
        Client::connect(Config {
            user: "dave".into(),
            password: Some("dave-pw".into()),
            ..config(at)
        })
        .unwrap()
    };
    assert!(dave(a4).execute("INSERT INTO t VALUES (1)", &[]).is_ok());
    let err = dave(a6)
        .execute("INSERT INTO t VALUES (2)", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Server(e) if e.code == "25006"));
    // root from ::1: no rule admits it.
    assert!(
        Client::connect(Config {
            user: "root".into(),
            password: Some("root-pw".into()),
            ..config(a6)
        })
        .is_err()
    );
}

/// A server on a real database (the real clock: for timeouts and lock
/// times), with `config`.
fn start_real(config: nucleus_server::Config) -> SocketAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nucleus-client-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db: Arc<dyn Kv> = Arc::new(Database::open(&dir, Options::default()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || serve(listener, db, config));
    addr
}

/// Protection (PLAN.md §M14.25): failed logins slowed and then locked,
/// connection limits, statement budgets — a timeout, and a token's rows,
/// scan and rate.
#[test]
fn protection_limits_logins_and_statements() {
    let addr = start_real(nucleus_server::Config {
        max_connections: Some(6),
        ..Default::default()
    });
    let login = |user: &str, password: &str| {
        Client::connect(Config {
            user: user.into(),
            password: Some(password.into()),
            ..config(addr)
        })
    };
    fn code<T>(r: Result<T, Error>) -> String {
        match r {
            Err(Error::Server(e)) => e.code,
            Err(e) => format!("{e}"),
            Ok(_) => "ok".into(),
        }
    }
    login("nucleus", "nucleus")
        .unwrap()
        .execute("CREATE USER root SUPERUSER PASSWORD 'root-password'", &[])
        .unwrap();
    let mut root = login("root", "root-password").unwrap();
    for sql in [
        "ALTER DATABASE nucleus SET login_lock_failures = 3",
        "ALTER DATABASE nucleus SET login_lock_time = '1 minute'",
        "CREATE USER app PASSWORD 'app-password' CONNECTION LIMIT 2",
        "CREATE USER other PASSWORD 'other-password'",
        "CREATE ROLE agent",
        "CREATE TABLE t (x int)",
        "INSERT INTO t SELECT * FROM generate_series(1, 100)",
        "GRANT SELECT ON t TO agent, app",
    ] {
        root.execute(sql, &[]).unwrap();
    }
    // Three failures lock app: then even its password is refused; another
    // role is not touched.
    for _ in 0..3 {
        assert_eq!(code(login("app", "wrong")), "28P01");
    }
    assert_eq!(code(login("app", "app-password")), "28000");
    assert!(login("other", "other-password").is_ok());
    // The lock is in memory for a lock time; another server would not have
    // it — here the setting shortens it to nothing for the rest.
    root.execute("ALTER DATABASE nucleus SET login_lock_time = '1ms'", &[])
        .unwrap();
    // (The lock already set lasts its minute; a fresh role shows the count
    // starting again after a success.)
    root.execute(
        "CREATE USER app2 PASSWORD 'app2-password' CONNECTION LIMIT 2",
        &[],
    )
    .unwrap();
    root.execute("GRANT SELECT ON t TO app2", &[]).unwrap();
    assert_eq!(code(login("app2", "wrong")), "28P01");
    assert!(login("app2", "app2-password").is_ok());
    // CONNECTION LIMIT 2: a third session at once is refused.
    let a = login("app2", "app2-password").unwrap();
    let b = login("app2", "app2-password").unwrap();
    assert_eq!(code(login("app2", "app2-password")), "53300");
    drop(a);
    drop(b);
    // The server sees the sessions end a moment later.
    let mut ok = false;
    for _ in 0..50 {
        if login("app2", "app2-password").is_ok() {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ok);
    // statement_timeout: the session's, or the database's.
    root.execute("SET statement_timeout = '50ms'", &[]).unwrap();
    assert_eq!(
        code(root.execute("SELECT count(*) FROM generate_series(1, 50000000)", &[])),
        "57014"
    );
    assert_eq!(
        code(root.execute(
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT count(*) FROM r",
            &[]
        )),
        "57014"
    );
    root.execute("RESET statement_timeout", &[]).unwrap();
    assert_eq!(code(root.execute("SELECT count(*) FROM t", &[])), "ok");
    // A token's budgets.
    let r = root
        .rows(
            "CREATE TOKEN bot FOR agent ROWS 5 SCAN 50 RATE 4 PER MINUTE TIMEOUT '1 minute'",
            &[],
        )
        .unwrap();
    let (Value::Text(l), Value::Text(s)) = (&r[0][1], &r[0][2]) else {
        panic!("{r:?}")
    };
    let mut bot = login(l, s).unwrap();
    assert_eq!(
        code(bot.execute("SELECT x FROM t WHERE x <= 5", &[])),
        "54000"
    ); // reads 100
    assert_eq!(code(bot.execute("SELECT x FROM t LIMIT 6", &[])), "54000"); // returns 6
    assert_eq!(code(bot.execute("SELECT 1", &[])), "ok");
    assert_eq!(code(bot.execute("SELECT 2", &[])), "ok");
    assert_eq!(code(bot.execute("SELECT 3", &[])), "53400"); // the fifth a minute
    assert_eq!(
        root.rows(
            "SELECT max_rows, max_scan, rate_per_minute FROM nc_catalog.tokens WHERE name = 'bot'",
            &[]
        )
        .unwrap(),
        vec![vec![Value::Int(5), Value::Int(50), Value::Int(4)]]
    );
}

/// The audit log (PLAN.md §M14.25 "done when"): every login and every
/// change of access recorded, statements as asked, and a broken chain —
/// an edit, a gap, a cut end — detected.
#[test]
fn the_audit_log_records_and_shows_tampering() {
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
    let kv = Arc::clone(&db);
    std::thread::spawn(move || serve(listener, kv, nucleus_server::Config::default()));
    let login = |user: &str, password: &str| {
        Client::connect(Config {
            user: user.into(),
            password: Some(password.into()),
            ..config(addr)
        })
    };
    login("nucleus", "nucleus")
        .unwrap()
        .execute("CREATE USER root SUPERUSER PASSWORD 'root-pw'", &[])
        .unwrap();
    let mut root = login("root", "root-pw").unwrap();
    for sql in [
        "CREATE USER app PASSWORD 'app-pw'",
        "CREATE TABLE pay (x int)",
        "GRANT SELECT, INSERT ON pay TO app",
        "ALTER DATABASE nucleus SET audit_tables = 'pay'",
    ] {
        root.execute(sql, &[]).unwrap();
    }
    assert!(login("app", "wrong").is_err());
    let mut app = login("app", "app-pw").unwrap();
    app.execute("INSERT INTO pay VALUES (1)", &[]).unwrap();
    app.execute("SELECT * FROM pay", &[]).unwrap();
    assert!(app.execute("DROP TABLE pay", &[]).is_err()); // refused: recorded too
    let rows = root
        .rows(
            "SELECT kind, ok, coalesce(code, ''), session_user_name, client_addr, detail FROM nc_catalog.audit ORDER BY seq",
            &[],
        )
        .unwrap();
    let text = |v: &Value| match v {
        Value::Text(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    };
    let lines: Vec<String> = rows
        .iter()
        .map(|r| r.iter().map(text).collect::<Vec<_>>().join(" | "))
        .collect();
    let want = [
        "login | true |  | nucleus | 127.0.0.1 | admitted",
        "ddl | true |  | nucleus | 127.0.0.1 | CREATE USER root SUPERUSER PASSWORD 'root-pw'",
        "login | true |  | root | 127.0.0.1 | admitted",
        "ddl | true |  | root | 127.0.0.1 | CREATE USER app PASSWORD 'app-pw'",
        "ddl | true |  | root | 127.0.0.1 | CREATE TABLE pay (x int)",
        "ddl | true |  | root | 127.0.0.1 | GRANT SELECT, INSERT ON pay TO app",
        "ddl | true |  | root | 127.0.0.1 | ALTER DATABASE nucleus SET audit_tables = 'pay'",
        "login | false | 28P01 | app | 127.0.0.1 | password authentication failed for user \"app\"",
        "login | true |  | app | 127.0.0.1 | admitted",
        "statement | true |  | app | 127.0.0.1 | INSERT INTO pay VALUES (1)",
        "statement | true |  | app | 127.0.0.1 | SELECT * FROM pay",
        "ddl | false | 42501 | app | 127.0.0.1 | DROP TABLE pay",
    ];
    assert_eq!(lines, want);
    let chain = |root: &mut Client| {
        root.rows(
            "SELECT records, intact, broken_at, problem FROM nc_catalog.audit_chain",
            &[],
        )
        .unwrap()
    };
    // (The query itself is not recorded: it touches no audited table.)
    assert_eq!(
        chain(&mut root),
        vec![vec![
            Value::Int(12),
            Value::Bool(true),
            Value::Null,
            Value::Null
        ]]
    );
    // Only an administrator reads it.
    assert_eq!(
        app.rows("SELECT count(*) FROM nc_catalog.audit", &[])
            .unwrap(),
        vec![vec![Value::Int(0)]]
    );
    // Tampering, straight in the store: an edit, a gap, a cut end.
    let key = |seq: u64| {
        let mut k = vec![0x00, 0x12];
        k.extend_from_slice(&seq.to_be_bytes());
        k
    };
    let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
    let mut rec = txn.get(&key(4)).unwrap().unwrap();
    let at = rec.windows(6).position(|w| w == b"app-pw").unwrap();
    rec[at] = b'X';
    txn.put(&key(4), &rec).unwrap();
    txn.commit().unwrap();
    let broken = chain(&mut root);
    assert_eq!(
        broken[0][1..],
        [
            Value::Bool(false),
            Value::Int(4),
            Value::from("it was changed")
        ]
    );
    // Put back exactly, it holds again; a record taken out is a gap.
    let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
    rec[at] = b'a';
    txn.put(&key(4), &rec).unwrap();
    let six = txn.get(&key(6)).unwrap().unwrap();
    txn.delete(&key(6)).unwrap();
    txn.commit().unwrap();
    let broken = chain(&mut root);
    assert_eq!(
        broken[0][1..],
        [
            Value::Bool(false),
            Value::Int(6),
            Value::from("a record is missing")
        ]
    );
    // The last records cut off: the head still names them.
    let mut txn = db.begin(nucleus_kv::TxnOptions::default()).unwrap();
    txn.put(&key(6), &six).unwrap();
    txn.delete(&key(12)).unwrap();
    txn.commit().unwrap();
    let broken = chain(&mut root);
    assert_eq!(
        broken[0][1..],
        [
            Value::Bool(false),
            Value::Int(12),
            Value::from("the last records are missing (the head names more)")
        ]
    );
}

/// Results stream (PLAN.md §M14.11): a cursor reads a statement's rows a
/// batch at a time, the server waiting for each; dropped early, it stops
/// the statement; an error after rows ends it as its last item.
#[test]
fn cursors_fetch_in_batches() {
    let mut c = Client::connect(config(start(None))).unwrap();
    let mut cur = c
        .cursor(
            "SELECT i, 'r' || i FROM generate_series(1, 10000) i",
            &[],
            128,
        )
        .unwrap();
    assert_eq!(cur.columns()[1].ty, Type::Text);
    let mut n = 0i64;
    for row in &mut cur {
        let row = row.unwrap();
        n += 1;
        assert_eq!(row[0], Value::Integer(n as i32));
    }
    assert_eq!(n, 10_000);
    assert_eq!(cur.command(), Some(("SELECT", 10_000)));
    drop(cur);

    // Ten billion rows: only what is read is ever produced.
    let big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b";
    let mut cur = c.cursor(big, &[], 1000).unwrap();
    let first: Vec<_> = (&mut cur).take(2500).map(Result::unwrap).collect();
    assert_eq!(first.len(), 2500);
    // Closed in the middle of a batch: what was sent is counted.
    assert_eq!(cur.close().unwrap(), ("SELECT".into(), 3000));
    // (0: the default batch.)
    let mut cur = c.cursor(big, &[], 0).unwrap();
    assert!(cur.next().unwrap().is_ok());
    drop(cur);
    assert_eq!(c.status(), TxnStatus::Idle);
    assert_eq!(
        c.rows("SELECT 1", &[]).unwrap(),
        vec![vec![Value::Integer(1)]]
    );

    // An error before the first row; after rows, as the last item.
    match c.cursor("SELECT nope", &[], 10) {
        Err(Error::Server(e)) => assert_eq!(e.code, "42703"),
        other => panic!("{other:?}"),
    }
    let rows: Vec<_> = c
        .cursor(
            "SELECT 1 / (500 - i) FROM generate_series(1, 1000) i",
            &[],
            100,
        )
        .unwrap()
        .collect();
    assert_eq!(rows.len(), 500);
    assert!(rows[..499].iter().all(Result::is_ok));
    match &rows[499] {
        Err(Error::Server(e)) => assert_eq!(e.code, "22012"),
        other => panic!("{other:?}"),
    }

    // In a block the statement's snapshot stays the block's; a cursor of a
    // statement without rows has none; batches need one statement.
    c.execute("BEGIN", &[]).unwrap();
    let cur = c.cursor("CREATE TABLE cu (id INT)", &[], 10).unwrap();
    assert!(cur.columns().is_empty());
    assert_eq!(cur.command(), Some(("CREATE TABLE", 0)));
    drop(cur);
    c.execute("INSERT INTO cu SELECT * FROM generate_series(1, 50)", &[])
        .unwrap();
    let n = c.cursor("SELECT id FROM cu", &[], 7).unwrap().count();
    assert_eq!(n, 50);
    c.execute("COMMIT", &[]).unwrap();
    match c.cursor("SELECT 1; SELECT 2", &[], 10) {
        Err(Error::Server(e)) => assert_eq!(e.code, "42601"),
        other => panic!("{other:?}"),
    }
}

/// Time a cursor's client spends between batches is not the statement's:
/// a statement timeout counts the server's work only.
#[test]
fn a_cursors_pauses_are_not_statement_time() {
    let mut c = Client::connect(config(start_real(nucleus_server::Config::default()))).unwrap();
    c.execute("SET statement_timeout = '300ms'", &[]).unwrap();
    let cur = c
        .cursor("SELECT i FROM generate_series(1, 40) i", &[], 10)
        .unwrap();
    let mut n = 0;
    for row in cur {
        row.unwrap();
        n += 1;
        if n % 10 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    assert_eq!(n, 40);
}

/// Columnar results (PLAN.md §M14.11 L3): every type, NULLs, batches the
/// size asked for — each cell the value a cursor gives.
#[test]
fn columnar_batches_match_rows() {
    let mut c = Client::connect(config(start(None))).unwrap();
    c.execute(
        "CREATE TABLE col (id INT PRIMARY KEY, b BOOLEAN, s SMALLINT, big BIGINT, r REAL, f FLOAT, \
         n NUMERIC(10, 2), t TEXT, ch CHAR(3), bt BYTEA, d DATE, tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ, \
         iv INTERVAL, u UUID, ip INET, net CIDR, arr INT[])",
        &[],
    )
    .unwrap();
    c.execute(
        "INSERT INTO col SELECT i, i % 2 = 0, i::smallint, i * 1000000000000, i / 4.0, i / 8.0, i / 3.0, \
         'v' || i, 'c' || (i % 10), decode('ff00', 'hex'), DATE '2024-01-01' + i, TIME '12:00' + i * INTERVAL '1 second', \
         TIMESTAMP '2024-01-01' + i * INTERVAL '1 hour', TIMESTAMPTZ '2024-01-01 00:00Z' + i * INTERVAL '1 minute', \
         i * INTERVAL '1 day', gen_random_uuid(), '10.0.0.1'::inet, '10.0.0.0/8'::cidr, ARRAY[i, NULL] \
         FROM generate_series(1, 2500) i",
        &[],
    )
    .unwrap();
    // Every third row NULL in every column but the key.
    c.execute(
        "UPDATE col SET b = NULL, s = NULL, big = NULL, r = NULL, f = NULL, n = NULL, t = NULL, ch = NULL, \
         bt = NULL, d = NULL, tm = NULL, ts = NULL, tz = NULL, iv = NULL, u = NULL, ip = NULL, net = NULL, \
         arr = NULL WHERE id % 3 = 0",
        &[],
    )
    .unwrap();
    let sql = "SELECT * FROM col ORDER BY id";
    let rows: Vec<Vec<Value>> = c.rows(sql, &[]).unwrap();
    let mut batches = c.batches(sql, &[], 1000).unwrap();
    assert_eq!(batches.columns()[16].ty, Type::Inet);
    let mut seen = 0;
    let mut sizes = Vec::new();
    for batch in &mut batches {
        let batch = batch.unwrap();
        sizes.push(batch.rows());
        for i in 0..batch.rows() {
            for (col, want) in rows[seen + i].iter().enumerate() {
                assert_eq!(
                    &batch.value(i, col).unwrap(),
                    want,
                    "row {} column {col}",
                    seen + i
                );
            }
        }
        seen += batch.rows();
    }
    assert_eq!(sizes, [1000, 1000, 500]);
    assert_eq!(batches.command(), Some(("SELECT", 2500)));
    drop(batches);

    // Typed buffers: no value per cell.
    let mut b = c
        .batches("SELECT id, big FROM col WHERE id <= 10 ORDER BY id", &[], 0)
        .unwrap();
    let batch = b.next().unwrap().unwrap();
    assert_eq!(
        batch.column(0).i32s().unwrap(),
        &(1..=10).collect::<Vec<_>>()[..]
    );
    assert!(batch.column(1).is_null(2) && !batch.column(1).is_null(3));
    assert_eq!(batch.column(1).i64s().unwrap()[3], 4_000_000_000_000);
    assert!(b.next().is_none());
    drop(b);

    // Closed early; an empty result has no batch; an error after batches
    // is the last item.
    let big = "SELECT a, b FROM generate_series(1, 100000) a, generate_series(1, 100000) b";
    let mut b = c.batches(big, &[], 4096).unwrap();
    assert_eq!(b.next().unwrap().unwrap().rows(), 4096);
    // (The server waits after each batch: one was sent.)
    assert_eq!(b.close().unwrap(), ("SELECT".into(), 4096));
    assert!(
        c.batches("SELECT * FROM col WHERE false", &[], 0)
            .unwrap()
            .next()
            .is_none()
    );
    let items: Vec<_> = c
        .batches(
            "SELECT 1 / (250 - i) FROM generate_series(1, 400) i",
            &[],
            100,
        )
        .unwrap()
        .collect();
    assert_eq!(items.len(), 4);
    assert_eq!(
        items[..2]
            .iter()
            .map(|b| b.as_ref().unwrap().rows())
            .sum::<usize>(),
        200
    );
    assert_eq!(items[2].as_ref().unwrap().rows(), 49);
    assert!(matches!(&items[3], Err(Error::Server(e)) if e.code == "22012"));
    assert_eq!(c.status(), TxnStatus::Idle);
}
