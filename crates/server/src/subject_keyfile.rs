//! Subjects' keys in a file of their own (PLAN.md §M14.24 R6): `nucleus
//! server --subject-keys FILE`, by default `<dir>.subject-keys` beside the
//! data directory.  Made when the first backup of subjects (or the first
//! `FORGET`) needs it, readable by its owner alone, and kept — with its
//! own backups — apart from the data and the database's backups: what
//! `FORGET SUBJECT` destroys here is what makes the backups taken before
//! it unreadable for the subject.
//!
//! Each change is written to a new file, synced and renamed over the old
//! one before the call returns (a key a backup sealed with is never lost
//! to a crash; one destroyed is gone from the file).  The old file's
//! blocks may stay on the disk until overwritten: a store in a key
//! management service is a module's to add.
//!
//! Under the key hierarchy (PLAN.md §M14.30 K1d) the table is sealed:
//! `nucleus subject keys sealed 1`, then on a second line the nonce and
//! the table's text sealed by ChaCha20-Poly1305 under the keyring's data
//! key `subjects`, in hex.  A table in the clear (before) is read, and
//! sealed at its next write — or at once, when the old key files are
//! removed (`--old-key-files remove`, the default).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nucleus_crypto::aead;
use nucleus_kv::Kv;
use nucleus_sql::subject_keys::{Id, Key, KeyId, KeyTable, SubjectKeys};

use crate::keyring::Keyring;

const SEALED: &str = "nucleus subject keys sealed 1";
const AAD: &[u8] = b"nucleus subject keys";

/// Subjects' keys in a file.
pub struct SubjectKeyFile {
    path: PathBuf,
    /// For new keys' bytes (the platform's unpredictable generator).
    kv: Arc<dyn Kv>,
    /// The file's contents, once read.
    table: Mutex<Option<KeyTable>>,
    /// The keyring whose data key `subjects` seals the table (none: the
    /// table in the clear, as before §M14.30).
    ring: Option<Arc<Keyring>>,
}

impl SubjectKeyFile {
    /// The keys in `path`; `kv` draws new ones' bytes.
    pub fn new(path: impl Into<PathBuf>, kv: Arc<dyn Kv>) -> Self {
        Self {
            path: path.into(),
            kv,
            table: Mutex::new(None),
            ring: None,
        }
    }

    /// The keys in `path`, sealed under `ring`'s data key `subjects`.
    pub fn sealed(path: impl Into<PathBuf>, kv: Arc<dyn Kv>, ring: Arc<Keyring>) -> Self {
        Self {
            ring: Some(ring),
            ..Self::new(path, kv)
        }
    }

    /// Whether the file holds the table in the clear (`None`: no file).
    pub fn in_the_clear(&self) -> Result<Option<bool>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(Some(!t.starts_with(SEALED))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    /// Seals the table now under the keyring's key `subjects`: one kept in
    /// the clear (rewritten in place: no copy in the clear stays, so a key
    /// `FORGET` destroys is gone from it), or one sealed with the key a
    /// rotation replaces (§M14.30 K1e).
    pub fn seal_now(&self) -> Result<(), String> {
        let mut cached = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let t = match cached.take() {
            Some(t) => t,
            None => self.load()?,
        };
        self.save(&t)?;
        *cached = Some(t);
        Ok(())
    }

    /// The default file for data directory `dir`: `<dir>.subject-keys`.
    pub fn beside(dir: &str) -> PathBuf {
        let d = dir.trim_end_matches(['/', '\\']);
        PathBuf::from(format!(
            "{}.subject-keys",
            if d.is_empty() { dir } else { d }
        ))
    }

    fn load(&self) -> Result<KeyTable, String> {
        let at = |e: String| format!("{}: {e}", self.path.display());
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(KeyTable::default()),
            Err(e) => return Err(at(e.to_string())),
        };
        let Some(rest) = text.strip_prefix(SEALED) else {
            return KeyTable::from_text(&text).map_err(at);
        };
        let ring = self
            .ring
            .as_ref()
            .ok_or_else(|| at("sealed, and no keyring to open it".into()))?;
        let key = ring.dek("subjects", false)?.ok_or_else(|| {
            at(format!(
                "sealed, and {} has no key for it",
                ring.path().display()
            ))
        })?;
        let b = nucleus_crypto::keys::from_hex(rest.trim())
            .filter(|b| b.len() > aead::NONCE_LEN)
            .ok_or_else(|| at("damaged".into()))?;
        let (nonce, sealed) = b.split_at(aead::NONCE_LEN);
        let nonce: [u8; aead::NONCE_LEN] = nonce.try_into().expect("nonce");
        // The key, or — a rotation cut short — the one it replaces.
        let opened = match aead::open(key.bytes(), &nonce, AAD, sealed) {
            Some(p) => Some(p),
            None => ring
                .previous("subjects")?
                .and_then(|k| aead::open(k.bytes(), &nonce, AAD, sealed)),
        };
        let mut plain = opened.ok_or_else(|| {
            at("does not open with the keyring's key: damaged, or another's".into())
        })?;
        let t = String::from_utf8(plain.clone()).map_err(|_| at("damaged".into()));
        plain.fill(0);
        KeyTable::from_text(&t?).map_err(at)
    }

