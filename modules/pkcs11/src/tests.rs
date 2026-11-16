use super::*;
use cryptoki::session::Session;
use nucleus_server::keyring::{Keyring, ProviderHandle, Root, Settings};
use std::process::Command;

#[test]
fn uris() {
    let n = parse(
        "pkcs11:token=my%20token;object=root;id=%01%ff;type=secret-key?module-path=/lib/x.so&pin-source=file:/p&x-vendor=1",
    )
    .unwrap();
    assert_eq!(n.token.as_deref(), Some("my token"));
    assert_eq!(n.object.as_deref(), Some("root"));
    assert_eq!(n.id, Some(vec![1, 0xff]));
    assert_eq!(n.module.as_deref(), Some("/lib/x.so"));
    assert_eq!(n.pin_source.as_deref(), Some("file:/p"));
    let n = parse("slot-id=3;id=%2a").unwrap();
    assert_eq!((n.slot, n.id), (Some(3), Some(vec![0x2a])));
    for (bad, says) in [
        ("token=t", "names no key"),
        ("token=t;objet=k", "no path attribute objet"),
        ("object=k?pin=1", "no query attribute pin"),
        ("object=k;type=private", "secret key"),
        ("object=%zz", "two hex digits"),
        ("object", "has no value"),
        ("slot-id=x;object=k", "not a number"),
    ] {
        let e = parse(bad).unwrap_err();
        assert!(e.contains(says), "{bad}: {e}");
    }
    assert_eq!(pct_encode(b"a b/c;d"), "a%20b%2fc%3bd");
}

/// The token's PINs in the tests.
const PIN: &str = "5678";
const SO_PIN: &str = "1234";

