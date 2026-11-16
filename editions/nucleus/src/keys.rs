//! The key hierarchy's options and `nucleus keys` (PLAN.md §M14.30 K1):
//! where the root key is, what becomes of the key files of before, and
//! whether backups and new databases are encrypted without being asked —
//! the same options for `nucleus server`, `nucleus sql <dir>` and
//! `nucleus keys`; and the database opened under its data key `data`
//! when it is encrypted (K2e).

use std::path::PathBuf;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_server::keyring::{self, Keyring, OldFiles, Root, SecretsKey, Settings};
use nucleus_server::subject_keyfile::SubjectKeyFile;

/// The options' usage, for the commands' help.
pub const OPTIONS: &str = "[--root-key <file> | --root-key-env <var> | --root-passphrase-env <var>
                      | --root-kms aws-kms:<key id, ARN or alias/…>
                      | --root-kms vault-transit:[<mount>/]<key>
                      | --root-kms gcp-kms:projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>
                      | --root-kms azure-kv:<vault>/<key>]
                     [--old-key-files remove|keep|manual] [--encrypt-backups yes|no]
                     [--encrypt-data yes|no]";

/// The key options given.
#[derive(Default)]
pub struct KeyOptions {
    root: Option<Root>,
    old_files: Option<OldFiles>,
    encrypt_backups: Option<bool>,
    encrypt_data: Option<bool>,
}

impl KeyOptions {
    /// Takes option `a` (and its value from `rest`) when it is one of
    /// these: `Ok(true)`.
    pub fn take<'a>(
        &mut self,
        a: &str,
        rest: &mut impl Iterator<Item = &'a String>,
    ) -> Result<bool, String> {
        self.take_with(a, &mut |what| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs {what}"))
        })
    }

    /// [`take`](Self::take), its value from `value`.
    pub fn take_with(
        &mut self,
        a: &str,
        value: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<bool, String> {
        let root = match a {
            "--root-key" => Root::File(value("a file")?.into()),
            "--root-key-env" => Root::Env(value("a variable's name")?),
            "--root-passphrase-env" => Root::Passphrase(value("a variable's name")?),
            "--root-kms" => Root::Kms(value(
                "a KMS key (aws-kms:, vault-transit:, gcp-kms:, azure-kv:<key>)",
            )?),
            "--old-key-files" => {
                self.old_files = Some(match value("remove, keep or manual")?.as_str() {
                    "remove" => OldFiles::Remove,
                    "keep" => OldFiles::Keep,
                    "manual" => OldFiles::Manual,
                    v => return Err(format!("--old-key-files {v}: remove, keep or manual")),
                });
                return Ok(true);
            }
            "--encrypt-backups" | "--encrypt-data" => {
                let yes = match value("yes or no")?.as_str() {
                    "yes" => true,
                    "no" => false,
                    v => return Err(format!("{a} {v}: yes or no")),
                };
                if a == "--encrypt-data" {
                    self.encrypt_data = Some(yes);
                } else {
                    self.encrypt_backups = Some(yes);
                }
                return Ok(true);
            }
            _ => return Ok(false),
        };
        if self.root.is_some() {
            return Err(
                "one of --root-key, --root-key-env, --root-passphrase-env and --root-kms".into(),
            );
        }
        self.root = Some(root);
        Ok(true)
    }

    /// The settings for data directory `dir`: the root given, else
    /// `NUCLEUS_ROOT_KEY_FILE`'s, else the database-managed one.
    pub fn settings(&self, dir: &str) -> Settings {
        let mut s = Settings::default_for(dir);
        if let Some(r) = self.root.clone().or_else(|| {
            std::env::var("NUCLEUS_ROOT_KEY_FILE")
                .ok()
                .map(|f| Root::File(f.into()))
        }) {
            s.root = r;
        }
        if let Some(o) = self.old_files {
            s.old_files = o;
        }
        s.encrypt_backups = self.encrypt_backups;
        s.encrypt_data = self.encrypt_data;
        s
    }
}

