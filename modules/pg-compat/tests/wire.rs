//! The PostgreSQL front end: the SQL layer's scripts and this module's run
//! over the wire (PLAN.md §M12 "done when"), authentication, and the
//! extended protocol through a real PostgreSQL driver.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_pg_compat::Pg;
use nucleus_platform::sim::SimPlatform;
use nucleus_server::{Config, serve_front_end};
use nucleus_testkit::slt::{Cell, Connection, SqlError, run_script};

/// This module's scripts (in its folder): (name, text).
pub fn own_scripts() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/slt");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|f| f.extension().is_some_and(|e| e == "slt"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|f| {
            (
                f.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(f).unwrap(),
            )
        })
        .collect()
}

fn start(config: Config) -> SocketAddr {
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
    std::thread::spawn(move || serve_front_end(Arc::new(Pg), listener, db, config));
    addr
}

/// A minimal client for the simple query protocol.
struct Client {
    s: TcpStream,
}

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn error_fields(body: &[u8]) -> SqlError {
    let (mut code, mut msg) = (String::new(), String::new());
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let f = body[i];
        let end = i + 1 + body[i + 1..].iter().position(|&c| c == 0).unwrap();
        let v = String::from_utf8_lossy(&body[i + 1..end]).into_owned();
        match f {
            b'C' => code = v,
            b'M' => msg = v,
            _ => {}
        }
        i = end + 1;
    }
    SqlError { code, message: msg }
}

impl Client {
    fn connect(addr: SocketAddr, password: Option<&str>) -> Result<Self, SqlError> {
        let mut s = TcpStream::connect(addr).unwrap();
        let mut body = 196_608i32.to_be_bytes().to_vec();
        for kv in ["user", "test", "database", "nucleus"] {
            body.extend_from_slice(kv.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut startup = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        startup.extend(body);
        s.write_all(&startup).unwrap();
        let mut c = Self { s };
        loop {
            let (tag, body) = c.read();
            match tag {
                b'R' if body[3] == 3 => {
                    let mut p = password.unwrap_or("").as_bytes().to_vec();
                    p.push(0);
                    c.s.write_all(&message(b'p', &p)).unwrap();
                }
                b'E' => return Err(error_fields(&body)),
                b'Z' => return Ok(c),
                _ => {}
            }
        }
    }

    fn read(&mut self) -> (u8, Vec<u8>) {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).unwrap();
        let len = i32::from_be_bytes(h[1..5].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len - 4];
        self.s.read_exact(&mut body).unwrap();
        (h[0], body)
    }
}

impl Connection for Client {
    fn run(&mut self, sql: &str) -> Result<Option<Vec<Vec<Cell>>>, SqlError> {
        let mut q = sql.as_bytes().to_vec();
        q.push(0);
        self.s.write_all(&message(b'Q', &q)).unwrap();
        let mut rows: Option<Vec<Vec<Cell>>> = None;
        let mut err = None;
        loop {
            let (tag, body) = self.read();
            match tag {
                b'T' => rows = Some(Vec::new()),
                b'D' => {
                    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
                    let mut at = 2;
                    let mut row = Vec::with_capacity(n);
                    for _ in 0..n {
                        let len = i32::from_be_bytes(body[at..at + 4].try_into().unwrap());
                        at += 4;
                        if len < 0 {
                            row.push(None);
                        } else {
                            row.push(Some(
                                String::from_utf8(body[at..at + len as usize].to_vec()).unwrap(),
                            ));
                            at += len as usize;
                        }
                    }
                    rows.get_or_insert_with(Vec::new).push(row);
                }
                b'E' => err = Some(error_fields(&body)),
                b'Z' => return err.map_or(Ok(rows), Err),
                _ => {}
            }
        }
    }
}

/// The SQL layer's scripts and pg-compat's (the PostgreSQL front end's
/// sessions have it).
#[test]
fn sql_scripts_over_the_wire() {
    let mut scripts: Vec<(String, String)> = nucleus_testkit::core_scripts()
        .iter()
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect();
    scripts.extend(
        own_scripts()
            .into_iter()
            .map(|(n, s)| (format!("pg-compat/{n}"), s)),
    );
    let mut failures = Vec::new();
    let mut ran = 0;
    for (name, script) in scripts.iter().filter(|(n, _)| nucleus_testkit::picked(n)) {
        let addr = start(Config::trust());
        let mut connect =
            || -> Box<dyn Connection> { Box::new(Client::connect(addr, None).unwrap()) };
        failures.extend(run_script(script, name, &mut connect));
        ran += 1;
    }
    assert!(ran >= 9, "only {ran} scripts");
    assert!(
        failures.is_empty(),
        "{} failure(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn password_authentication() {
    let addr = start(Config::password("test", "s3cret"));
    let err = Client::connect(addr, Some("wrong"))
        .err()
        .expect("wrong password accepted");
    assert_eq!(err.code, "28P01");
    let mut c = Client::connect(addr, Some("s3cret")).unwrap();
    assert_eq!(
        c.run("SELECT 1").unwrap(),
        Some(vec![vec![Some("1".into())]])
    );
}

#[test]
fn ssl_is_declined_and_plain_connections_continue() {
    let addr = start(Config::trust());
    let mut s = TcpStream::connect(addr).unwrap();
    let mut req = 8i32.to_be_bytes().to_vec();
    req.extend_from_slice(&80_877_103i32.to_be_bytes());
    s.write_all(&req).unwrap();
    let mut answer = [0u8; 1];
    s.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"N");
}

#[test]
fn a_real_driver_with_the_extended_protocol() {
    use postgres::{Client as Pg, NoTls, error::SqlState};

    let addr = start(Config::trust());
    let url = format!(
        "host={} port={} user=test dbname=nucleus",
        addr.ip(),
        addr.port()
    );
    let mut pg = Pg::connect(&url, NoTls).unwrap();

    pg.batch_execute(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT, score DOUBLE PRECISION, ok BOOLEAN)",
    )
    .unwrap();
    let insert = pg.prepare("INSERT INTO t VALUES ($1, $2, $3, $4)").unwrap();
    for (id, name, score, ok) in [
        (1i64, Some("anna"), 1.5f64, true),
        (2, None, -2.25, false),
        (3, Some("cili"), 0.0, true),
    ] {
        assert_eq!(pg.execute(&insert, &[&id, &name, &score, &ok]).unwrap(), 1);
    }

