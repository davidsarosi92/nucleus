//! The key hierarchy's custody (PLAN.md §M14.30 K1): a root key wraps
//! the data keys, which are kept only wrapped, in the keyring file
//! `<dir>.keyring` beside the data directory.
//!
//! The root's custody is the owner's choice ([`Root`]):
//! - *database-managed* (the default): a file beside the data,
//!   `<dir>.root-key`, made at first use, readable by its owner alone —
//!   guards a copied directory or a stolen backup, not a compromised host;
//! - *user-managed*: a file, an environment variable, or a passphrase
//!   (PBKDF2, its salt in the keyring) the user gives at start — the
//!   database holds only wrapped keys;
//! - a KMS: a module's [`KeyProvider`], whose root never leaves it.
//!
//! The keyring's form, text, one line each:
//!
//! ```text
//! nucleus keyring 1
//! root <root id> <custody>
//! salt <hex> <iterations>                      (a passphrase's root only)
//! dek <purpose> <key id> <wrapped, hex> <made, µs since 1970>
//! ```
//!
//! Each change is a new file, synced and renamed over the old one, the
//! directory synced.  The key files of before (`<dir>.secrets-key`, …)
//! are taken in — their bytes become the data keys, so what they sealed
//! still opens — and then, as the owner set ([`OldFiles`]), removed once
//! the keyring holding them is on disk and read back, kept with a warning
//! at every start, or left for `nucleus keys import`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nucleus_crypto::keys::{self, Key, KeyProvider, LocalRoot};
use nucleus_kv::Kv;
use nucleus_sql::keys::{Custody, KeyInfo};

const HEADER: &str = "nucleus keyring 1";
/// The identifier label of data keys in the keyring's listing.
const DEK_ID_LABEL: &str = "nucleus data key";

/// Where the root key is.
#[derive(Clone, Debug, PartialEq)]
pub enum Root {
    /// A file the database makes at first use (database-managed).
    Managed(PathBuf),
    /// A file the user gives: 64 hex digits.
    File(PathBuf),
    /// An environment variable holding 64 hex digits.
    Env(String),
    /// An environment variable holding a passphrase.
    Passphrase(String),
    /// A key provider of a module's (a KMS).
    Provider(Arc<dyn ProviderHandle>),
}

/// A module's root (a KMS), comparable for [`Root`]'s sake.
pub trait ProviderHandle: Send + Sync + std::fmt::Debug {
    /// The provider.
    fn provider(&self) -> Arc<dyn KeyProvider>;
}

impl PartialEq for dyn ProviderHandle {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self, other)
    }
}

/// What becomes of a key file of before once its key is in the keyring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OldFiles {
    /// Removed, once the keyring holding its key is on disk and read back
    /// (the default).
    Remove,
    /// Kept; every start warns that it is still there.
    Keep,
    /// Not taken in at start: `nucleus keys import` does it (meanwhile the
    /// old file is used as before).
    Manual,
}

/// The owner's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Where the root is.
    pub root: Root,
    /// Old key files.
    pub old_files: OldFiles,
    /// Whether backups are encrypted without being asked; `None`: when the
    /// root was given explicitly (not the database-managed one, which a
    /// lost disk takes with it).
    pub encrypt_backups: Option<bool>,
}

impl Settings {
    /// The defaults for data directory `dir`: a database-managed root.
    pub fn default_for(dir: &str) -> Self {
        Self {
            root: Root::Managed(beside(dir, "root-key")),
            old_files: OldFiles::Remove,
            encrypt_backups: None,
        }
    }
}

/// `<dir>.<what>` beside data directory `dir`.
pub fn beside(dir: &str, what: &str) -> PathBuf {
    let d = dir.trim_end_matches(['/', '\\']);
    PathBuf::from(format!("{}.{what}", if d.is_empty() { dir } else { d }))
}

/// A data key wrapped, as the keyring keeps it.
#[derive(Clone, Debug, PartialEq)]
struct Wrapped {
    id: String,
    blob: Vec<u8>,
    created: i64,
}

/// The keyring file's contents.
#[derive(Clone, Debug, Default, PartialEq)]
struct Contents {
    /// The root's id and custody.
    root: Option<(String, String)>,
    /// A passphrase root's salt and iterations.
    salt: Option<(Vec<u8>, u32)>,
    deks: BTreeMap<String, Wrapped>,
}