/// Opens the database in directory `dir` with `opts` (PLAN.md §M14.30
/// K2e): under the keyring's data key `data` when it has one — an
/// encryption cut short (`nucleus keys <dir> encrypt`) finished first —,
/// and a new database made encrypted, under a new data key `data`, when
/// `settings` say ([`Settings::encrypt_new_data`]).  An existing database
/// in the clear stays so until `nucleus keys <dir> encrypt`.
pub fn open_database(
    dir: &str,
    settings: &Settings,
    mut opts: nucleus_kv::Options,
) -> Result<nucleus_kv::Database, String> {
    let platform: Arc<dyn nucleus_platform::Platform> =
        Arc::new(nucleus_platform::real::RealPlatform::new().map_err(|e| format!("{dir}: {e}"))?);
    let ring = Keyring::before_open(dir, settings.clone(), platform);
    let encrypted = nucleus_kv::is_encrypted(dir).map_err(|e| format!("{dir}: {e}"))?;
    let key = if ring.has("data")? {
        ring.dek("data", false)?
    } else if encrypted.is_none() && settings.encrypt_new_data() {
        let key = ring.dek("data", true)?;
        nucleus_platform::log!("nucleus: {dir} is made encrypted (its data key `data`)");
        key
    } else {
        None
    };
    if let Some(k) = key {
        let cipher = nucleus_kv::PageCipher::new(*k.bytes());
        if encrypted == Some(false) {
            nucleus_platform::log!("nucleus: {dir}: an encryption begun before is finished");
            nucleus_kv::encrypt(dir, &opts, &cipher).map_err(|e| format!("{dir}: {e}"))?;
        }
        opts.encryption_key = Some(cipher);
    }
    nucleus_kv::Database::open(dir, opts).map_err(|e| format!("{dir}: {e}"))
}

/// Installs the key hierarchy of data directory `dir` for this process:
/// the keyring as the custody, the secrets' key from it — the old key
/// file (`secrets_file`, else `<dir>.secrets-key`) taken in as the owner
/// set —, the backup key set (load it first) taken in, and the subjects'
/// keys (`subject_file`, else `<dir>.subject-keys`) sealed under the
/// keyring.  An explicit root is checked at once.
pub fn install(
    dir: &str,
    kv: &Arc<dyn Kv>,
    settings: Settings,
    secrets_file: Option<PathBuf>,
    subject_file: Option<PathBuf>,
) -> Result<Arc<Keyring>, String> {
    let explicit = !matches!(settings.root, Root::Managed(_));
    let ring = Arc::new(Keyring::new(dir, settings, Arc::clone(kv)));
    if explicit {
        ring.root(true)?;
    }
    let secrets_file = secrets_file.unwrap_or_else(|| keyring::beside(dir, "secrets-key"));
    if ring.adopt("secrets", &secrets_file, true)? {
        nucleus_sql::secrets::set_keyring(Some(Arc::new(SecretsKey(Arc::clone(&ring)))));
    } else {
        // `--old-key-files manual`, not imported yet: the old file as before.
        nucleus_sql::secrets::set_keyring(Some(Arc::new(nucleus_server::keyfile::KeyFile::new(
            secrets_file,
            Arc::clone(kv),
        ))));
    }
    // A backup key given (`--backup-key`, NUCLEUS_BACKUP_KEY_FILE) taken
    // in as the data key `backup` — the user's file, never removed — so
    // the backups made without it later go on with the same key.
    if let Some(k) = nucleus_sql::backup::backup_key()
        && !ring.has("backup")?
    {
        ring.import("backup", &nucleus_crypto::keys::Key::new(*k.bytes()))?;
    }
    let subjects = subject_keys(
        &ring,
        kv,
        subject_file.unwrap_or_else(|| keyring::beside(dir, "subject-keys")),
    )?;
    nucleus_sql::subject_keys::set_subject_keys(Some(subjects.clone()));
    // A rotation of a data key cut short: finished now.
    finish_rotations(&ring, kv, &subjects)?;
    nucleus_sql::keys::set_custody(Some(ring.clone() as Arc<dyn nucleus_sql::keys::Custody>));
    Ok(ring)
}

