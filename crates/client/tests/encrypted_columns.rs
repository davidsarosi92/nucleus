//! Client-side encrypted columns (PLAN.md §M14.30 K5): the format's test
//! vectors, which every driver checks (`drivers/encrypted-columns.txt`;
//! `NUCLEUS_WRITE_VECTORS=1` writes them again), and a column through a
//! server that never sees the plaintext.

use nucleus_client::Value;
use nucleus_client::columns::{ColumnKey, Plain};

const VECTORS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../drivers/encrypted-columns.txt"
);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The cases: a column's name, a value, a nonce.
fn cases() -> Vec<(&'static str, Plain, [u8; 24])> {
    let nonce = |seed: u8| std::array::from_fn(|i| seed.wrapping_add(i as u8 * 7));
    vec![
        ("users.email", Plain::from("ann@example.com"), nonce(1)),
        ("users.email", Plain::from("ann@example.com"), nonce(2)),
        ("users.email", Plain::from(""), nonce(3)),
        (
            "users.email",
            Plain::from("Árvíztűrő tükörfúrógép 🦀"),
            nonce(4),
        ),
        ("users.phone", Plain::from("ann@example.com"), nonce(5)),
        ("accounts.balance", Plain::Int(0), nonce(6)),
        ("accounts.balance", Plain::Int(-1), nonce(7)),
        ("accounts.balance", Plain::Int(i64::MAX), nonce(8)),
        ("files.blob", Plain::Bytes(vec![]), nonce(9)),
        ("files.blob", Plain::Bytes((0..=255).collect()), nonce(10)),
        (
            "notes.body",
            Plain::Text(
                "a longer text, past one ChaCha20 block of 64 bytes, so the counter moves on"
                    .into(),
            ),
            nonce(11),
        ),
    ]
}

fn line(master: &[u8; 32], name: &str, v: &Plain, nonce: &[u8; 24]) -> String {
    let k = ColumnKey::new(master, name);
    let (ty, value) = match v {
        Plain::Bytes(b) => ("bytes", hex(b)),
        Plain::Text(s) => ("text", hex(s.as_bytes())),
        Plain::Int(i) => ("int", i.to_string()),
    };
    let value = if value.is_empty() { "-".into() } else { value };
    format!(
        "{name} {ty} {value} {} {} {}",
        hex(nonce),
        hex(&k.encrypt_with_nonce(v.clone(), nonce)),
        hex(&k.blind_index_bytes(v.clone()))
    )
}

#[test]
fn the_format_s_vectors() {
    let master: [u8; 32] = std::array::from_fn(|i| (i as u8) * 3 + 1);
    let mut want = String::from(
        "# nucleus's client-side encrypted columns, format 1 (PLAN.md §M14.30 K5;\n\
         # crates/client/src/columns.rs): test vectors every driver checks.\n\
         # `master` is the master key; each case is a column's name, the value's\n\
         # type (bytes, text, int), the value (hex, `-` when empty; an int in\n\
         # decimal), the nonce, the ciphertext and the blind index (hex).\n",
    );
    want += &format!("master {}\n", hex(&master));
    for (name, v, nonce) in cases() {
        want += &line(&master, name, &v, &nonce);
        want.push('\n');
    }
    if std::env::var_os("NUCLEUS_WRITE_VECTORS").is_some() {
        std::fs::write(VECTORS, &want).unwrap();
    }
    // A Windows checkout may have turned the newlines into CRLF.
    let have = std::fs::read_to_string(VECTORS)
        .expect("the vectors")
        .replace("\r\n", "\n");
    assert_eq!(have, want, "the format changed: its vectors did");
    // And each decrypts back.
    for l in have
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("master"))
    {
        let f: Vec<&str> = l.split(' ').collect();
        let k = ColumnKey::new(&master, f[0]);
        let value = if f[2] == "-" { "" } else { f[2] };
        let v = k.decrypt(&unhex(f[4])).unwrap();
        let expect = match f[1] {
            "bytes" => Plain::Bytes(unhex(value)),
            "text" => Plain::Text(String::from_utf8(unhex(value)).unwrap()),
            _ => Plain::Int(value.parse().unwrap()),
        };
        assert_eq!(v, expect);
    }
}

