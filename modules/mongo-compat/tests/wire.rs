//! The MongoDB front end over a socket, spoken by a client of the test's
//! own (this crate's BSON): the handshake a driver makes, a login by
//! SCRAM-SHA-256, CRUD and queries, the errors drivers act on.  `mongosh`
//! and a standard driver are run against it outside the repository
//! (PLAN.md §M16 Q5).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use nucleus_kv::{Database, Kv, Options};
use nucleus_mongo_compat::Mongo;
use nucleus_mongo_compat::bson::{self, B, Doc};
use nucleus_platform::sim::SimPlatform;
use nucleus_server::{Config, ScramClient, serve_front_end};

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
    std::thread::spawn(move || serve_front_end(Arc::new(Mongo), listener, db, config));
    addr
}

struct Client {
    s: TcpStream,
    id: i32,
}

fn s(v: &str) -> B {
    B::Str(v.into())
}

fn d(fields: &[(&str, B)]) -> B {
    B::Doc(
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        Client {
            s: TcpStream::connect(addr).unwrap(),
            id: 0,
        }
    }

    fn read_message(&mut self) -> (i32, Vec<u8>) {
        let mut h = [0u8; 16];
        self.s.read_exact(&mut h).unwrap();
        let len = i32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        let to = i32::from_le_bytes(h[8..12].try_into().unwrap());
        assert_eq!(to, self.id, "a reply to the request sent");
        let op = i32::from_le_bytes(h[12..16].try_into().unwrap());
        let mut body = vec![0u8; len - 16];
        self.s.read_exact(&mut body).unwrap();
        (op, body)
    }

    /// The first `hello`, as drivers send it: OP_QUERY on admin.$cmd.
    fn legacy_hello(&mut self) -> Doc {
        self.id += 1;
        let q = bson::write_doc(&[
            ("isMaster".into(), B::Int32(1)),
            ("saslSupportedMechs".into(), s("admin.nucleus")),
        ]);
        let mut body = 0i32.to_le_bytes().to_vec();
        body.extend_from_slice(b"admin.$cmd\0");
        body.extend_from_slice(&0i32.to_le_bytes());
        body.extend_from_slice(&(-1i32).to_le_bytes());
        body.extend_from_slice(&q);
        let mut m = ((16 + body.len()) as i32).to_le_bytes().to_vec();
        m.extend_from_slice(&self.id.to_le_bytes());
        m.extend_from_slice(&0i32.to_le_bytes());
        m.extend_from_slice(&2004i32.to_le_bytes());
        m.extend_from_slice(&body);
        self.s.write_all(&m).unwrap();
        let (op, body) = self.read_message();
        assert_eq!(op, 1, "OP_REPLY");
        assert_eq!(
            i32::from_le_bytes(body[16..20].try_into().unwrap()),
            1,
            "one document"
        );
        bson::read_doc(&body[20..]).unwrap().0
    }

    /// A command by OP_MSG; `docs`: a document sequence (insert's).
    fn run(&mut self, cmd: B, docs: Option<(&str, Vec<B>)>) -> Doc {
        self.id += 1;
        let B::Doc(mut cmd) = cmd else { panic!() };
        cmd.push(("$db".into(), s("test")));
        let mut body = 0u32.to_le_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(&bson::write_doc(&cmd));
        if let Some((name, docs)) = docs {
            let mut seq = name.as_bytes().to_vec();
            seq.push(0);
            for doc in docs {
                let B::Doc(doc) = doc else { panic!() };
                seq.extend_from_slice(&bson::write_doc(&doc));
            }
            body.push(1);
            body.extend_from_slice(&((seq.len() + 4) as i32).to_le_bytes());
            body.extend_from_slice(&seq);
        }
        let mut m = ((16 + body.len()) as i32).to_le_bytes().to_vec();
        m.extend_from_slice(&self.id.to_le_bytes());
        m.extend_from_slice(&0i32.to_le_bytes());
        m.extend_from_slice(&2013i32.to_le_bytes());
        m.extend_from_slice(&body);
        self.s.write_all(&m).unwrap();
        let (op, body) = self.read_message();
        assert_eq!(op, 2013, "OP_MSG");
        assert_eq!(body[4], 0, "a body section");
        bson::read_doc(&body[5..]).unwrap().0
    }

    fn ok(&mut self, cmd: B, docs: Option<(&str, Vec<B>)>) -> Doc {
        let r = self.run(cmd, docs);
        assert_eq!(
            bson::get(&r, "ok"),
            Some(&B::Double(1.0)),
            "{}",
            bson::doc_json(&r, false)
        );
        r
    }

    fn login(&mut self, user: &str, password: &str) -> Doc {
        let mut scram = ScramClient::new(password, "abcdefghijkl").with_user(user);
        let r = self.run(
            d(&[
                ("saslStart", B::Int32(1)),
                ("mechanism", s("SCRAM-SHA-256")),
                ("payload", B::Binary(0, scram.first().into_bytes())),
                ("options", d(&[("skipEmptyExchange", B::Bool(true))])),
            ]),
            None,
        );
        let Some(B::Binary(_, first)) = bson::get(&r, "payload") else {
            return r;
        };
        let last = scram.respond(&String::from_utf8_lossy(first)).unwrap();
        self.run(
            d(&[
                ("saslContinue", B::Int32(1)),
                ("conversationId", B::Int32(1)),
                ("payload", B::Binary(0, last.into_bytes())),
            ]),
            None,
        )
    }
}

