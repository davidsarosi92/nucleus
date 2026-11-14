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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nucleus_kv::Kv;
use nucleus_sql::subject_keys::{Id, Key, KeyId, KeyTable, SubjectKeys};

/// Subjects' keys in a file.
pub struct SubjectKeyFile {
    path: PathBuf,
    /// For new keys' bytes (the platform's unpredictable generator).
    kv: Arc<dyn Kv>,
    /// The file's contents, once read.
    table: Mutex<Option<KeyTable>>,
}

impl SubjectKeyFile {
    /// The keys in `path`; `kv` draws new ones' bytes.
    pub fn new(path: impl Into<PathBuf>, kv: Arc<dyn Kv>) -> Self {
        Self {
            path: path.into(),
            kv,
            table: Mutex::new(None),
        }
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
        match std::fs::read_to_string(&self.path) {
            Ok(t) => KeyTable::from_text(&t).map_err(|e| format!("{}: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KeyTable::default()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
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
        let mut f = o.open(&new).map_err(fail)?;
        f.write_all(t.to_text().as_bytes())
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
            eprintln!(
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
}