impl Contents {
    fn to_text(&self) -> String {
        let mut s = format!("{HEADER}\n");
        if let Some((id, kind)) = &self.root {
            s.push_str(&format!("root {id} {kind}\n"));
        }
        if let Some((salt, n)) = &self.salt {
            s.push_str(&format!("salt {} {n}\n", keys::to_hex(salt)));
        }
        for (purpose, w) in &self.deks {
            s.push_str(&format!(
                "dek {purpose} {} {} {}\n",
                w.id,
                keys::to_hex(&w.blob),
                w.created
            ));
        }
        s
    }

    fn from_text(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(format!(
                "not a keyring (its first line is not \"{HEADER}\")"
            ));
        }
        let mut c = Contents::default();
        for (n, line) in lines.enumerate() {
            let bad = || format!("line {}: {line:?}", n + 2);
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                [] => {}
                ["root", id, kind] => c.root = Some((id.to_string(), kind.to_string())),
                ["salt", salt, iterations] => {
                    c.salt = Some((
                        keys::from_hex(salt).ok_or_else(bad)?,
                        iterations.parse().map_err(|_| bad())?,
                    ))
                }
                ["dek", purpose, id, blob, created] => {
                    c.deks.insert(
                        purpose.to_string(),
                        Wrapped {
                            id: id.to_string(),
                            blob: keys::from_hex(blob).ok_or_else(bad)?,
                            created: created.parse().map_err(|_| bad())?,
                        },
                    );
                }
                _ => return Err(bad()),
            }
        }
        Ok(c)
    }
}

/// Writes `text` to `path` durably and readable by its owner alone: a new
/// file, synced, renamed over the old one, the directory synced.
pub fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut new = path.as_os_str().to_owned();
    new.push(".new");
    let new = PathBuf::from(new);
    let _ = std::fs::remove_file(&new);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&new).map_err(fail)?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(fail)?;
    drop(f);
    std::fs::rename(&new, path).map_err(fail)?;
    sync_dir(path);
    Ok(())
}

fn sync_dir(path: &Path) {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
}