/// Every file's bytes under `dir` of a simulated platform.
fn files(p: &nucleus_platform::sim::SimPlatform, dir: &std::path::Path, out: &mut Vec<u8>) {
    use nucleus_platform::Platform;
    for name in p.fs().list_dir(dir).unwrap() {
        let path = dir.join(&name);
        if p.fs().list_dir(&path).is_ok() {
            files(p, &path, out);
            continue;
        }
        let f = p
            .fs()
            .open(&path, nucleus_platform::OpenMode::ReadOnly)
            .unwrap();
        let mut bytes = vec![0u8; f.size().unwrap() as usize];
        f.read_exact_at(&mut bytes, 0).unwrap();
        out.extend(bytes);
    }
}

/// A column encrypted by the driver through a server: stored, found by
/// its blind index, decrypted — and no file of the server's holds the
/// plaintext (a column written in the clear beside it does, so the
/// search would see it).
#[test]
fn a_column_the_server_never_reads() {
    use std::sync::Arc;
    let p = Arc::new(nucleus_platform::sim::SimPlatform::new(5));
    let db: Arc<dyn nucleus_kv::Kv> = Arc::new(
        nucleus_kv::Database::open_on(
            p.clone(),
            std::path::Path::new("/db"),
            nucleus_kv::Options::default(),
        )
        .unwrap(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        nucleus_server::serve(listener, db, nucleus_server::Config::default())
    });
    let mut c = nucleus_client::Client::connect(nucleus_client::Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ..nucleus_client::Config::default()
    })
    .unwrap();
    let master = [0x5au8; 32];
    let email = ColumnKey::new(&master, "users.email");
    let balance = ColumnKey::new(&master, "users.balance");
    c.execute(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email BYTEA, email_index BYTEA, \
         balance BYTEA, note TEXT)",
        &[],
    )
    .unwrap();
    c.execute("CREATE INDEX ON users (email_index)", &[])
        .unwrap();
    for (id, who, money) in [
        (1, "planted-ann-7f3a@example.com", 100),
        (2, "planted-bob-7f3a@example.com", -5),
        (3, "planted-ann-7f3a@example.com", 7),
    ] {
        c.execute(
            "INSERT INTO users VALUES ($1, $2, $3, $4, $5)",
            &[
                Value::Int(id),
                email.encrypt(who),
                email.blind_index(who),
                balance.encrypt(money),
                Value::from("clear-note-7f3a"),
            ],
        )
        .unwrap();
    }
    let rows = c
        .rows(
            "SELECT id, email, balance FROM users WHERE email_index = $1 ORDER BY id",
            &[email.blind_index("planted-ann-7f3a@example.com")],
        )
        .unwrap();
    let got: Vec<(i64, Plain, Plain)> = rows
        .iter()
        .map(|r| {
            let Value::Int(id) = r[0] else {
                panic!("{r:?}")
            };
            (
                id,
                email.decrypt_value(&r[1]).unwrap().unwrap(),
                balance.decrypt_value(&r[2]).unwrap().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                1,
                Plain::from("planted-ann-7f3a@example.com"),
                Plain::Int(100)
            ),
            (
                3,
                Plain::from("planted-ann-7f3a@example.com"),
                Plain::Int(7)
            ),
        ]
    );
    let mut all = Vec::new();
    files(&p, std::path::Path::new("/db"), &mut all);
    let has = |n: &[u8]| all.windows(n.len()).any(|w| w == n);
    assert!(
        has(b"clear-note-7f3a"),
        "the search sees what is in the clear"
    );
    assert!(!has(b"planted-ann-7f3a") && !has(b"planted-bob-7f3a"));
}