/// Finishes the rotations of data keys begun before (§M14.30 K1e): what
/// the key being replaced sealed sealed again with the new one, then the
/// old one dropped from the keyring — `secrets` (the secrets' records, in
/// one transaction), `subjects` (the subject keys' table).  Run at start
/// and by `nucleus keys rotate`; each step may be repeated.
fn finish_rotations(
    ring: &Arc<Keyring>,
    kv: &Arc<dyn Kv>,
    subjects: &SubjectKeyFile,
) -> Result<(), String> {
    if ring.rotating("secrets")? {
        let ts = nucleus_sql::datetime::from_system(kv.wall_clock());
        let n = nucleus_sql::datetime::with_now(ts, kv, || -> Result<u64, String> {
            let mut txn = kv
                .begin(nucleus_kv::TxnOptions::default())
                .map_err(|e| e.to_string())?;
            let n = nucleus_sql::secrets::reseal(&mut *txn).map_err(|e| e.message)?;
            txn.commit().map_err(|e| e.to_string())?;
            Ok(n)
        })?;
        ring.retire("secrets")?;
        nucleus_platform::log!(
            "nucleus: the secrets' key rotated: {n} secret{} sealed again",
            if n == 1 { "" } else { "s" }
        );
    }
    if ring.rotating("subjects")? {
        if subjects.in_the_clear()?.is_some() {
            subjects.seal_now()?;
        }
        ring.retire("subjects")?;
        nucleus_platform::log!(
            "nucleus: the subject keys' key rotated: {} sealed again",
            subjects.path().display()
        );
    }
    Ok(())
}

/// The subjects' keys in `file`, sealed under `ring` (§M14.30 K1d): a
/// table in the clear sealed at once — rewritten in place, so no copy in
/// the clear stays — unless the owner keeps the old key files, when it
/// stays in the clear (a copy would keep the keys `FORGET` destroys).
pub fn subject_keys(
    ring: &Arc<Keyring>,
    kv: &Arc<dyn Kv>,
    file: PathBuf,
) -> Result<Arc<SubjectKeyFile>, String> {
    let sealed = SubjectKeyFile::sealed(file.clone(), Arc::clone(kv), Arc::clone(ring));
    if sealed.in_the_clear()? != Some(true) {
        return Ok(Arc::new(sealed));
    }
    if ring.settings().old_files != OldFiles::Remove {
        nucleus_platform::log!(
            "nucleus: warning: {} holds the subjects' keys in the clear; `nucleus keys \
             import` seals them under the keyring",
            file.display()
        );
        return Ok(Arc::new(SubjectKeyFile::new(file, Arc::clone(kv))));
    }
    sealed.seal_now()?;
    nucleus_platform::log!(
        "nucleus: the subjects' keys in {} are sealed under the keyring's key",
        file.display()
    );
    Ok(Arc::new(sealed))
}

/// The usage of `nucleus keys`.
pub const USAGE: &str = "usage: nucleus keys <dir> list|import|encrypt [options]
       nucleus keys <dir> rotate-root --to-root-key <file> | --to-root-key-env <var>
                  | --to-root-passphrase-env <var> | --to-root-kms <scheme>:<key>
                  | --to-managed-root  [options]
       nucleus keys <dir> rotate secrets|subjects|backup  [options]";