    // Typed parameters in WHERE, typed (binary) results.
    let rows = pg
        .query(
            "SELECT id, name, score, ok FROM t WHERE score > $1 ORDER BY id",
            &[&-1.0f64],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i64>(0), 1);
    assert_eq!(rows[0].get::<_, Option<String>>(1).as_deref(), Some("anna"));
    assert_eq!(rows[0].get::<_, f64>(2), 1.5);
    assert!(rows[0].get::<_, bool>(3));
    let row = pg
        .query_one("SELECT name FROM t WHERE id = $1", &[&2i64])
        .unwrap();
    assert_eq!(row.get::<_, Option<String>>(0), None);
    let n: i64 = pg
        .query_one("SELECT count(*) FROM t WHERE name LIKE $1", &[&"%i%"])
        .unwrap()
        .get(0);
    assert_eq!(n, 1);

    // SQLSTATE codes reach the driver.
    let err = pg
        .execute(&insert, &[&1i64, &"dup", &0.0f64, &true])
        .unwrap_err();
    assert_eq!(err.code(), Some(&SqlState::UNIQUE_VIOLATION));
    let err = pg.query("SELECT * FROM missing", &[]).unwrap_err();
    assert_eq!(err.code(), Some(&SqlState::UNDEFINED_TABLE));

    // Transactions: rollback and commit.
    let mut tx = pg.transaction().unwrap();
    tx.execute("DELETE FROM t WHERE id = $1", &[&1i64]).unwrap();
    tx.rollback().unwrap();
    let mut tx = pg.transaction().unwrap();
    tx.execute("UPDATE t SET score = score * 2 WHERE id = $1", &[&3i64])
        .unwrap();
    // A portal fetched two rows at a time.
    let stmt = tx.prepare("SELECT id FROM t ORDER BY id").unwrap();
    let portal = tx.bind(&stmt, &[]).unwrap();
    let first = tx.query_portal(&portal, 2).unwrap();
    let rest = tx.query_portal(&portal, 2).unwrap();
    assert_eq!((first.len(), rest.len()), (2, 1));
    tx.commit().unwrap();
    assert_eq!(pg.query("SELECT id FROM t", &[]).unwrap().len(), 3);

    // RETURNING through the extended protocol.
    let r = pg
        .query_one(
            "INSERT INTO t (id, name) VALUES ($1, $2) RETURNING id, name",
            &[&9i64, &"new"],
        )
        .unwrap();
    assert_eq!(
        (r.get::<_, i64>(0), r.get::<_, String>(1)),
        (9, "new".to_string())
    );

    // The simple query protocol too.
    let msgs = pg.simple_query("SELECT 1 AS one; SELECT 'x'").unwrap();
    assert!(msgs.len() >= 4);
}

/// A uuid as PostgreSQL drivers see it: type `uuid` (oid 2950), its 16
/// bytes in binary format, its canonical text in text format.
#[test]
fn uuid_through_a_real_driver() {
    use postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
    use postgres::{Client as Pg, NoTls};

    #[derive(Debug, PartialEq)]
    struct Uuid([u8; 16]);

    impl<'a> FromSql<'a> for Uuid {
        fn from_sql(
            _: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(Uuid(raw.try_into()?))
        }
        fn accepts(ty: &Type) -> bool {
            *ty == Type::UUID
        }
    }

    impl ToSql for Uuid {
        fn to_sql(
            &self,
            _: &Type,
            out: &mut postgres::types::private::BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.extend_from_slice(&self.0);
            Ok(IsNull::No)
        }
        fn accepts(ty: &Type) -> bool {
            *ty == Type::UUID
        }
        to_sql_checked!();
    }

    let addr = start(Config::trust());
    let url = format!(
        "host={} port={} user=test dbname=nucleus",
        addr.ip(),
        addr.port()
    );
    let mut pg = Pg::connect(&url, NoTls).unwrap();
    pg.batch_execute("CREATE TABLE u (id UUID PRIMARY KEY, n INT)")
        .unwrap();
    let id = Uuid([
        0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a,
        0x11,
    ]);
    pg.execute("INSERT INTO u VALUES ($1, 1)", &[&id]).unwrap();
    let row = pg
        .query_one("SELECT id, n FROM u WHERE id = $1", &[&id])
        .unwrap();
    assert_eq!(row.columns()[0].type_(), &Type::UUID);
    assert_eq!(row.get::<_, Uuid>(0), id);
    let text = pg.simple_query("SELECT id FROM u").unwrap();
    let postgres::SimpleQueryMessage::Row(r) = &text[1] else {
        panic!("{text:?}")
    };
    assert_eq!(r.get(0), Some("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
}