/// A SoftHSM2 token for the test `test`.  SoftHSM reads its
/// configuration's place from the process's environment when the
/// library initializes, which a test cannot set (the module forbids the
/// unsafe `set_var`): so the test runs again in a child process with
/// `SOFTHSM2_CONF` naming a token directory of its own, made here with
/// `softhsm2-util`.  The child gets the library's path; this side, after
/// the child, gets `None`.  Without SoftHSM2 the test is skipped — but
/// fails where `NUCLEUS_TEST_SOFTHSM_REQUIRED` is set (CI's kms job).
fn softhsm(test: &str) -> Option<PathBuf> {
    if let Ok(lib) = std::env::var("NUCLEUS_TEST_SOFTHSM_LIB") {
        return Some(PathBuf::from(lib));
    }
    let lib = std::env::var("SOFTHSM2_MODULE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            [
                "/usr/lib/softhsm/libsofthsm2.so",
                "/usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so",
                "/usr/lib/aarch64-linux-gnu/softhsm/libsofthsm2.so",
                "/opt/homebrew/lib/softhsm/libsofthsm2.so",
                "/usr/local/lib/softhsm/libsofthsm2.so",
            ]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
        });
    let Some(lib) = lib else {
        assert!(
            std::env::var("NUCLEUS_TEST_SOFTHSM_REQUIRED").is_err(),
            "SoftHSM2 is not installed (NUCLEUS_TEST_SOFTHSM_REQUIRED)"
        );
        eprintln!("{test}: skipped, no SoftHSM2 (brew install softhsm, apt install softhsm2)");
        return None;
    };
    let dir = std::env::temp_dir().join(format!(
        "nucleus-pkcs11-{}-{}",
        test.replace("::", "-"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    let conf = dir.join("softhsm2.conf");
    std::fs::write(
        &conf,
        format!(
            "directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n",
            dir.join("tokens").display()
        ),
    )
    .unwrap();
    let util = Command::new("softhsm2-util")
        .args(["--init-token", "--free", "--label", "nucleus"])
        .args(["--so-pin", SO_PIN, "--pin", PIN])
        .env("SOFTHSM2_CONF", &conf)
        .output()
        .expect("softhsm2-util not run");
    assert!(util.status.success(), "{util:?}");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("SOFTHSM2_CONF", &conf)
        .env("NUCLEUS_TEST_SOFTHSM_LIB", &lib)
        .status()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(child.success(), "{test} failed in its child process");
    None
}

/// A user session on the test token, for making and taking keys.
fn admin(lib: &Path) -> Session {
    let ctx = context(lib).unwrap();
    let slot = ctx.get_slots_with_token().unwrap()[0];
    let s = ctx.open_rw_session(slot).unwrap();
    match s.login(UserType::User, Some(&AuthPin::from(PIN.to_string()))) {
        Ok(()) | Err(P11Error::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => {}
        Err(e) => panic!("{e}"),
    }
    s
}

/// Makes a secret key in the token: AES of `len` bytes, or a generic
/// secret's when `aes` is false.
fn make_key(s: &Session, label: &str, id: &[u8], aes: bool, len: u64, decrypt: bool) {
    let mut t = vec![
        Attribute::Token(true),
        Attribute::Label(label.as_bytes().to_vec()),
        Attribute::Id(id.to_vec()),
        Attribute::ValueLen(len.into()),
        Attribute::Sensitive(true),
        Attribute::Extractable(false),
    ];
    if aes {
        t.extend([Attribute::Encrypt(true), Attribute::Decrypt(decrypt)]);
    }
    let m = if aes {
        Mechanism::AesKeyGen
    } else {
        Mechanism::GenericSecretKeyGen
    };
    s.generate_key(&m, &t).unwrap();
}

fn env_for(lib: &Path) -> impl Fn(&str) -> Option<String> + use<> {
    let lib = lib.display().to_string();
    move |k| match k {
        "NUCLEUS_PKCS11_MODULE" => Some(lib.clone()),
        "NUCLEUS_PKCS11_PIN" => Some(PIN.to_string()),
        _ => None,
    }
}

/// A data key wrapped and unwrapped by the token, for its purpose only;
/// one URI or another for the same key, the same root id; keys that are
/// not AES-256 or cannot decrypt, a wrong PIN, no PIN, a token not
/// there — each refused by name; the key destroyed, refused.
#[test]
fn data_keys_in_a_token() {
    let Some(lib) = softhsm("tests::data_keys_in_a_token") else {
        return;
    };
    let a = admin(&lib);
    make_key(&a, "root", &[1], true, 32, true);
    make_key(&a, "short", &[2], true, 16, true);
    make_key(&a, "seal-only", &[3], true, 32, false);
    make_key(&a, "hmac", &[4], false, 32, true);
    let env = env_for(&lib);

    let k = Pkcs11Key::new("pkcs11:token=nucleus;object=root", &env).unwrap();
    assert_eq!(k.id(), "pkcs11:token=nucleus;object=root;id=%01");
    assert_eq!(k.kind(), "kms:pkcs11");
    assert_eq!(Pkcs11Key::new("id=%01", &env).unwrap().id(), k.id());
    let path = lib.display();
    let pin = std::env::temp_dir().join(format!("nucleus-pkcs11-pin-{}", std::process::id()));
    std::fs::write(&pin, format!("{PIN}\n")).unwrap();
    let full = format!(
        "object=root?module-path={path}&pin-source=file:{}",
        pin.display()
    );
    assert_eq!(Pkcs11Key::new(&full, |_| None).unwrap().id(), k.id());
    let _ = std::fs::remove_file(&pin);

    let dek = Key::new([0x42; 32]);
    let blob = k.wrap("data", &dek).unwrap();
    assert_eq!(blob.len(), BLOB_LEN);
    assert_ne!(k.wrap("data", &dek).unwrap(), blob, "the IV is not fresh");
    assert_eq!(k.unwrap("data", &blob).unwrap().bytes(), dek.bytes());
    let e = k.unwrap("backup", &blob).unwrap_err();
    assert!(e.contains("C_Decrypt"), "{e}");
    let mut bent = blob.clone();
    bent[20] ^= 1;
    assert!(k.unwrap("data", &bent).is_err());
    assert!(k.unwrap("data", b"garbage").is_err());

    for (uri, says) in [
        ("object=short", "AES-128 key, not AES-256"),
        ("object=seal-only", "cannot both encrypt and decrypt"),
        ("object=hmac", "not an AES key"),
        ("object=none", "no secret key object=none"),
        ("token=other;object=root", "no token token=other"),
    ] {
        let e = Pkcs11Key::new(uri, &env)
            .err()
            .unwrap_or_else(|| panic!("{uri} opened"));
        assert!(e.contains(says), "{uri}: {e}");
    }
    let e = Pkcs11Key::new("object=root?module-path=/no/such/lib.so", &env)
        .err()
        .unwrap();
    assert!(e.contains("not loaded"), "{e}");

    // The key destroyed: the kept handle stale, the key not found anew.
    let root = a
        .find_objects(&[Attribute::Label(b"root".to_vec())])
        .unwrap();
    a.destroy_object(root[0]).unwrap();
    let e = k.unwrap("data", &blob).unwrap_err();
    assert!(e.contains("no secret key object=root"), "{e}");

    // Logged out (a login is the process's, every session's): no PIN
    // finds no key, a wrong one is refused.
    a.logout().unwrap();
    let no_pin = |k: &str| (k != "NUCLEUS_PKCS11_PIN").then(|| env(k)).flatten();
    let e = Pkcs11Key::new("object=short", no_pin).err().unwrap();
    assert!(
        e.contains("no secret key object=short (no PIN given"),
        "{e}"
    );
    let e = Pkcs11Key::new("object=short?pin-value=0000", &env)
        .err()
        .unwrap();
    assert!(e.contains("C_Login: PinIncorrect"), "{e}");
}

struct Handle(Arc<dyn KeyProvider>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.describe())
    }
}

impl ProviderHandle for Handle {
    fn provider(&self) -> Arc<dyn KeyProvider> {
        Arc::clone(&self.0)
    }
}

/// A keyring under a token's root: its data keys only wrapped in the
/// file, read back by another keyring.
#[test]
fn a_keyring_under_a_token_root() {
    let Some(lib) = softhsm("tests::a_keyring_under_a_token_root") else {
        return;
    };
    make_key(&admin(&lib), "ring", b"r", true, 32, true);
    let d = std::env::temp_dir().join(format!("nucleus-pkcs11-ring-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let db = d.join("db");
    let db = db.to_str().unwrap();
    let kv = || -> Arc<dyn nucleus_kv::Kv> {
        Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
    };
    let settings = || Settings {
        root: Root::Provider(Arc::new(Handle(Arc::new(
            Pkcs11Key::new("object=ring", env_for(&lib)).unwrap(),
        )))),
        ..Settings::default_for(db)
    };
    let ring = Keyring::new(db, settings(), kv());
    let dek = ring.dek("data", true).unwrap().unwrap();
    let file = std::fs::read_to_string(format!("{db}.keyring")).unwrap();
    assert!(
        file.contains("pkcs11:token=nucleus;object=ring;id=%72"),
        "{file}"
    );
    assert!(!file.contains(&dek.to_hex()), "the data key in the clear");
    assert!(!std::path::Path::new(&format!("{db}.root-key")).exists());

    let held = Keyring::new(db, settings(), kv());
    assert_eq!(
        held.dek("data", false).unwrap().unwrap().bytes(),
        dek.bytes()
    );
    let _ = std::fs::remove_dir_all(&d);
}