/// `nucleus keys <dir> encrypt` (PLAN.md §M14.30 K2e): the database in
/// the clear, the server stopped, encrypted in place under a new data key
/// `data` — made and in the keyring first, so a crash after leaves a
/// database the next start finishes encrypting.
fn encrypt(dir: &str, settings: Settings) -> Result<(), String> {
    match nucleus_kv::is_encrypted(dir).map_err(|e| format!("{dir}: {e}"))? {
        None => return Err(format!("{dir}: no database there")),
        Some(true) => return Err(format!("{dir} is encrypted already")),
        Some(false) => {}
    }
    let platform: Arc<dyn nucleus_platform::Platform> =
        Arc::new(nucleus_platform::real::RealPlatform::new().map_err(|e| format!("{dir}: {e}"))?);
    let ring = Keyring::before_open(dir, settings, platform);
    let key = ring
        .dek("data", true)?
        .ok_or_else(|| format!("{dir}: no data key could be made"))?;
    let cipher = nucleus_kv::PageCipher::new(*key.bytes());
    let pages = nucleus_kv::encrypt(dir, &nucleus_kv::Options::default(), &cipher)
        .map_err(|e| format!("{dir}: {e}"))?;
    println!(
        "{dir}: encrypted — {pages} pages sealed under the data key `data`, its logs started \
         again; the file in the clear is replaced (its old blocks are the file system's)"
    );
    Ok(())
}

