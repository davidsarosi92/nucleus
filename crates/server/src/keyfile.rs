//! The key secrets are sealed with (PLAN.md §M14.22), in a file of its own
//! outside the data directory: `nucleus server --secrets-key FILE`, by
//! default `<dir>.secrets-key` beside the directory.  Made when the first
//! secret with a value is, readable by its owner alone; a copy of the
//! directory, or a backup of it, then holds no secret in the clear — and
//! the file must be kept (and backed up) apart from it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nucleus_crypto::aead::KEY_LEN;
use nucleus_kv::Kv;
use nucleus_sql::secrets::Keyring;

/// A key in a file: 64 hex digits.
pub struct KeyFile {
    path: PathBuf,
    /// For a new key's bytes (the platform's unpredictable generator).
    kv: Arc<dyn Kv>,
    key: Mutex<Option<[u8; KEY_LEN]>>,
}

impl KeyFile {
    /// The key in `path`; `kv` draws a new one's bytes.
    pub fn new(path: impl Into<PathBuf>, kv: Arc<dyn Kv>) -> Self {
        Self {
            path: path.into(),
            kv,
            key: Mutex::new(None),
        }
    }

    /// The default file for data directory `dir`: `<dir>.secrets-key`.
    pub fn beside(dir: &str) -> PathBuf {
        let d = dir.trim_end_matches(['/', '\\']);
        PathBuf::from(format!(
            "{}.secrets-key",
            if d.is_empty() { dir } else { d }
        ))
    }

    fn read(&self) -> Result<Option<[u8; KEY_LEN]>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let t = text.trim();
        if t.len() != 2 * KEY_LEN || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{} is not 64 hex digits", self.path.display()));
        }
        let mut k = [0u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).expect("hex");
        }
        Ok(Some(k))
    }

    fn create(&self) -> Result<[u8; KEY_LEN], String> {
        let mut k = [0u8; KEY_LEN];
        self.kv.random_bytes(&mut k);
        let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = match o.open(&self.path) {
            Ok(f) => f,
            // Made meanwhile by another process: that one is the key.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return self
                    .read()?
                    .ok_or_else(|| format!("{} vanished", self.path.display()));
            }
            Err(e) => return Err(e.to_string()),
        };
        f.write_all(format!("{hex}\n").as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| e.to_string())?;
        eprintln!(
            "nucleus: made the key for secrets, {} — keep it, and back it up apart from the data",
            self.path.display()
        );
        Ok(k)
    }
}

impl Keyring for KeyFile {
    fn key(&self, create: bool) -> Result<Option<[u8; KEY_LEN]>, String> {
        let mut cached = self.key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = *cached {
            return Ok(Some(k));
        }
        let k = match self.read()? {
            Some(k) => k,
            None if create => self.create()?,
            None => return Ok(None),
        };
        *cached = Some(k);
        Ok(Some(k))
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}