    /// The file's text for `t`: sealed under the keyring's key, or in the
    /// clear without a keyring.
    fn text(&self, t: &KeyTable) -> Result<String, String> {
        let Some(ring) = &self.ring else {
            return Ok(t.to_text());
        };
        let key = ring.dek("subjects", true)?.expect("made");
        let mut nonce = [0u8; aead::NONCE_LEN];
        self.kv.random_bytes(&mut nonce);
        let mut plain = t.to_text().into_bytes();
        let sealed = aead::seal(key.bytes(), &nonce, AAD, &plain);
        plain.fill(0);
        let mut b = nonce.to_vec();
        b.extend_from_slice(&sealed);
        Ok(format!("{SEALED}\n{}\n", nucleus_crypto::keys::to_hex(&b)))
    }

    /// Writes `t` durably: a new file, synced, renamed over the old one,
    /// the directory synced.
    fn save(&self, t: &KeyTable) -> Result<(), String> {
        let fail = |e: std::io::Error| format!("{}: {e}", self.path.display());
        let new = self.path.with_extension("subject-keys.new");
        let _ = std::fs::remove_file(&new);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let text = self.text(t)?;
        let mut f = o.open(&new).map_err(fail)?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(fail)?;
        drop(f);
        let first = !self.path.exists();
        std::fs::rename(&new, &self.path).map_err(fail)?;
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty())
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        if first {
            nucleus_platform::log!(
                "nucleus: made the file of subject keys, {} — keep it, and back it up apart \
                 from the data and its backups",
                self.path.display()
            );
        }
        Ok(())
    }

    /// Runs `f` on the keys, read if not yet; written if it changed them.
    fn with<T>(
        &self,
        f: impl FnOnce(&mut KeyTable, &dyn Fn(&mut [u8])) -> (T, bool),
    ) -> Result<T, String> {
        let mut cached = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            *cached = Some(self.load()?);
        }
        let t = cached.as_mut().expect("loaded");
        let before = t.clone();
        let (out, changed) = f(t, &|b| self.kv.random_bytes(b));
        if changed && let Err(e) = self.save(t) {
            // Not kept: not made.
            *t = before;
            return Err(e);
        }
        Ok(out)
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SubjectKeys for SubjectKeyFile {
    fn pepper(&self, create: bool) -> Result<Option<Key>, String> {
        self.with(|t, random| t.pepper(create, &mut |b| random(b)))
    }

    fn keys(&self, ids: &[Id], snapshot: u64) -> Result<Vec<Option<(KeyId, Key)>>, String> {
        self.with(|t, random| t.keys(ids, snapshot, &mut |b| random(b)))
    }

    fn open(&self, kids: &[KeyId]) -> Result<Vec<Option<Key>>, String> {
        self.with(|t, _| (t.open(kids), false))
    }

    fn destroy(&self, ids: &[Id], commit: u64) -> Result<u64, String> {
        self.with(|t, _| (t.destroy(ids, commit), !ids.is_empty()))
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_outlive_the_process_and_destroyed_ones_do_not() {
        let dir = nucleus_platform::temp_dir().join(format!(
            "nucleus-subject-keys-{}",
            nucleus_platform::process_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let kv: Arc<dyn Kv> =
            Arc::new(nucleus_kv::Database::open(dir.join("db"), Default::default()).unwrap());
        let path = dir.join("db.subject-keys");
        let _ = std::fs::remove_file(&path);
        let f = SubjectKeyFile::new(&path, Arc::clone(&kv));
        let pepper = f.pepper(true).unwrap().unwrap();
        let got = f.keys(&[[1; 32], [2; 32]], 5).unwrap();
        let (kid1, key1) = got[0].unwrap();
        let (kid2, _) = got[1].unwrap();
        f.destroy(&[[2; 32]], 9).unwrap();
        // Read again, as another process would.
        let g = SubjectKeyFile::new(&path, kv);
        assert_eq!(g.pepper(false).unwrap(), Some(pepper));
        assert_eq!(g.open(&[kid1, kid2]).unwrap(), vec![Some(key1), None]);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains(&kid2.iter().map(|b| format!("{b:02x}")).collect::<String>())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_table_in_the_clear_is_sealed_and_reads_back() {
        let dir = nucleus_platform::temp_dir().join(format!(
            "nucleus-subject-keys-sealed-{}",
            nucleus_platform::process_id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kv: Arc<dyn Kv> =
            Arc::new(nucleus_kv::Database::open(dir.join("db"), Default::default()).unwrap());
        let db = dir.join("db");
        let db = db.to_str().unwrap();
        let path = dir.join("db.subject-keys");
        // Before: in the clear.
        let old = SubjectKeyFile::new(&path, Arc::clone(&kv));
        let (kid, key) = old.keys(&[[3; 32]], 1).unwrap()[0].unwrap();
        let ring = Arc::new(Keyring::new(
            db,
            crate::keyring::Settings::default_for(db),
            Arc::clone(&kv),
        ));
        let f = SubjectKeyFile::sealed(&path, Arc::clone(&kv), Arc::clone(&ring));
        assert_eq!(f.in_the_clear().unwrap(), Some(true));
        f.seal_now().unwrap();
        assert_eq!(f.in_the_clear().unwrap(), Some(false));
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&hex));
        // Another process with the keyring: the key; without: refused.
        let g = SubjectKeyFile::sealed(&path, Arc::clone(&kv), ring);
        assert_eq!(g.open(&[kid]).unwrap(), vec![Some(key)]);
        let e = SubjectKeyFile::new(&path, kv).open(&[kid]).unwrap_err();
        assert!(e.contains("no keyring"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