fn batch(r: &Doc, name: &str) -> Vec<Doc> {
    let c = bson::get(r, "cursor")
        .and_then(B::as_doc)
        .expect("a cursor");
    match bson::get(c, name) {
        Some(B::Arr(a)) => a.iter().map(|x| x.as_doc().unwrap().clone()).collect(),
        other => panic!("{other:?}"),
    }
}

fn names(docs: &[Doc]) -> Vec<String> {
    docs.iter()
        .map(|x| {
            bson::get(x, "name")
                .and_then(B::as_str)
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

#[test]
fn a_driver_logs_in_and_runs_crud_and_queries() {
    let addr = start(Config::default());
    let mut c = Client::connect(addr);
    let hello = c.legacy_hello();
    assert_eq!(bson::get(&hello, "ismaster"), Some(&B::Bool(true)));
    assert!(
        matches!(bson::get(&hello, "saslSupportedMechs"), Some(B::Arr(m)) if m == &vec![s("SCRAM-SHA-256")])
    );
    // Nothing before the login; a wrong password refused.
    let r = c.run(d(&[("find", s("people"))]), None);
    assert_eq!(bson::get(&r, "code"), Some(&B::Int32(13)));
    let r = Client::connect(addr).login("nucleus", "wrong");
    assert_eq!(
        bson::get(&r, "code"),
        Some(&B::Int32(18)),
        "{}",
        bson::doc_json(&r, false)
    );
    let r = c.login("nucleus", "nucleus");
    assert_eq!(
        bson::get(&r, "done"),
        Some(&B::Bool(true)),
        "{}",
        bson::doc_json(&r, false)
    );

    // A first write makes the collection; ids are kept, or made ObjectIds.
    let at = B::Date(1_700_000_000_000);
    let people = vec![
        d(&[
            ("_id", B::Int32(1)),
            ("name", s("ann")),
            ("age", B::Int32(34)),
            ("at", at.clone()),
        ]),
        d(&[
            ("_id", B::Int32(2)),
            ("name", s("bob")),
            ("age", B::Int32(25)),
            ("tags", B::Arr(vec![s("x")])),
        ]),
        d(&[
            ("name", s("cy")),
            ("age", B::Int64(40)),
            ("score", B::Double(2.5)),
        ]),
    ];
    let r = c.ok(d(&[("insert", s("people"))]), Some(("documents", people)));
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(3)));
    // A duplicate _id: 11000, the ones before it kept (ordered).
    let r = c.ok(
        d(&[("insert", s("people"))]),
        Some((
            "documents",
            vec![
                d(&[("_id", B::Int32(4)), ("name", s("dan"))]),
                d(&[("_id", B::Int32(1))]),
            ],
        )),
    );
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(1)));
    let err = match bson::get(&r, "writeErrors") {
        Some(B::Arr(e)) => e[0].as_doc().unwrap().clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(bson::get(&err, "index"), Some(&B::Int32(1)));
    assert_eq!(bson::get(&err, "code"), Some(&B::Int32(11000)));

    // find: filter, projection, sort, skip, limit; types come back.
    let r = c.ok(
        d(&[
            ("find", s("people")),
            ("filter", d(&[("age", d(&[("$gte", B::Int32(25))]))])),
            ("sort", d(&[("age", B::Int32(-1))])),
            ("projection", d(&[("name", B::Int32(1))])),
            ("skip", B::Int32(1)),
            ("limit", B::Int32(2)),
        ]),
        None,
    );
    assert_eq!(names(&batch(&r, "firstBatch")), ["ann", "bob"]);
    let r = c.ok(
        d(&[
            ("find", s("people")),
            ("filter", d(&[("_id", B::Int32(1))])),
        ]),
        None,
    );
    let ann = &batch(&r, "firstBatch")[0];
    assert_eq!(ann[0].0, "_id", "_id first");
    assert_eq!(bson::get(ann, "at"), Some(&at));
    let r = c.ok(
        d(&[("find", s("people")), ("filter", d(&[("name", s("cy"))]))]),
        None,
    );
    let cy = &batch(&r, "firstBatch")[0];
    assert!(matches!(bson::get(cy, "_id"), Some(B::ObjectId(_))));
    assert_eq!(bson::get(cy, "score"), Some(&B::Double(2.5)));
    // A regular expression, a date range, a cursor in batches.
    let r = c.ok(
        d(&[
            ("find", s("people")),
            (
                "filter",
                d(&[("name", B::Regex("^[ab]".into(), "".into()))]),
            ),
        ]),
        None,
    );
    assert_eq!(batch(&r, "firstBatch").len(), 2);
    let r = c.ok(
        d(&[
            ("find", s("people")),
            (
                "filter",
                d(&[("at", d(&[("$gt", B::Date(1_600_000_000_000))]))]),
            ),
        ]),
        None,
    );
    assert_eq!(names(&batch(&r, "firstBatch")), ["ann"]);
    let r = c.ok(
        d(&[
            ("find", s("people")),
            ("sort", d(&[("_id", B::Int32(1))])),
            ("batchSize", B::Int32(2)),
        ]),
        None,
    );
    assert_eq!(batch(&r, "firstBatch").len(), 2);
    let id = bson::get(bson::get(&r, "cursor").and_then(B::as_doc).unwrap(), "id")
        .cloned()
        .unwrap();
    assert_ne!(id, B::Int64(0));
    let r = c.ok(d(&[("getMore", id), ("collection", s("people"))]), None);
    assert_eq!(batch(&r, "nextBatch").len(), 2);

    // count, and countDocuments' aggregate.
    let r = c.ok(
        d(&[
            ("count", s("people")),
            ("query", d(&[("age", d(&[("$gt", B::Int32(30))]))])),
        ]),
        None,
    );
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(2)));
    let pipeline = B::Arr(vec![
        d(&[("$match", d(&[("age", d(&[("$exists", B::Bool(true))]))]))]),
        d(&[(
            "$group",
            d(&[("_id", B::Int32(1)), ("n", d(&[("$sum", B::Int32(1))]))]),
        )]),
    ]);
    let r = c.ok(
        d(&[
            ("aggregate", s("people")),
            ("pipeline", pipeline),
            ("cursor", d(&[])),
        ]),
        None,
    );
    assert_eq!(
        bson::get(&batch(&r, "firstBatch")[0], "n"),
        Some(&B::Int32(3))
    );

    // update: operators, many, an upsert's ObjectId; a replacement.
    let r = c.ok(
        d(&[
            ("update", s("people")),
            (
                "updates",
                B::Arr(vec![
                    d(&[
                        ("q", d(&[])),
                        ("u", d(&[("$max", d(&[("age", B::Int32(30))]))])),
                        ("multi", B::Bool(true)),
                    ]),
                    d(&[
                        ("q", d(&[("name", s("eve"))])),
                        ("u", d(&[("$set", d(&[("age", B::Int32(5))]))])),
                        ("upsert", B::Bool(true)),
                    ]),
                    d(&[
                        ("q", d(&[("_id", B::Int32(2))])),
                        ("u", d(&[("name", s("bobby"))])),
                    ]),
                ]),
            ),
        ]),
        None,
    );
    assert_eq!(
        bson::get(&r, "nModified"),
        Some(&B::Int32(3)),
        "{}",
        bson::doc_json(&r, false)
    );
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(4 + 1 + 1)));
    let up = match bson::get(&r, "upserted") {
        Some(B::Arr(u)) => u[0].as_doc().unwrap().clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(bson::get(&up, "index"), Some(&B::Int32(1)));
    assert!(matches!(bson::get(&up, "_id"), Some(B::ObjectId(_))));
    // An update document without operators where one needs them: an error.
    let r = c.ok(
        d(&[
            ("update", s("people")),
            (
                "updates",
                B::Arr(vec![d(&[("q", d(&[])), ("u", B::Arr(vec![]))])]),
            ),
        ]),
        None,
    );
    assert!(bson::get(&r, "writeErrors").is_some());

    // findAndModify, new and old.
    let r = c.ok(
        d(&[
            ("findAndModify", s("people")),
            ("query", d(&[("name", s("ann"))])),
            ("update", d(&[("$inc", d(&[("age", B::Int32(1))]))])),
            ("new", B::Bool(true)),
            ("fields", d(&[("age", B::Int32(1)), ("_id", B::Int32(0))])),
        ]),
        None,
    );
    assert_eq!(
        bson::get(&r, "value"),
        Some(&d(&[("age", B::Int32(35))])),
        "{}",
        bson::doc_json(&r, false)
    );

    // delete one and many.
    let r = c.ok(
        d(&[
            ("delete", s("people")),
            (
                "deletes",
                B::Arr(vec![d(&[
                    ("q", d(&[("age", d(&[("$gte", B::Int32(30))]))])),
                    ("limit", B::Int32(1)),
                ])]),
            ),
        ]),
        None,
    );
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(1)));
    let r = c.ok(
        d(&[
            ("delete", s("people")),
            (
                "deletes",
                B::Arr(vec![d(&[("q", d(&[])), ("limit", B::Int32(0))])]),
            ),
        ]),
        None,
    );
    assert_eq!(bson::get(&r, "n"), Some(&B::Int32(4)));

    // A collection not there: no documents, no error; listed once made.
    let r = c.ok(d(&[("find", s("nosuch"))]), None);
    assert!(batch(&r, "firstBatch").is_empty());
    let r = c.ok(
        d(&[
            ("listCollections", B::Int32(1)),
            ("nameOnly", B::Bool(true)),
        ]),
        None,
    );
    assert_eq!(names(&batch(&r, "firstBatch")), ["people"]);
    c.ok(
        d(&[
            ("createIndexes", s("people")),
            (
                "indexes",
                B::Arr(vec![d(&[
                    ("key", d(&[("age", B::Int32(1))])),
                    ("name", s("age_1")),
                ])]),
            ),
        ]),
        None,
    );
    c.ok(d(&[("drop", s("people"))]), None);
    // An unknown command, a bad filter: MongoDB's codes.
    let r = c.run(d(&[("mapReduce", s("people"))]), None);
    assert_eq!(bson::get(&r, "code"), Some(&B::Int32(59)));
    let r = c.run(
        d(&[
            ("find", s("people")),
            ("filter", d(&[("a", d(&[("$where", s("1"))]))])),
        ]),
        None,
    );
    assert_eq!(bson::get(&r, "ok"), Some(&B::Double(0.0)));
}