/// A key in a file of 64 hex digits; `None` when there is no file.
pub fn read_key_file(path: &Path) -> Result<Option<Key>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Key::from_hex(&t)
            .map(Some)
            .ok_or_else(|| format!("{} is not 64 hex digits", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// How a variable is read (the process's environment; a test's own).
type Env = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct State {
    contents: Option<Contents>,
    root: Option<Arc<dyn KeyProvider>>,
    /// Data keys unwrapped so far.
    open: BTreeMap<String, Key>,
}

/// The keyring of a data directory, with the custody of its root.
pub struct Keyring {
    path: PathBuf,
    settings: Settings,
    /// For new keys' bytes and the time (the platform's).
    kv: Arc<dyn Kv>,
    env: Env,
    state: Mutex<State>,
}

impl Keyring {
    /// The keyring of data directory `dir`, `<dir>.keyring`.
    pub fn new(dir: &str, settings: Settings, kv: Arc<dyn Kv>) -> Self {
        Self::at(beside(dir, "keyring"), settings, kv)
    }

    /// The keyring in file `path`.
    pub fn at(path: PathBuf, settings: Settings, kv: Arc<dyn Kv>) -> Self {
        Self {
            path,
            settings,
            kv,
            env: Box::new(|v| std::env::var(v).ok()),
            state: Mutex::new(State {
                contents: None,
                root: None,
                open: BTreeMap::new(),
            }),
        }
    }

    /// Reads variables with `env` instead of the process's environment.
    pub fn with_env(
        mut self,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Box::new(env);
        self
    }

    /// The keyring's file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The owner's settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_us(&self) -> i64 {
        self.kv
            .wall_clock()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as i64)
    }

    fn load(&self, st: &mut State) -> Result<(), String> {
        if st.contents.is_none() {
            st.contents = Some(match std::fs::read_to_string(&self.path) {
                Ok(t) => {
                    Contents::from_text(&t).map_err(|e| format!("{}: {e}", self.path.display()))?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Contents::default(),
                Err(e) => return Err(format!("{}: {e}", self.path.display())),
            });
        }
        Ok(())
    }

    fn save(&self, c: &Contents) -> Result<(), String> {
        let first = !self.path.exists();
        write_private(&self.path, &c.to_text())?;
        if first {
            nucleus_platform::log!(
                "nucleus: made the keyring {} — its keys are wrapped by the root key; \
                 it is needed with the root to read what they sealed",
                self.path.display()
            );
        }
        Ok(())
    }

    /// The root's provider, made when `create` and its custody allows.
    fn root_in(
        &self,
        st: &mut State,
        create: bool,
    ) -> Result<Option<Arc<dyn KeyProvider>>, String> {
        if let Some(r) = &st.root {
            return Ok(Some(Arc::clone(r)));
        }
        self.load(st)?;
        let provider: Arc<dyn KeyProvider> = match &self.settings.root {
            Root::Managed(path) => {
                let key = match read_key_file(path)? {
                    Some(k) => k,
                    None if create => self.make_root_file(path)?,
                    None => return Ok(None),
                };
                Arc::new(LocalRoot::new(
                    key,
                    "database",
                    &format!("the file {}", path.display()),
                ))
            }
            Root::File(path) => {
                let key = read_key_file(path)?.ok_or_else(|| {
                    format!("the root key's file {} is not there", path.display())
                })?;
                Arc::new(LocalRoot::new(
                    key,
                    "user",
                    &format!("the file {}", path.display()),
                ))
            }
            Root::Env(var) => {
                let text = (self.env)(var)
                    .ok_or_else(|| format!("the root key's variable {var} is not set"))?;
                let key = Key::from_hex(&text)
                    .ok_or_else(|| format!("the root key's variable {var} is not 64 hex digits"))?;
                Arc::new(LocalRoot::new(key, "user", &format!("the variable {var}")))
            }
            Root::Passphrase(var) => {
                let pass = (self.env)(var)
                    .ok_or_else(|| format!("the root's passphrase variable {var} is not set"))?;
                if pass.is_empty() {
                    return Err(format!("the root's passphrase in {var} is empty"));
                }
                let c = st.contents.as_mut().expect("loaded");
                let (salt, iterations) = match &c.salt {
                    Some(s) => s.clone(),
                    None if create => {
                        let mut salt = vec![0u8; 16];
                        self.kv.random_bytes(&mut salt);
                        let s = (salt, keys::PASSPHRASE_ITERATIONS);
                        c.salt = Some(s.clone());
                        s
                    }
                    None => return Ok(None),
                };
                let key = keys::root_from_passphrase(pass.as_bytes(), &salt, iterations);
                Arc::new(LocalRoot::new(
                    key,
                    "user",
                    &format!("a passphrase (the variable {var})"),
                ))
            }
            Root::Provider(h) => h.provider(),
        };
        let c = st.contents.as_mut().expect("loaded");
        match &c.root {
            Some((id, _)) if *id != provider.id() => {
                return Err(format!(
                    "{} was wrapped by the root key {id}; the root given ({}) is {}",
                    self.path.display(),
                    provider.describe(),
                    provider.id()
                ));
            }
            Some(_) => {}
            None => {
                // The root's first use: recorded (with a passphrase's salt).
                c.root = Some((provider.id(), provider.kind()));
                let c = c.clone();
                self.save(&c)?;
            }
        }
        st.root = Some(Arc::clone(&provider));
        Ok(Some(provider))
    }

    fn make_root_file(&self, path: &Path) -> Result<Key, String> {
        let mut b = [0u8; keys::KEY_LEN];
        self.kv.random_bytes(&mut b);
        let key = Key::new(b);
        b.fill(0);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = match o.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return read_key_file(path)?.ok_or_else(|| format!("{} vanished", path.display()));
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        f.write_all(format!("{}\n", key.to_hex()).as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        sync_dir(path);
        nucleus_platform::log!(
            "nucleus: made the root key {} — keep it, and back it up apart from the data: \
             without it nothing its keys sealed can be read",
            path.display()
        );
        Ok(key)
    }

    fn dek_in(&self, st: &mut State, purpose: &str, create: bool) -> Result<Option<Key>, String> {
        if let Some(k) = st.open.get(purpose) {
            return Ok(Some(k.clone()));
        }
        self.load(st)?;
        let wrapped = st
            .contents
            .as_ref()
            .and_then(|c| c.deks.get(purpose))
            .cloned();
        let key = match wrapped {
            Some(w) => {
                let Some(root) = self.root_in(st, false)? else {
                    return Err(format!(
                        "{} holds the key for {purpose}, but the root key is not there ({})",
                        self.path.display(),
                        self.describe_root()
                    ));
                };
                root.unwrap(purpose, &w.blob)?
            }
            None if create => {
                let mut b = [0u8; keys::KEY_LEN];
                self.kv.random_bytes(&mut b);
                let k = Key::new(b);
                b.fill(0);
                self.add_in(st, purpose, &k)?;
                k
            }
            None => return Ok(None),
        };
        st.open.insert(purpose.to_string(), key.clone());
        Ok(Some(key))
    }

    /// Wraps `key` for `purpose` and writes the keyring (the root made if
    /// its custody allows).
    fn add_in(&self, st: &mut State, purpose: &str, key: &Key) -> Result<(), String> {
        let root = self
            .root_in(st, true)?
            .ok_or_else(|| format!("no root key ({})", self.describe_root()))?;
        let blob = root.wrap(purpose, key)?;
        let w = Wrapped {
            id: key.id(DEK_ID_LABEL),
            blob,
            created: self.now_us(),
        };
        let mut c = st.contents.clone().expect("loaded");
        c.deks.insert(purpose.to_string(), w);
        self.save(&c)?;
        st.contents = Some(c);
        st.open.insert(purpose.to_string(), key.clone());
        Ok(())
    }

    fn describe_root(&self) -> String {
        match &self.settings.root {
            Root::Managed(p) | Root::File(p) => format!("the file {}", p.display()),
            Root::Env(v) => format!("the variable {v}"),
            Root::Passphrase(v) => format!("a passphrase in the variable {v}"),
            Root::Provider(h) => h.provider().describe(),
        }
    }

    /// Whether the keyring holds a key for `purpose`.
    pub fn has(&self, purpose: &str) -> Result<bool, String> {
        let mut st = self.lock();
        self.load(&mut st)?;
        Ok(st
            .contents
            .as_ref()
            .is_some_and(|c| c.deks.contains_key(purpose)))
    }

    /// Takes in the key of an old file for `purpose`: wrapped into the
    /// keyring, then the keyring read back from disk and the key unwrapped
    /// from it — `Ok(true)` once the key is there and the same.  A
    /// different key already there for `purpose` is an error.
    pub fn import(&self, purpose: &str, key: &Key) -> Result<bool, String> {
        let mut st = self.lock();
        match self.dek_in(&mut st, purpose, false)? {
            Some(k) if k == *key => {}
            Some(_) => {
                return Err(format!(
                    "{} already holds another key for {purpose}",
                    self.path.display()
                ));
            }
            None => self.add_in(&mut st, purpose, key)?,
        }
        drop(st);
        // Read back: what a restart would find.
        let mut again = Keyring::at(self.path.clone(), self.settings.clone(), self.kv.clone());
        // The root as found: a passphrase or variable read the same way.
        again
            .state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .root = self.root(false)?;
        Ok(again.dek(purpose, false)?.as_ref() == Some(key))
    }

    /// Takes in an old key file for `purpose` at start, as the owner set:
    /// imported and removed, imported and kept (warned of), or — for
    /// [`OldFiles::Manual`] — left alone, `Ok(false)` telling the caller
    /// to use the old file as before.  `ours` is false for a file the
    /// user gave (a backup key): imported, never removed.
    pub fn adopt(&self, purpose: &str, file: &Path, ours: bool) -> Result<bool, String> {
        let Some(key) = read_key_file(file)? else {
            return Ok(true);
        };
        if self.settings.old_files == OldFiles::Manual && !self.has(purpose)? {
            nucleus_platform::log!(
                "nucleus: {} is used as before; `nucleus keys import` moves it into the keyring",
                file.display()
            );
            return Ok(false);
        }
        if !self.import(purpose, &key)? {
            return Err(format!(
                "{}: the key of {} did not read back from it",
                self.path.display(),
                file.display()
            ));
        }
        match (self.settings.old_files, ours) {
            (OldFiles::Remove, true) => {
                std::fs::remove_file(file).map_err(|e| format!("{}: {e}", file.display()))?;
                sync_dir(file);
                nucleus_platform::log!(
                    "nucleus: the key of {} is in {}, wrapped by the root; the file is removed",
                    file.display(),
                    self.path.display()
                );
            }
            (OldFiles::Keep | OldFiles::Manual, true) => nucleus_platform::log!(
                "nucleus: warning: {} is still there, its key in the clear — it is in {} now, \
                 wrapped by the root, and the file can be removed",
                file.display(),
                self.path.display()
            ),
            (_, false) => {}
        }
        Ok(true)
    }

    /// The data key for `purpose` (made when `create`).
    pub fn dek(&self, purpose: &str, create: bool) -> Result<Option<Key>, String> {
        let mut st = self.lock();
        self.dek_in(&mut st, purpose, create)
    }

    /// The root (made when `create` and its custody allows).
    pub fn root(&self, create: bool) -> Result<Option<Arc<dyn KeyProvider>>, String> {
        let mut st = self.lock();
        self.root_in(&mut st, create)
    }

    /// Wraps every data key again under `new`, and writes the keyring:
    /// the data keys — and so what they sealed — stay as they are (PLAN.md
    /// §M14.30 K1's root rotation, seconds whatever the data).
    pub fn rotate_root(&self, new: Arc<dyn KeyProvider>) -> Result<(), String> {
        let mut st = self.lock();
        self.load(&mut st)?;
        let purposes: Vec<String> = st
            .contents
            .as_ref()
            .map(|c| c.deks.keys().cloned().collect())
            .unwrap_or_default();
        let mut c = st.contents.clone().expect("loaded");
        for p in &purposes {
            let k = self
                .dek_in(&mut st, p, false)?
                .expect("listed in the keyring");
            let w = c.deks.get_mut(p).expect("listed");
            w.blob = new.wrap(p, &k)?;
        }
        c.root = Some((new.id(), new.kind()));
        if !matches!(self.settings.root, Root::Passphrase(_)) {
            c.salt = None;
        }
        self.save(&c)?;
        st.contents = Some(c);
        st.root = Some(new);
        Ok(())
    }

    /// The keys, for `nc_catalog.keys` and `nucleus keys list`.
    pub fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let mut st = self.lock();
        self.load(&mut st)?;
        let c = st.contents.as_ref().expect("loaded");
        let mut out = Vec::new();
        if let Some((id, kind)) = &c.root {
            out.push(KeyInfo {
                name: "root".into(),
                role: "root".into(),
                id: id.clone(),
                custody: kind.clone(),
                location: self.describe_root(),
                created: None,
            });
        }
        for (purpose, w) in &c.deks {
            out.push(KeyInfo {
                name: purpose.clone(),
                role: "data".into(),
                id: w.id.clone(),
                custody: c.root.as_ref().map_or(String::new(), |(id, _)| id.clone()),
                location: self.path.display().to_string(),
                created: Some(w.created),
            });
        }
        Ok(out)
    }
}

impl Custody for Keyring {
    fn dek(&self, purpose: &str, create: bool) -> Result<Option<Key>, String> {
        Keyring::dek(self, purpose, create)
    }

    fn root(&self, create: bool) -> Result<Option<Arc<dyn KeyProvider>>, String> {
        Keyring::root(self, create)
    }

    fn encrypt_backups(&self) -> bool {
        self.settings
            .encrypt_backups
            .unwrap_or(!matches!(self.settings.root, Root::Managed(_)))
    }

    fn list(&self) -> Result<Vec<KeyInfo>, String> {
        Keyring::list(self)
    }
}

/// The secrets' key (§M14.22) as the keyring's data key `secrets`.
pub struct SecretsKey(pub Arc<Keyring>);

impl nucleus_sql::secrets::Keyring for SecretsKey {
    fn key(&self, create: bool) -> Result<Option<[u8; keys::KEY_LEN]>, String> {
        Ok(self.0.dek("secrets", create)?.map(|k| *k.bytes()))
    }

    fn describe(&self) -> String {
        format!("the key \"secrets\" in {}", self.0.path().display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv() -> Arc<dyn Kv> {
        Arc::new(nucleus_kv::Database::open_in_memory(nucleus_kv::Options::default()).unwrap())
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nucleus-keyring-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn data_keys_are_kept_wrapped_and_read_back() {
        let d = dir("roundtrip");
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let s = Settings::default_for(db);
        let k = Keyring::new(db, s.clone(), kv());
        assert_eq!(k.dek("secrets", false).unwrap(), None);
        assert!(
            !beside(db, "root-key").exists(),
            "made before it was needed"
        );
        let a = k.dek("secrets", true).unwrap().unwrap();
        let text = std::fs::read_to_string(beside(db, "keyring")).unwrap();
        assert!(text.starts_with(HEADER), "{text}");
        assert!(!text.contains(&a.to_hex()), "a key in the clear");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["keyring", "root-key"] {
                let m = std::fs::metadata(beside(db, f))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(m & 0o777, 0o600, "{f}");
            }
        }
        // Another process: the same key.
        let again = Keyring::new(db, s.clone(), kv());
        assert_eq!(again.dek("secrets", false).unwrap(), Some(a.clone()));
        let names: Vec<String> = again.list().unwrap().into_iter().map(|i| i.name).collect();
        assert_eq!(names, ["root", "secrets"]);
        // Another root: refused, saying which.
        std::fs::write(beside(db, "root-key"), format!("{}\n", "11".repeat(32))).unwrap();
        let e = Keyring::new(db, s, kv()).dek("secrets", false).unwrap_err();
        assert!(e.contains("wrapped by the root key"), "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn old_files_are_taken_in_as_the_owner_set() {
        let d = dir("adopt");
        for (n, old) in [OldFiles::Remove, OldFiles::Keep, OldFiles::Manual]
            .into_iter()
            .enumerate()
        {
            let db = d.join(format!("db{n}"));
            let db = db.to_str().unwrap();
            let file = beside(db, "secrets-key");
            std::fs::write(&file, format!("{}\n", "ab".repeat(32))).unwrap();
            let mut s = Settings::default_for(db);
            s.old_files = old;
            let k = Keyring::new(db, s, kv());
            let used = k.adopt("secrets", &file, true).unwrap();
            match old {
                OldFiles::Remove => {
                    assert!(used && !file.exists());
                    assert_eq!(
                        *k.dek("secrets", false).unwrap().unwrap().bytes(),
                        [0xab; 32]
                    );
                }
                OldFiles::Keep => assert!(used && file.exists()),
                OldFiles::Manual => {
                    assert!(!used && file.exists());
                    assert!(!k.has("secrets").unwrap());
                }
            }
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_passphrase_root_and_a_rotation() {
        let d = dir("passphrase");
        let db = d.join("db");
        let db = db.to_str().unwrap();
        let var = "PASSPHRASE".to_string();
        let env = |v: &str| (v == "PASSPHRASE").then(|| "correct horse battery staple".to_string());
        let s = Settings {
            root: Root::Passphrase(var.clone()),
            old_files: OldFiles::Remove,
            encrypt_backups: None,
        };
        let k = Keyring::new(db, s.clone(), kv()).with_env(env);
        assert!(
            Custody::encrypt_backups(&k),
            "an explicit root encrypts backups"
        );
        let a = k.dek("backup", true).unwrap().unwrap();
        assert!(
            std::fs::read_to_string(beside(db, "keyring"))
                .unwrap()
                .contains("salt ")
        );
        assert_eq!(
            Keyring::new(db, s.clone(), kv())
                .with_env(env)
                .dek("backup", false)
                .unwrap(),
            Some(a.clone())
        );
        // A new root wraps the same data keys.
        let new: Arc<dyn KeyProvider> =
            Arc::new(LocalRoot::new(Key::new([5; 32]), "user", "a test's"));
        k.rotate_root(new.clone()).unwrap();
        let mut s2 = s.clone();
        s2.root = Root::Provider(Arc::new(Fixed(new)));
        assert_eq!(
            Keyring::new(db, s2, kv()).dek("backup", false).unwrap(),
            Some(a)
        );
        // The passphrase no longer opens it.
        assert!(
            Keyring::new(db, s, kv())
                .with_env(env)
                .dek("backup", false)
                .is_err()
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    struct Fixed(Arc<dyn KeyProvider>);
    impl std::fmt::Debug for Fixed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0.describe())
        }
    }
    impl ProviderHandle for Fixed {
        fn provider(&self) -> Arc<dyn KeyProvider> {
            Arc::clone(&self.0)
        }
    }
}
