//! The key hierarchy's options and `nucleus keys` (PLAN.md §M14.30 K1):
//! where the root key is, what becomes of the key files of before, and
//! whether backups are encrypted without being asked — the same options
//! for `nucleus server`, `nucleus sql <dir>` and `nucleus keys`.

use std::path::PathBuf;
use std::sync::Arc;

use nucleus_kv::Kv;
use nucleus_server::keyring::{self, Keyring, OldFiles, Root, SecretsKey, Settings};

/// The options' usage, for the commands' help.
pub const OPTIONS: &str = "[--root-key <file> | --root-key-env <var> | --root-passphrase-env <var>]
                     [--old-key-files remove|keep|manual] [--encrypt-backups yes|no]";

/// The key options given.
#[derive(Default)]
pub struct KeyOptions {
    root: Option<Root>,
    old_files: Option<OldFiles>,
    encrypt_backups: Option<bool>,
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
            "--old-key-files" => {
                self.old_files = Some(match value("remove, keep or manual")?.as_str() {
                    "remove" => OldFiles::Remove,
                    "keep" => OldFiles::Keep,
                    "manual" => OldFiles::Manual,
                    v => return Err(format!("--old-key-files {v}: remove, keep or manual")),
                });
                return Ok(true);
            }
            "--encrypt-backups" => {
                self.encrypt_backups = Some(match value("yes or no")?.as_str() {
                    "yes" => true,
                    "no" => false,
                    v => return Err(format!("--encrypt-backups {v}: yes or no")),
                });
                return Ok(true);
            }
            _ => return Ok(false),
        };
        if self.root.is_some() {
            return Err("one of --root-key, --root-key-env and --root-passphrase-env".into());
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
        s
    }
}

/// Installs the key hierarchy of data directory `dir` for this process:
/// the keyring as the custody, the secrets' key from it — the old key
/// file (`secrets_file`, else `<dir>.secrets-key`) taken in as the owner
/// set.  An explicit root is checked at once.
pub fn install(
    dir: &str,
    kv: &Arc<dyn Kv>,
    settings: Settings,
    secrets_file: Option<PathBuf>,
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
    nucleus_sql::keys::set_custody(Some(ring.clone() as Arc<dyn nucleus_sql::keys::Custody>));
    Ok(ring)
}

/// `nucleus keys <dir> list | import [options]`.
pub fn run(args: &[String]) -> Result<(), String> {
    let usage = format!("usage: nucleus keys <dir> list|import {OPTIONS}");
    let mut opts = KeyOptions::default();
    let mut positional = Vec::new();
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        if opts.take(a, &mut rest)? {
            continue;
        }
        if a.starts_with("--") {
            return Err(format!("keys: unknown option {a}\n{usage}"));
        }
        positional.push(a.clone());
    }
    let [dir, what] = positional.as_slice() else {
        return Err(usage);
    };
    // The database opened: its lock keeps a server from running beside.
    let db = nucleus_kv::Database::open(dir, nucleus_kv::Options::default())
        .map_err(|e| format!("{dir}: {e}"))?;
    let kv: Arc<dyn Kv> = Arc::new(db);
    let mut settings = opts.settings(dir);
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
            let ring = Keyring::new(dir, settings, kv);
            let file = keyring::beside(dir, "secrets-key");
            if file.exists() {
                ring.adopt("secrets", &file, true)?;
                println!("secrets: {} taken in", file.display());
            } else {
                println!("secrets: no old file");
            }
            Ok(())
        }
        _ => Err(usage),
    }
}