/// `nucleus keys <dir> list | import | encrypt | rotate-root | rotate
/// [options]`.
pub fn run(args: &[String]) -> Result<(), String> {
    let usage = format!("{USAGE}\noptions: {OPTIONS}");
    let mut opts = KeyOptions::default();
    let mut to = None::<Root>;
    let mut positional = Vec::new();
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        if opts.take(a, &mut rest)? {
            continue;
        }
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value"))
        };
        let new = match a.as_str() {
            "--to-root-key" => Some(Root::File(value()?.into())),
            "--to-root-key-env" => Some(Root::Env(value()?)),
            "--to-root-passphrase-env" => Some(Root::Passphrase(value()?)),
            "--to-root-kms" => Some(Root::Kms(value()?)),
            "--to-managed-root" => Some(Root::Managed(PathBuf::new())),
            _ => None,
        };
        if let Some(r) = new {
            if to.replace(r).is_some() {
                return Err("one new root (--to-…)".into());
            }
            continue;
        }
        if a.starts_with("--") {
            return Err(format!("keys: unknown option {a}\n{usage}"));
        }
        positional.push(a.clone());
    }
    let (dir, what, purpose) = match positional.as_slice() {
        [dir, what] => (dir, what, None),
        [dir, what, purpose] if what == "rotate" => (dir, what, Some(purpose.as_str())),
        _ => return Err(usage),
    };
    if (what == "rotate-root") != to.is_some() {
        return Err(usage);
    }
    let mut settings = opts.settings(dir);
    if what == "encrypt" {
        return encrypt(dir, settings);
    }
    // The database opened: its lock keeps a server from running beside.
    let db = open_database(dir, &settings, nucleus_kv::Options::default())?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    match what.as_str() {
        "list" => {
            let ring = Keyring::new(dir, settings, kv);
            for k in ring.list()? {
                println!(
                    "{:8} {:5} {} {} {}",
                    k.name, k.role, k.id, k.custody, k.location
                );
            }
            Ok(())
        }
        "import" => {
            // The old files taken in now, and removed (unless kept).
            if settings.old_files == OldFiles::Manual {
                settings.old_files = OldFiles::Remove;
            }
            let ring = Arc::new(Keyring::new(dir, settings, Arc::clone(&kv)));
            let file = keyring::beside(dir, "secrets-key");
            if file.exists() {
                ring.adopt("secrets", &file, true)?;
                println!("secrets: {} taken in", file.display());
            } else {
                println!("secrets: no old file");
            }
            let file = keyring::beside(dir, "subject-keys");
            let f = SubjectKeyFile::sealed(file.clone(), kv, Arc::clone(&ring));
            if f.in_the_clear()? == Some(true) {
                f.seal_now()?;
                println!("subject keys: {} sealed", file.display());
            } else {
                println!("subject keys: nothing in the clear");
            }
            Ok(())
        }
        "rotate-root" => {
            let ring = Keyring::new(dir, settings, kv);
            let to = match to.expect("checked") {
                Root::Managed(_) => Root::Managed(keyring::beside(dir, "root-key")),
                r => r,
            };
            let done = ring.rotate_root_to(&to)?;
            println!(
                "root: {} → {} ({}); the data keys wrapped again",
                done.old, done.new, done.describe
            );
            if let Some(f) = &done.removed {
                println!("the old root's file {} is removed", f.display());
            }
            if let Some(f) = &done.retired {
                println!(
                    "the old root is kept as {}: the backups made before carry their keys \
                     wrapped by it — keep it as long as them",
                    f.display()
                );
            } else if !matches!(ring.settings().root, Root::Managed(_)) {
                println!(
                    "the old root ({}) is yours: backups made before carry their keys wrapped by \
                     it — keep it as long as them",
                    done.old
                );
            }
            println!("start the database with the new root from now on");
            Ok(())
        }
        "rotate" => {
            let purpose = purpose.expect("matched");
            let ring = Arc::new(Keyring::new(dir, settings, Arc::clone(&kv)));
            let began = match purpose {
                "secrets" | "subjects" => ring.rotate_dek(purpose, true)?,
                // A backup chain starts again (a full backup) with the new
                // key; the backups before carry theirs, wrapped by the root.
                "backup" => ring.rotate_dek(purpose, false)?,
                _ => return Err(usage),
            };
            if !began {
                println!("{purpose}: a rotation begun before is finished first");
            }
            nucleus_sql::secrets::set_keyring(Some(Arc::new(SecretsKey(Arc::clone(&ring)))));
            let subjects = SubjectKeyFile::sealed(
                keyring::beside(dir, "subject-keys"),
                Arc::clone(&kv),
                Arc::clone(&ring),
            );
            let r = finish_rotations(&ring, &kv, &subjects);
            nucleus_sql::secrets::set_keyring(None);
            r?;
            println!(
                "{purpose}: a new key{}",
                if purpose == "backup" {
                    " — the next backup is a full one"
                } else {
                    ", what the old one sealed sealed again"
                }
            );
            Ok(())
        }
        _ => Err(usage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rotation of the secrets' key: every secret sealed again with the
    /// new key, its value the same; a rotation cut short still opens them.
    #[test]
    fn the_secrets_are_sealed_again_after_a_rotation() {
        let d = std::env::temp_dir().join(format!("nucleus-rotate-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let kv: Arc<dyn Kv> =
            Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap());
        let ring = Arc::new(Keyring::new(db, Settings::default_for(db), Arc::clone(&kv)));
        nucleus_sql::secrets::set_keyring(Some(Arc::new(SecretsKey(Arc::clone(&ring)))));
        let sealed_with = |kv: &Arc<dyn Kv>| {
            let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
            nucleus_sql::secrets::get(&*txn, "api")
                .unwrap()
                .map(|s| s.source)
        };
        let reveal = |kv: &Arc<dyn Kv>| {
            let txn = kv.begin(nucleus_kv::TxnOptions::read_only()).unwrap();
            nucleus_sql::secrets::reveal(&*txn, "api")
                .unwrap()
                .expose()
                .to_string()
        };
        nucleus_sql::datetime::with_now(0, &kv, || {
            let mut txn = kv.begin(nucleus_kv::TxnOptions::default()).unwrap();
            let v = nucleus_sql::secrets::Given::Value("Badacsony".into());
            nucleus_sql::secrets::create(&mut *txn, "api", &v, false).unwrap();
            txn.commit().unwrap();
        });
        let before = sealed_with(&kv);
        assert!(ring.rotate_dek("secrets", true).unwrap());
        // Cut short: the old key still opens it.
        assert_eq!(reveal(&kv), "Badacsony");
        let subjects = SubjectKeyFile::sealed(
            d.join("db.subject-keys"),
            Arc::clone(&kv),
            Arc::clone(&ring),
        );
        finish_rotations(&ring, &kv, &subjects).unwrap();
        assert!(!ring.rotating("secrets").unwrap());
        assert_ne!(sealed_with(&kv), before);
        assert_eq!(reveal(&kv), "Badacsony");
        nucleus_sql::secrets::set_keyring(None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
