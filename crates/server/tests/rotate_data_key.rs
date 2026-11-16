//! `ROTATE KEY data` with the database in use, finished by the server's
//! maintenance step (PLAN.md §M14.30 K3f): writes go on between the
//! batches sealed again, the keyring holds both keys until the last
//! page is done and then the new one alone, which then opens the
//! database by itself.

use std::sync::Arc;

use nucleus_kv::{Kv, Options, PageCipher, Rekey};
use nucleus_server::keyring::{Keyring, Settings};
use nucleus_sql::Session;

fn open(dir: &str, key: [u8; 32], previous: Option<[u8; 32]>) -> Arc<dyn Kv> {
    let opts = Options {
        encryption_key: Some(PageCipher::new(key)),
        previous_encryption_key: previous.map(PageCipher::new),
        ..Options::default()
    };
    Arc::new(nucleus_kv::Database::open(dir, opts).unwrap())
}

#[test]
fn the_data_key_rotated_online() {
    let d = std::env::temp_dir().join(format!("nucleus-rotate-online-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let dir = d.join("db");
    let dir = dir.to_str().unwrap();

    // The keyring's data key made first, as the edition does.
    let platform: Arc<dyn nucleus_platform::Platform> =
        Arc::new(nucleus_platform::real::RealPlatform::new().unwrap());
    let before = Keyring::before_open(dir, Settings::default_for(dir), platform);
    let first = *before.dek("data", true).unwrap().unwrap().bytes();
    drop(before);
    let kv = open(dir, first, None);
    let ring = Arc::new(Keyring::new(
        dir,
        Settings::default_for(dir),
        Arc::clone(&kv),
    ));
    nucleus_sql::keys::set_custody(Some(ring.clone() as Arc<dyn nucleus_sql::keys::Custody>));

    let mut s = Session::new(Arc::clone(&kv));
    s.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    s.execute("INSERT INTO t SELECT g, repeat('x', 200) || g FROM generate_series(1, 20000) g")
        .unwrap();
    s.execute("ROTATE KEY data").unwrap();
    assert!(ring.rotating("data").unwrap());
    assert!(matches!(kv.rekey_state(), Rekey::Running { .. }));
    let e = s.execute("ROTATE KEY data").unwrap_err();
    assert!(e.message.contains("still running"), "{}", e.message);
    let e = s.execute("BEGIN; ROTATE KEY data").unwrap_err();
    assert!(e.message.contains("transaction block"), "{}", e.message);
    let _ = s.execute("ROLLBACK");

    // The maintenance step, a write between its batches.
    let mut n = 20000;
    let mut writer = Session::new(Arc::clone(&kv));
    let mut go_on = || {
        n += 1;
        writer
            .execute(&format!("INSERT INTO t VALUES ({n}, 'during')"))
            .unwrap();
        // The keyring keeps the old key while pages are left.
        assert!(ring.rotating("data").unwrap());
        // The search for erased bytes reads pages under either key.
        if n == 20002 {
            let found = kv.find_bytes(&[b"xxxxxxxxxx1999".to_vec()]).unwrap();
            assert!(found.is_some_and(|f| f > 0), "{found:?}");
        }
        true
    };
    assert!(nucleus_server::reseal(&kv, &mut go_on).unwrap());
    assert!(n > 20001, "one batch only: {n}");
    assert_eq!(kv.rekey_state(), Rekey::Idle);
    assert!(!ring.rotating("data").unwrap(), "the old key kept");
    let second = *ring.dek("data", false).unwrap().unwrap().bytes();
    assert_ne!(second, first);
    let found = kv.find_bytes(&[b"xxxxxxxxxx1999".to_vec()]).unwrap();
    assert!(found.is_some_and(|f| f > 0), "after: {found:?}");
    nucleus_sql::keys::set_custody(None);
    drop((s, writer, ring));
    drop(kv);

    // The new key alone opens it; the old one does not.
    let count = |kv: &Arc<dyn Kv>| {
        let mut s = Session::new(Arc::clone(kv));
        format!("{:?}", s.execute("SELECT count(*) FROM t").unwrap())
    };
    let kv = open(dir, second, None);
    assert!(count(&kv).contains(&n.to_string()), "{}", count(&kv));
    drop(kv);
    assert!(
        nucleus_kv::Database::open(
            dir,
            Options {
                encryption_key: Some(PageCipher::new(first)),
                ..Options::default()
            }
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(&d);
}
