//! Secrets (PLAN.md §M14.22 E2a): sealed under a key kept apart from the
//! data, never shown, used by those granted `USAGE`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nucleus_kv::{Database, Kv, Options, TxnOptions};
use nucleus_platform::sim::SimPlatform;
use nucleus_sql::Session;
use nucleus_sql::secrets::{self, StaticKey};
use nucleus_sql::session::Output;
use nucleus_sql::types::Value;

/// The keyring is the process's: tests that change it take turns.
static KEYRING: Mutex<()> = Mutex::new(());

fn db() -> Arc<dyn Kv> {
    Arc::new(
        Database::open_on(
            Arc::new(SimPlatform::new(11)),
            Path::new("/db"),
            Options::default(),
        )
        .unwrap(),
    )
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s.execute(sql).unwrap().pop().unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn code(s: &mut Session, sql: &str) -> String {
    s.execute(sql).unwrap_err().code.to_string()
}

fn reveal(kv: &Arc<dyn Kv>, name: &str) -> nucleus_sql::error::Result<String> {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    secrets::reveal(&*txn, name).map(|v| v.expose().to_string())
}

/// Whether `needle` is anywhere in the database's keys or values.
fn stored(kv: &Arc<dyn Kv>, needle: &[u8]) -> bool {
    let txn = kv.begin(TxnOptions::read_only()).unwrap();
    let mut found = false;
    for e in txn
        .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap()
    {
        let (k, v) = e.unwrap();
        found |= k.windows(needle.len()).any(|w| w == needle)
            || v.windows(needle.len()).any(|w| w == needle);
    }
    found
}

fn admin_and_app(kv: &Arc<dyn Kv>) -> (Session, Session) {
    let mut admin = Session::new(kv.clone());
    admin.login("root").unwrap();
    admin
        .execute("CREATE USER app PASSWORD 'app-pw'; CREATE USER other PASSWORD 'o-pw'")
        .unwrap();
    let mut app = Session::new(kv.clone());
    app.login("app").unwrap();
    (admin, app)
}

#[test]
fn secrets_are_sealed_shown_to_none_and_granted() {
    let _turn = KEYRING.lock().unwrap_or_else(|e| e.into_inner());
    secrets::set_keyring(Some(Arc::new(StaticKey([7; 32]))));
    let kv = db();
    let (mut admin, mut app) = admin_and_app(&kv);

    admin
        .execute("CREATE SECRET openai VALUE 'sk-live-0123456789'")
        .unwrap();
    assert_eq!(code(&mut admin, "CREATE SECRET openai VALUE 'x'"), "42710");
    admin
        .execute("CREATE SECRET IF NOT EXISTS openai VALUE 'x'")
        .unwrap();
    assert_eq!(reveal(&kv, "openai").unwrap(), "sk-live-0123456789");
    // Nowhere in the database in the clear: not the record, not the audit
    // log, not the statement statistics.
    assert!(!stored(&kv, b"sk-live-0123456789"));
    assert!(!stored(&kv, b"0123456789"));
    let audit = rows(
        &mut admin,
        "SELECT detail FROM nc_catalog.audit WHERE detail LIKE '%SECRET%'",
    );
    assert_eq!(
        audit,
        vec![
            vec![Value::Text("CREATE SECRET openai VALUE '***'".into())],
            vec![Value::Text("CREATE SECRET openai VALUE '***'".into())],
            vec![Value::Text(
                "CREATE SECRET IF NOT EXISTS openai VALUE '***'".into()
            )],
        ]
    );
    let shown = rows(&mut admin, "SELECT * FROM nc_catalog.statements");
    assert!(!format!("{shown:?}").contains("sk-live"));
    let listed = rows(
        &mut admin,
        "SELECT name, owner, source, env FROM nc_catalog.secrets",
    );
    assert_eq!(
        listed,
        vec![vec![
            Value::Text("openai".into()),
            Value::Text("root".into()),
            Value::Text("value".into()),
            Value::Null,
        ]]
    );

    // Another role: sees none, may not make, change or drop one.
    assert!(rows(&mut app, "SELECT name FROM nc_catalog.secrets").is_empty());
    assert_eq!(code(&mut app, "CREATE SECRET mine VALUE 'x'"), "42501");
    assert_eq!(code(&mut app, "ALTER SECRET openai VALUE 'x'"), "42501");
    assert_eq!(code(&mut app, "DROP SECRET openai"), "42501");
    assert_eq!(code(&mut app, "CREATE SECRET e FROM ENV 'PATH'"), "42501");
    // Granted USAGE: listed (never its value); still not its owner.
    admin
        .execute("GRANT USAGE ON SECRET openai TO app")
        .unwrap();
    assert_eq!(
        rows(&mut app, "SELECT name, source FROM nc_catalog.secrets"),
        vec![vec![
            Value::Text("openai".into()),
            Value::Text("value".into())
        ]]
    );
    assert_eq!(code(&mut app, "ALTER SECRET openai VALUE 'x'"), "42501");
    assert_eq!(
        code(&mut admin, "GRANT USAGE ON SECRET nothing TO app"),
        "42704"
    );
    // With CREATE on the database a role makes its own, which it owns.
    admin
        .execute("GRANT CREATE ON DATABASE nucleus TO app")
        .unwrap();
    app.execute("CREATE SECRET mine VALUE 'app-key'").unwrap();
    app.execute("ALTER SECRET mine VALUE 'app-key-2'").unwrap();
    assert_eq!(reveal(&kv, "mine").unwrap(), "app-key-2");
    assert_eq!(
        code(&mut admin, "DROP ROLE app"),
        "2BP01",
        "a role that owns a secret is not dropped"
    );
    admin.execute("ALTER ROLE app RENAME TO app2").unwrap();
    assert_eq!(
        rows(
            &mut admin,
            "SELECT owner FROM nc_catalog.secrets WHERE name = 'mine'"
        ),
        vec![vec![Value::Text("app2".into())]]
    );

    // An administrator names the server's environment instead.
    admin
        .execute("CREATE SECRET from_env FROM ENV 'PATH'")
        .unwrap();
    assert_eq!(
        reveal(&kv, "from_env").unwrap(),
        std::env::var("PATH").unwrap()
    );
    assert_eq!(
        rows(
            &mut admin,
            "SELECT source, env FROM nc_catalog.secrets WHERE name = 'from_env'"
        ),
        vec![vec![Value::Text("env".into()), Value::Text("PATH".into())]]
    );
    admin
        .execute("CREATE SECRET unset FROM ENV 'NUCLEUS_NO_SUCH_VARIABLE_42'")
        .unwrap();
    assert_eq!(reveal(&kv, "unset").unwrap_err().code, "55000");
    assert_eq!(
        code(&mut admin, "CREATE SECRET bad FROM ENV 'A=B'"),
        "22023"
    );

    // A changed value replaces the old.
    admin
        .execute("ALTER SECRET openai VALUE 'sk-live-rotated'")
        .unwrap();
    assert_eq!(reveal(&kv, "openai").unwrap(), "sk-live-rotated");
    assert!(!stored(&kv, b"sk-live-rotated"));

    // A record changed by a byte does not open.
    {
        let mut txn = kv.begin(TxnOptions::default()).unwrap();
        let key = [&[0x00, 0x14][..], b"openai"].concat();
        let mut v = txn.get(&key).unwrap().unwrap().to_vec();
        *v.last_mut().unwrap() ^= 1;
        txn.put(&key, &v).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(reveal(&kv, "openai").unwrap_err().code, "22000");

    // Another key, or none: refused, never garbage.
    secrets::set_keyring(Some(Arc::new(StaticKey([8; 32]))));
    let e = reveal(&kv, "mine").unwrap_err();
    assert_eq!(e.code, "55000");
    assert!(e.message.contains("another key"), "{}", e.message);
    secrets::set_keyring(None);
    assert_eq!(reveal(&kv, "mine").unwrap_err().code, "55000");
    assert_eq!(code(&mut admin, "CREATE SECRET k VALUE 'x'"), "55000");
    // A secret from the environment needs no key.
    admin.execute("CREATE SECRET env2 FROM ENV 'PATH'").unwrap();
    secrets::set_keyring(Some(Arc::new(StaticKey([7; 32]))));
    assert_eq!(reveal(&kv, "mine").unwrap(), "app-key-2");

    // Dropped, with its grants.
    admin
        .execute("GRANT USAGE ON SECRET mine TO other")
        .unwrap();
    admin.execute("DROP SECRET mine").unwrap();
    assert_eq!(code(&mut admin, "DROP SECRET mine"), "42704");
    admin.execute("DROP SECRET IF EXISTS mine").unwrap();
    admin.execute("CREATE SECRET mine VALUE 'again'").unwrap();
    let mut other = Session::new(kv.clone());
    other.login("other").unwrap();
    assert!(
        rows(&mut other, "SELECT name FROM nc_catalog.secrets").is_empty(),
        "a grant does not outlive the secret"
    );
}

#[test]
fn a_mistyped_secret_is_not_echoed() {
    let kv = db();
    let mut s = Session::new(kv);
    for sql in [
        "CREATE SECRET k 'sk-zzz-oops'",
        "ALTER SECRET k 'sk-zzz-oops'",
    ] {
        let e = s.execute(sql).unwrap_err();
        assert_eq!(e.code, "42601");
        assert!(!e.message.contains("sk-zzz"), "{}", e.message);
    }
}
