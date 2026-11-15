//! Backups (PLAN.md §M14.24 R6, §M14.18 O3): `BACKUP TO 'repo'` writes
//! the database as one snapshot saw it, while others go on writing, into
//! a repository — the first backup all of it, each next one what changed
//! since the one before; `restore` (`nucleus restore`) makes a new
//! database of a chain, up to a chosen backup.
//!
//! A backup is the store's entries, less what is derived from them:
//! secondary indexes (a B-tree's, HNSW's, a text index's — their pages
//! hold the indexed values in the clear, and are rebuilt from the rows)
//! and `ANALYZE`'s samples (taken again).  The catalog, roles, models,
//! secrets (sealed already) and the rest travel as they are.
//!
//! Each entry about a subject ([`crate::forget`]) is sealed — ChaCha20-
//! Poly1305 under a key derived from its subjects' keys
//! ([`crate::subject_keys`]), kept apart from the data and the backups:
//! a subject table's row, its values stored out of line, its embedding
//! jobs and next model's vectors, and the rows of the tables referencing
//! it (unless their key is set to `NULL` or a default when it goes),
//! transitively — what `FORGET SUBJECT` deletes with it.  An audit record
//! whose statement names a subject (a literal `'value'`) has its detail
//! sealed and its other fields in the clear.  Once `FORGET` destroyed a
//! subject's key, a restore leaves those entries out and blanks those
//! details: the backups taken before forget too.
//!
//! The files: a directory a backup in the repository (`000001-full`,
//! `000002-incr`, …), its parts of about 16 MiB (`part-000000`, …), a
//! sequence of entries each; a `manifest`, written last and renamed into
//! place, names the format, the backup it follows, the snapshot, the
//! counts and each part's SHA-256 — no manifest, no backup.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;

use nucleus_crypto::{aead, sha256};
use nucleus_kv::{Kv, KvTxn, TxnOptions};
use nucleus_platform::{FileSystem, OpenMode};

use crate::ast::FkAction;
use crate::catalog::{self, Table};
use crate::error::{Error, Result};
use crate::ext::Registry;
use crate::subject_keys::{self, Id, Key, KeyId, SubjectKeys};
use crate::types::{DataType, Value};

/// The manifest's first line.
pub const FORMAT: &str = "nucleus backup 2";
/// That of a backup made before repositories (one directory, all of the
/// database, no deletes): read still.
pub const FORMAT_1: &str = "nucleus backup 1";
/// A part is closed once it holds this many bytes.
const PART_BYTES: usize = 16 << 20;
const MANIFEST: &str = "manifest";
/// A restore commits its writes in transactions of about this much.
const RESTORE_BATCH: usize = 4 << 20;

/// An entry as it was.
const PLAIN: u8 = 0;
/// An entry sealed: its key and value.
const SEALED: u8 = 1;
/// An audit record, its detail sealed.
const SEALED_DETAIL: u8 = 2;
/// A key deleted (an incremental backup's).
const DELETE: u8 = 3;
/// A key deleted, sealed (a row of subjects).
const SEALED_DELETE: u8 = 4;

const STATS: [u8; 2] = [0x00, 0x04];
const QUEUE: [u8; 2] = [0x00, 0x17];
const SHADOW: [u8; 2] = [0x00, 0x18];
const AUDIT: [u8; 2] = [0x00, 0x12];
const DATA: u8 = 0x01;

/// What `BACKUP` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Backup {
    /// The snapshot written: `AS OF COMMIT` it reads the same.
    pub commit: u64,
    /// The entries written.
    pub entries: u64,
    /// Of them sealed (an audit record's detail counts).
    pub sealed: u64,
    /// Entries left out: their subject forgotten while the backup ran.
    pub omitted: u64,
    /// The parts' bytes.
    pub bytes: u64,
    /// Of the entries, keys deleted since the backup before.
    pub deleted: u64,
    /// `full` or `incremental`.
    pub kind: &'static str,
    /// Its directory in the repository.
    pub name: String,
    /// Older backups removed (`KEEP`).
    pub removed: u64,
}

/// The columns `BACKUP` returns.
pub const RESULT: [(&str, DataType); 9] = [
    ("commit", DataType::BigInt),
    ("entries", DataType::BigInt),
    ("sealed", DataType::BigInt),
    ("omitted", DataType::BigInt),
    ("bytes", DataType::BigInt),
    ("deleted", DataType::BigInt),
    ("kind", DataType::Text),
    ("backup", DataType::Text),
    ("removed", DataType::BigInt),
];

impl Backup {
    /// As `BACKUP`'s row.
    pub fn row(&self) -> Vec<Value> {
        let mut row: Vec<Value> = [
            self.commit,
            self.entries,
            self.sealed,
            self.omitted,
            self.bytes,
            self.deleted,
        ]
        .map(|n| Value::Int(n as i64))
        .to_vec();
        row.push(Value::Text(self.kind.into()));
        row.push(Value::Text(self.name.as_str().into()));
        row.push(Value::Int(self.removed as i64));
        row
    }
}

/// Where an entry's subjects are found: by table, the rows of subjects
/// (theirs, or those of the rows they reference).
#[derive(Default)]
struct Subjects {
    /// Each set of subjects (ids sorted), by number.
    sets: Vec<Vec<Id>>,
    numbers: HashMap<Vec<Id>, u32>,
    /// Table id → a row's primary key suffix → its set.
    rows: HashMap<u32, HashMap<Vec<u8>, u32>>,
    /// A subject's text → its id (an audit detail's literals).
    texts: HashMap<String, Id>,
    /// A hash of the head of a text its rows hold (as `FORGET` blanks the
    /// statements holding one, [`crate::forget`]) → their set.
    heads: HashMap<u64, u32>,
}

impl Subjects {
    fn number(&mut self, mut ids: Vec<Id>) -> u32 {
        ids.sort_unstable();
        ids.dedup();
        if let Some(&n) = self.numbers.get(&ids) {
            return n;
        }
        let n = self.sets.len() as u32;
        self.sets.push(ids.clone());
        self.numbers.insert(ids, n);
        n
    }

    /// The set of the row `pk` of table `table`, if it has subjects.
    fn of(&self, table: u32, pk: &[u8]) -> Option<u32> {
        self.rows.get(&table)?.get(pk).copied()
    }
}

/// The shortest text of a row a statement is matched by (`FORGET`'s).
const MIN_TEXT: usize = 8;

/// A text as a statement would hold it, its head: its literal's escaping,
/// its first 64 bytes (`FORGET`'s rule), hashed.
fn head(s: &str) -> u64 {
    let lit = s.replace('\'', "''");
    let mut end = lit.len().min(64);
    while !lit.is_char_boundary(end) {
        end -= 1;
    }
    crate::stablehash::xxh64(&lit.as_bytes()[..end], 0)
}

/// A value as `FORGET SUBJECT` compares it: cast to text.
fn subject_text(v: &Value) -> String {
    match v.clone().cast(DataType::Text) {
        Ok(Value::Text(s)) => s.to_string(),
        _ => v.render(),
    }
}

/// A tuple of values as one key (`None` when one is `NULL`: such a key
/// references nothing).
fn tuple(row: &[Value], columns: &[usize]) -> Option<String> {
    let mut s = String::new();
    for &c in columns {
        let v = row.get(c)?;
        if *v == Value::Null {
            return None;
        }
        s.push_str(&v.render());
        s.push('\u{1f}');
    }
    Some(s)
}

/// Whether a key's action carries a row with the row it references: all
/// but setting it to `NULL` or a default, which keep the row.
fn follows(fk: &catalog::ForeignKey) -> bool {
    !matches!(fk.on_delete, FkAction::SetNull | FkAction::SetDefault)
}

/// The tables with rows of subjects, and each such row's subjects.
fn subjects(txn: &dyn KvTxn, tables: &[Table], pepper: Option<&Key>) -> Result<Subjects> {
    let by_name: HashMap<&str, &Table> = tables.iter().map(|t| (t.name.as_str(), t)).collect();
    let parent = |t: &Table| -> Option<&Table> {
        t.partition_of
            .as_ref()
            .and_then(|(p, _)| by_name.get(p.as_str()).copied())
    };
    // A table's subject columns by name (a partition's are its parent's).
    let subject_names = |t: &Table| -> Vec<String> {
        let owner = if t.subject.is_empty() {
            parent(t).unwrap_or(t)
        } else {
            t
        };
        owner
            .subject
            .iter()
            .map(|&c| owner.columns[c].name.clone())
            .collect()
    };
    // The keys a row follows the referenced row by, as (declared on, key).
    let following = |t: &Table| -> Vec<(&Table, &catalog::ForeignKey)> {
        let mut out: Vec<(&Table, &catalog::ForeignKey)> = Vec::new();
        for owner in std::iter::once(by_name[t.name.as_str()]).chain(parent(t)) {
            for fk in &owner.foreign_keys {
                if follows(fk)
                    && fk.table != owner.name
                    && !out.iter().any(|(_, f)| f.name == fk.name)
                {
                    out.push((owner, fk));
                }
            }
        }
        out
    };
    // The tables whose rows have subjects.
    let mut related: Vec<&Table> = tables
        .iter()
        .filter(|t| !subject_names(t).is_empty())
        .collect();
    loop {
        let more: Vec<&Table> = tables
            .iter()
            .filter(|t| {
                !related.iter().any(|r| r.name == t.name)
                    && following(t)
                        .iter()
                        .any(|(_, fk)| related.iter().any(|r| r.name == fk.table))
            })
            .collect();
        if more.is_empty() {
            break;
        }
        related.extend(more);
    }
    let mut out = Subjects::default();
    if related.is_empty() {
        return Ok(out);
    }
    let Some(pepper) = pepper else {
        return Err(Error::prerequisite(
            "the database has subject columns and no store of subject keys is set \
             (nucleus_sql::subject_keys::set_subject_keys): its backups could not be forgotten",
        ));
    };
    for t in &related {
        if t.storage != crate::storage::ROW {
            return Err(Error::feature(format!(
                "a backup of \"{}\", stored as {} with rows of subjects: not yet (only rows \
                 stored as rows are sealed one by one)",
                t.name, t.storage
            )));
        }
    }
    // The referenced columns wanted, by the name a key declares (a
    // partitioned table's rows are in its partitions).
    let mut wanted_refs: HashMap<String, Vec<Vec<String>>> = HashMap::new();
    for t in &related {
        for (_, fk) in following(t) {
            let Some(p) = by_name.get(fk.table.as_str()) else {
                continue;
            };
            let names: Vec<String> = fk
                .ref_columns
                .iter()
                .map(|&c| p.columns[c].name.clone())
                .collect();
            let w = wanted_refs.entry(fk.table.clone()).or_default();
            if !w.contains(&names) {
                w.push(names);
            }
        }
    }
    let mut refs: HashMap<(String, Vec<String>), HashMap<String, u32>> = HashMap::new();
    // Referenced tables first: a table is ready once those its keys name
    // (and their partitions) are done; in a cycle, the first left.
    let mut done: HashSet<&str> = HashSet::new();
    let mut left: Vec<&Table> = related.clone();
    while !left.is_empty() {
        let ready = |t: &Table| {
            following(t).iter().all(|(_, fk)| {
                related.iter().all(|r| {
                    !(r.name == fk.table
                        || r.partition_of.as_ref().is_some_and(|(p, _)| *p == fk.table))
                        || done.contains(r.name.as_str())
                })
            })
        };
        let i = (0..left.len()).find(|&i| ready(left[i])).unwrap_or(0);
        let t = left.remove(i);
        let position = |name: &str| t.column(name);
        let subject_cols: Vec<usize> = subject_names(t)
            .iter()
            .filter_map(|n| position(n))
            .collect();
        let keys: Vec<(String, Vec<String>, Vec<usize>)> = following(t)
            .into_iter()
            .filter_map(|(owner, fk)| {
                let p = by_name.get(fk.table.as_str())?;
                let refs: Vec<String> = fk
                    .ref_columns
                    .iter()
                    .map(|&c| p.columns[c].name.clone())
                    .collect();
                let cols: Option<Vec<usize>> = fk
                    .columns
                    .iter()
                    .map(|&c| position(&owner.columns[c].name))
                    .collect();
                Some((fk.table.clone(), refs, cols?))
            })
            .collect();
        let mut referenced: Vec<(Vec<String>, Vec<usize>)> = Vec::new();
        for name in std::iter::once(t.name.as_str()).chain(parent(t).map(|p| p.name.as_str())) {
            for names in wanted_refs.get(name).into_iter().flatten() {
                let cols: Option<Vec<usize>> = names.iter().map(|n| position(n)).collect();
                if let Some(cols) = cols {
                    referenced.push((names.clone(), cols));
                }
            }
        }
        let declared = parent(t).map_or(t.name.clone(), |p| p.name.clone());
        let storage = crate::storage::of(t)?;
        let prefix = t.index_prefix(0);
        let mut mine = HashMap::new();
        for r in storage.rows(crate::storage::Scan {
            txn,
            table: t,
            start: prefix.clone(),
            end: nucleus_kv::prefix_end(&prefix),
            wanted: None,
            validated: false,
            bounds: Vec::new(),
            dicts: Vec::new(),
        })? {
            let (key, row) = r?;
            let mut ids = Vec::new();
            for &c in &subject_cols {
                if row[c] != Value::Null {
                    let text = subject_text(&row[c]);
                    let id = subject_keys::id(pepper, &text);
                    out.texts.insert(text, id);
                    ids.push(id);
                }
            }
            for (table, names, cols) in &keys {
                if let Some(v) = tuple(&row, cols)
                    && let Some(&s) = refs
                        .get(&(table.clone(), names.clone()))
                        .and_then(|m| m.get(&v))
                {
                    ids.extend_from_slice(&out.sets[s as usize]);
                }
            }
            if ids.is_empty() {
                continue;
            }
            let s = out.number(ids);
            mine.insert(crate::exec::pk_suffix(t, &key).to_vec(), s);
            for v in &row {
                if let Value::Text(text) = v
                    && text.len() >= MIN_TEXT
                {
                    let h = head(text);
                    let joined = match out.heads.get(&h) {
                        Some(&o) if o != s => {
                            let mut both = out.sets[o as usize].clone();
                            both.extend_from_slice(&out.sets[s as usize]);
                            out.number(both)
                        }
                        _ => s,
                    };
                    out.heads.insert(h, joined);
                }
            }
            for (names, cols) in &referenced {
                if let Some(v) = tuple(&row, cols) {
                    for name in [&t.name, &declared] {
                        if wanted_refs.get(name).is_some_and(|w| w.contains(names)) {
                            refs.entry((name.clone(), names.clone()))
                                .or_default()
                                .insert(v.clone(), s);
                        }
                    }
                }
            }
        }
        out.rows.insert(t.id, mine);
        done.insert(t.name.as_str());
    }
    Ok(out)
}

/// The literals (`'…'`) a statement's text holds, unescaped.
fn literals(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\'' {
            i += 1;
            continue;
        }
        let mut lit = Vec::new();
        let mut j = i + 1;
        let mut closed = false;
        while j < b.len() {
            if b[j] == b'\'' {
                if b.get(j + 1) == Some(&b'\'') {
                    lit.push(b'\'');
                    j += 2;
                    continue;
                }
                closed = true;
                break;
            }
            lit.push(b[j]);
            j += 1;
        }
        if closed {
            out.push(String::from_utf8_lossy(&lit).into_owned());
        }
        i = j + 1;
    }
    out
}

/// Where a key is about a table's row: (table id, primary key suffix) —
/// the row itself (and whether it is), a value of it stored out of line,
/// an embedding job or a next model's vector.
fn row_of(key: &[u8]) -> Option<(u32, &[u8], bool)> {
    let id = |b: &[u8]| u32::from_be_bytes(b.try_into().expect("4"));
    if key.first() == Some(&DATA) && key.len() >= catalog::INDEX_PREFIX_LEN {
        let table = id(&key[1..5]);
        let index = id(&key[5..9]);
        let rest = &key[catalog::INDEX_PREFIX_LEN..];
        return match index {
            0 => Some((table, rest, true)),
            crate::large::LARGE_INDEX if rest.len() >= 6 => {
                Some((table, &rest[..rest.len() - 6], false))
            }
            _ => None,
        };
    }
    if (key.starts_with(&QUEUE) || key.starts_with(&SHADOW)) && key.len() >= 8 {
        return Some((id(&key[2..6]), &key[8..], false));
    }
    None
}

/// The parts being written.
struct Parts<'a> {
    target: &'a dyn Target,
    /// The backup's directory in the repository.
    dir: String,
    buf: Vec<u8>,
    /// The repository's key: the parts encrypted with it.
    key: Option<BackupKey>,
    /// The parts' nonces (a random base, as [`Nonces`]).
    nonces: Nonces,
    done: Vec<(String, u64, [u8; 32])>,
}

impl Parts<'_> {
    /// A part, stored: its entries' length (`u32`, little endian) and them
    /// compressed (LZ4's block format, as the storage's values); with a
    /// key, that encrypted (ChaCha20-Poly1305, its nonce first, the part's
    /// path in the repository authenticated with it).
    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let name = format!("part-{:06}", self.done.len());
        let path = format!("{}/{name}", self.dir);
        let mut z = Vec::with_capacity(self.buf.len() / 2 + 64);
        z.extend_from_slice(&(self.buf.len() as u32).to_le_bytes());
        nucleus_kv::lz::compress(&self.buf, &mut z);
        let stored = match &self.key {
            None => z,
            Some(key) => {
                let nonce = self.nonces.next();
                let mut s = nonce.to_vec();
                s.extend_from_slice(&aead::seal(&key.key, &nonce, path.as_bytes(), &z));
                s
            }
        };
        put(self.target, &path, &stored)?;
        self.done.push((name, stored.len() as u64, sha256(&stored)));
        self.buf.clear();
        Ok(())
    }

    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(b);
    }

    fn end_entry(&mut self) -> Result<()> {
        if self.buf.len() >= PART_BYTES {
            self.flush()?;
        }
        Ok(())
    }
}

/// A repository's key (PLAN.md §M14.18 O3c): a backup's parts are
/// encrypted with it and its manifest authenticated.  Restoring needs it;
/// with one, a backup not made with it is refused (whoever can write to
/// the repository could otherwise add one).
#[derive(Clone)]
pub struct BackupKey {
    key: [u8; 32],
    id: String,
}

impl BackupKey {
    /// A key of 32 bytes.
    pub fn new(key: [u8; 32]) -> Self {
        let mut tagged = b"nucleus backup key".to_vec();
        tagged.extend_from_slice(&key);
        let id = crate::audit::hex(&sha256(&tagged)[..8]);
        Self { key, id }
    }

    /// A key written as 64 hexadecimal digits (`openssl rand -hex 32`),
    /// whitespace around them ignored.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let t = text.trim();
        if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("a backup key is 64 hexadecimal digits (openssl rand -hex 32)".into());
        }
        let mut key = [0; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = u8::from_str_radix(&t[2 * i..2 * i + 2], 16).expect("hex");
        }
        Ok(Self::new(key))
    }

    /// The key in file `path` ([`BackupKey::parse`]'s form).
    pub fn from_file(path: &std::path::Path) -> std::result::Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Its id, written in the manifests: which key, not the key.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Debug for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BackupKey({})", self.id)
    }
}

static BACKUP_KEY: std::sync::RwLock<Option<BackupKey>> = std::sync::RwLock::new(None);

/// Sets the key backups are made and restored with (the process's: the
/// server's `--backup-key`), or none.
pub fn set_backup_key(key: Option<BackupKey>) {
    *BACKUP_KEY.write().unwrap_or_else(|e| e.into_inner()) = key;
}

/// The key set, if any.
pub fn backup_key() -> Option<BackupKey> {
    BACKUP_KEY.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Nonces: a random base a backup, counted up (a key seals entries of
/// many backups: a random base keeps their nonces apart).
struct Nonces {
    base: [u8; aead::NONCE_LEN],
    n: u64,
}

impl Nonces {
    fn next(&mut self) -> [u8; aead::NONCE_LEN] {
        self.n += 1;
        let mut nonce = self.base;
        let tail = u64::from_le_bytes(nonce[4..].try_into().expect("8")).wrapping_add(self.n);
        nonce[4..].copy_from_slice(&tail.to_le_bytes());
        nonce
    }
}

/// What a sealed entry's authentication covers: its kind, what of it is
/// in the clear, and its subjects.
fn aad(kind: u8, clear: &[&[u8]], kids: &[KeyId]) -> Vec<u8> {
    let mut a = vec![kind];
    for c in clear {
        a.extend_from_slice(&(c.len() as u32).to_le_bytes());
        a.extend_from_slice(c);
    }
    for k in kids {
        a.extend_from_slice(k);
    }
    a
}

/// A set's keys' ids, written.
fn put_kids(buf: &mut Vec<u8>, kids: &[KeyId]) {
    buf.extend_from_slice(&(kids.len() as u16).to_le_bytes());
    for k in kids {
        buf.extend_from_slice(k);
    }
}

/// The backups of a repository: their directories, `000001-full`,
/// `000002-incr`, … — a number, and whether it holds all or what changed
/// since the one before.
fn backup_name(number: u64, incremental: bool) -> String {
    format!("{number:06}-{}", if incremental { "incr" } else { "full" })
}

/// A backup's number, from its directory's name.
fn backup_number(name: &str) -> Option<u64> {
    let (n, kind) = name.split_once('-')?;
    (n.len() >= 6 && n.bytes().all(|b| b.is_ascii_digit()) && matches!(kind, "full" | "incr"))
        .then(|| n.parse().ok())
        .flatten()
}

/// Where a repository's backups are kept (PLAN.md §M14.18 O3b): files
/// named by paths within it (`000001-full/part-000000`) — a directory of
/// the database's file system built in, an object store a module adds
/// ([`register_target`]).  A file is written whole and seen only whole
/// (the manifest last: no manifest, no backup).
pub trait Target: Send + Sync {
    /// The names right under `dir` (`""`: the repository): a backup's
    /// directory, a backup's files.  None if there is nothing there.
    fn list(&self, dir: &str) -> std::io::Result<Vec<String>>;
    /// File `name`'s bytes; `None` if there is none.
    fn get(&self, name: &str) -> std::io::Result<Option<Vec<u8>>>;
    /// Writes file `name` whole, durably before this returns.
    fn put(&self, name: &str, data: &[u8]) -> std::io::Result<()>;
    /// Removes file `name` (none: done).
    fn delete(&self, name: &str) -> std::io::Result<()>;
    /// Where it is, for messages (never a secret).
    fn describe(&self) -> String;
}

/// Opens the repository at a URL of a module's scheme.
pub type Opener = fn(&str) -> std::result::Result<Box<dyn Target>, String>;

static SCHEMES: std::sync::RwLock<Vec<(String, Opener)>> = std::sync::RwLock::new(Vec::new());

/// Registers the repositories at `<scheme>://…` (a module's: `s3`).
pub fn register_target(scheme: &str, open: Opener) {
    let mut s = SCHEMES.write().unwrap_or_else(|e| e.into_inner());
    s.retain(|(n, _)| n != scheme);
    s.push((scheme.to_string(), open));
}

/// A repository in a directory of the database's file system: each file
/// written beside as `.tmp`, synced, renamed, the directory synced.
struct DirTarget<'a> {
    fs: &'a dyn FileSystem,
    root: PathBuf,
}

impl DirTarget<'_> {
    fn path(&self, name: &str) -> PathBuf {
        name.split('/')
            .filter(|s| !s.is_empty())
            .fold(self.root.clone(), |p, s| p.join(s))
    }
}

impl Target for DirTarget<'_> {
    fn list(&self, dir: &str) -> std::io::Result<Vec<String>> {
        let p = self.path(dir);
        if !self.fs.exists(&p)? {
            return Ok(Vec::new());
        }
        Ok(self
            .fs
            .list_dir(&p)?
            .into_iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect())
    }

    fn get(&self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        let p = self.path(name);
        if !self.fs.exists(&p)? {
            return Ok(None);
        }
        let f = self.fs.open(&p, OpenMode::ReadOnly)?;
        let mut b = vec![0u8; f.size()? as usize];
        f.read_exact_at(&mut b, 0)?;
        Ok(Some(b))
    }

    fn put(&self, name: &str, data: &[u8]) -> std::io::Result<()> {
        let p = self.path(name);
        let dir = p.parent().unwrap_or(&self.root).to_path_buf();
        self.fs.create_dir_all(&dir)?;
        let tmp = PathBuf::from(format!("{}.tmp", p.display()));
        if self.fs.exists(&tmp)? {
            self.fs.remove_file(&tmp)?;
        }
        let f = self.fs.open(&tmp, OpenMode::CreateNew)?;
        f.write_all_at(data, 0)?;
        f.sync()?;
        drop(f);
        self.fs.rename(&tmp, &p)?;
        self.fs.sync_dir(&dir)
    }

    /// A backup's directory left empty goes too.
    fn delete(&self, name: &str) -> std::io::Result<()> {
        let p = self.path(name);
        if !self.fs.exists(&p)? {
            return Ok(());
        }
        self.fs.remove_file(&p)?;
        let dir = p.parent().unwrap_or(&self.root).to_path_buf();
        self.fs.sync_dir(&dir)?;
        if dir != self.root && self.fs.list_dir(&dir)?.is_empty() {
            self.fs.remove_dir(&dir)?;
            self.fs.sync_dir(dir.parent().unwrap_or(&self.root))?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// The repository `spec` names: `<scheme>://…` a module's, else an
/// absolute path on the server.
fn open_target<'a>(kv: &'a Arc<dyn Kv>, spec: &str) -> Result<Box<dyn Target + 'a>> {
    if let Some((scheme, _)) = spec.split_once("://") {
        let open = SCHEMES
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(n, _)| n == scheme)
            .map(|(_, o)| *o)
            .ok_or_else(|| {
                Error::feature(format!(
                    "no module keeps backups at {scheme}:// (a local directory is an absolute path)"
                ))
            })?;
        return open(spec).map_err(Error::invalid_parameter);
    }
    let fs = kv
        .files()
        .ok_or_else(|| Error::feature("this database has no files to keep backups beside"))?;
    let root = PathBuf::from(spec);
    // From the root: on Windows a path without a drive (`\backups`) is
    // the current drive's.
    if !root.has_root() {
        return Err(Error::invalid_parameter(format!(
            "a repository of backups is an absolute path or a URL, not '{spec}'"
        )));
    }
    Ok(Box::new(DirTarget { fs, root }))
}

fn target_error(t: &dyn Target, what: &str, name: &str, e: std::io::Error) -> Error {
    Error::io(format!("could not {what} {name} in {}: {e}", t.describe()))
}

fn put(t: &dyn Target, name: &str, data: &[u8]) -> Result<()> {
    t.put(name, data)
        .map_err(|e| target_error(t, "write", name, e))
}

fn get(t: &dyn Target, name: &str) -> Result<Option<Vec<u8>>> {
    t.get(name).map_err(|e| target_error(t, "read", name, e))
}

/// The backups in a repository (none if it is not there), by number.
fn listing(t: &dyn Target) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    for name in t.list("").map_err(|e| target_error(t, "list", "", e))? {
        if name == MANIFEST {
            return Err(Error::invalid_parameter(format!(
                "{} holds one backup (as made before repositories of backups): \
                 back up to a new directory",
                t.describe()
            )));
        }
        match backup_number(&name) {
            Some(n) => out.push((n, name)),
            None if name.starts_with('.') => {}
            None => {
                return Err(Error::invalid_parameter(format!(
                    "{} is not a repository of backups (it holds {name}): \
                     back up to a new or empty directory, or a repository",
                    t.describe()
                )));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A backup's manifest, if it has one (one without did not finish).
fn read_manifest(t: &dyn Target, dir: &str) -> Result<Option<Manifest>> {
    let name = if dir.is_empty() {
        MANIFEST.to_string()
    } else {
        format!("{dir}/{MANIFEST}")
    };
    match get(t, &name)? {
        None => Ok(None),
        Some(b) => manifest(&String::from_utf8_lossy(&b)).map(Some),
    }
}

/// The branch a backup keeps for the next one to compare with: of the
/// repository (its path hashed) and the backup's number.
fn backup_branch(repo: &str, number: u64) -> String {
    format!("{}{number:06}", backup_branch_prefix(repo))
}

/// The names of the branches backups keep start so.
pub const BRANCH_PREFIX: &str = "nc-backup-";

fn backup_branch_prefix(repo: &str) -> String {
    format!(
        "{BRANCH_PREFIX}{}-",
        crate::audit::hex(&sha256(repo.as_bytes())[..6])
    )
}

/// Drops every branch a backup kept for the next one to compare with —
/// `FORGET SUBJECT`'s, before its scrub: what such a branch shares with
/// the database is what an erasure removes, and kept it would stay in
/// the files.  The repositories' next backups are full ones.
pub(crate) fn drop_backup_branches(kv: &Arc<dyn Kv>) -> Result<()> {
    for b in kv.branches()? {
        if b.name.starts_with(BRANCH_PREFIX) {
            kv.drop_branch(&b.name)?;
        }
    }
    Ok(())
}

/// Drops the branch made for a backup unless it is kept (the backup
/// done).
struct Made<'a> {
    kv: &'a Arc<dyn Kv>,
    name: String,
    keep: bool,
}

impl Drop for Made<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = self.kv.drop_branch(&self.name);
        }
    }
}

/// What a backup writes: its parts, entry by entry, each about a subject
/// sealed.
struct Writer<'a> {
    parts: Parts<'a>,
    nonces: Nonces,
    subjects: Subjects,
    keys: HashMap<Id, Option<(KeyId, Key)>>,
    data_keys: HashMap<u32, Option<(Key, Vec<KeyId>)>>,
    /// The indexes left out, by table id.
    derived: HashMap<u32, HashSet<u32>>,
    out: Backup,
}

/// Whether `k` is an entry of an index of `derived`.
fn is_derived(derived: &HashMap<u32, HashSet<u32>>, k: &[u8]) -> bool {
    k.first() == Some(&DATA)
        && k.len() >= catalog::INDEX_PREFIX_LEN
        && derived
            .get(&u32::from_be_bytes(k[1..5].try_into().expect("4")))
            .is_some_and(|ix| ix.contains(&u32::from_be_bytes(k[5..9].try_into().expect("4"))))
}

impl Writer<'_> {
    /// A set's key and its keys' ids (`None`: one of them destroyed).
    fn data_key(&mut self, set: u32) -> Option<(Key, Vec<KeyId>)> {
        if let Some(k) = self.data_keys.get(&set) {
            return k.clone();
        }
        let ks: Option<Vec<(KeyId, Key)>> = self.subjects.sets[set as usize]
            .iter()
            .map(|id| self.keys.get(id).copied().flatten())
            .collect();
        let k = ks.map(|ks| {
            let (kids, keys): (Vec<KeyId>, Vec<Key>) = ks.into_iter().unzip();
            (subject_keys::data_key(&keys), kids)
        });
        self.data_keys.insert(set, k.clone());
        k
    }

    /// Writes entry `k`, `v` — sealed if it is about a subject.
    fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        if k.starts_with(&STATS) || is_derived(&self.derived, k) {
            return Ok(());
        }
        // An entry about a row of subjects: sealed.
        if let Some((table, pk, is_row)) = row_of(k)
            && let Some(set) = self.subjects.of(table, pk)
        {
            let Some((key, kids)) = self.data_key(set) else {
                self.out.omitted += 1;
                return Ok(());
            };
            let mut plain = Vec::with_capacity(k.len() + v.len() + 8);
            plain.extend_from_slice(&(k.len() as u32).to_le_bytes());
            plain.extend_from_slice(k);
            plain.extend_from_slice(v);
            let row_table = if is_row { table } else { u32::MAX };
            let nonce = self.nonces.next();
            let sealed = aead::seal(
                &key,
                &nonce,
                &aad(SEALED, &[&row_table.to_le_bytes()], &kids),
                &plain,
            );
            let buf = &mut self.parts.buf;
            buf.push(SEALED);
            buf.extend_from_slice(&row_table.to_le_bytes());
            put_kids(buf, &kids);
            buf.extend_from_slice(&nonce);
            self.parts.bytes(&sealed);
            self.out.entries += 1;
            self.out.sealed += 1;
            return self.parts.end_entry();
        }
        // An audit record naming a subject: its detail sealed.
        if k.starts_with(&AUDIT)
            && !self.subjects.texts.is_empty()
            && let Some((blank, detail)) = crate::audit::split_detail(v)?
        {
            // Its literals: a subject named, or a text of a subject's rows.
            let mut ids: Vec<Id> = Vec::new();
            for l in literals(&detail) {
                if let Some(id) = self.subjects.texts.get(&l) {
                    ids.push(*id);
                }
                if l.len() >= MIN_TEXT
                    && let Some(&set) = self.subjects.heads.get(&head(&l))
                {
                    ids.extend_from_slice(&self.subjects.sets[set as usize]);
                }
            }
            if !ids.is_empty() {
                let set = self.subjects.number(ids);
                match self.data_key(set) {
                    Some((key, kids)) => {
                        let nonce = self.nonces.next();
                        let sealed = aead::seal(
                            &key,
                            &nonce,
                            &aad(SEALED_DETAIL, &[k, &blank], &kids),
                            detail.as_bytes(),
                        );
                        self.parts.buf.push(SEALED_DETAIL);
                        self.parts.bytes(k);
                        self.parts.bytes(&blank);
                        put_kids(&mut self.parts.buf, &kids);
                        self.parts.buf.extend_from_slice(&nonce);
                        self.parts.bytes(&sealed);
                        self.out.sealed += 1;
                    }
                    // Forgotten meanwhile: blank.
                    None => {
                        self.parts.buf.push(PLAIN);
                        self.parts.bytes(k);
                        self.parts.bytes(&crate::audit::join_detail(&blank, None)?);
                    }
                }
                self.out.entries += 1;
                return self.parts.end_entry();
            }
        }
        self.parts.buf.push(PLAIN);
        self.parts.bytes(k);
        self.parts.bytes(v);
        self.out.entries += 1;
        self.parts.end_entry()
    }

    /// Writes that `k` is gone — its key sealed under `sealing` (the keys
    /// of the subjects it was about, as the backup before saw them; `None`
    /// in it: one destroyed since, the entry left out).
    fn delete(&mut self, k: &[u8], sealing: Option<Option<(Key, Vec<KeyId>)>>) -> Result<()> {
        match sealing {
            None => {
                self.parts.buf.push(DELETE);
                self.parts.bytes(k);
            }
            Some(None) => {
                self.out.omitted += 1;
                return Ok(());
            }
            Some(Some((key, kids))) => {
                let nonce = self.nonces.next();
                let sealed = aead::seal(&key, &nonce, &aad(SEALED_DELETE, &[], &kids), k);
                self.parts.buf.push(SEALED_DELETE);
                put_kids(&mut self.parts.buf, &kids);
                self.parts.buf.extend_from_slice(&nonce);
                self.parts.bytes(&sealed);
                self.out.sealed += 1;
            }
        }
        self.out.deleted += 1;
        self.parts.end_entry()
    }
}

/// The subjects of the backup before, for the deletes of an incremental
/// one: the rows about subjects as its snapshot saw them, and their keys.
struct Before<'a> {
    txn: &'a dyn KvTxn,
    commit: u64,
    tables: Vec<Table>,
    pepper: Option<Key>,
    subjects: Option<Subjects>,
    keys: HashMap<u32, Option<(Key, Vec<KeyId>)>>,
}

impl Before<'_> {
    /// How a delete of `k` is written: in the clear (`None`), or sealed
    /// under its subjects' keys (`Some(None)`: one destroyed).
    fn sealing(&mut self, k: &[u8]) -> Result<Option<Option<(Key, Vec<KeyId>)>>> {
        let Some((table, pk, _)) = row_of(k) else {
            return Ok(None);
        };
        let Some(store) = subject_keys::subject_keys() else {
            return Ok(None);
        };
        if self.subjects.is_none() {
            if !self.tables.iter().any(|t| !t.subject.is_empty()) {
                return Ok(None);
            }
            self.subjects = Some(subjects(self.txn, &self.tables, self.pepper.as_ref())?);
        }
        let subjects = self.subjects.as_ref().expect("made");
        let Some(set) = subjects.of(table, pk) else {
            return Ok(None);
        };
        if let Some(k) = self.keys.get(&set) {
            return Ok(Some(k.clone()));
        }
        let ids = &subjects.sets[set as usize];
        let got: Option<Vec<(KeyId, Key)>> = store
            .keys(ids, self.commit)
            .map_err(Error::io)?
            .into_iter()
            .collect();
        let k = got.map(|ks| {
            let (kids, keys): (Vec<KeyId>, Vec<Key>) = ks.into_iter().unzip();
            (subject_keys::data_key(&keys), kids)
        });
        self.keys.insert(set, k.clone());
        Ok(Some(k))
    }
}

/// The commit a snapshot reads.
fn snapshot_commit(txn: &dyn KvTxn) -> Result<u64> {
    txn.read_ts()
        .to_string()
        .parse()
        .map_err(|_| Error::internal("a snapshot's commit"))
}

fn as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// `BACKUP [FULL] TO 'repo'`: the database as one snapshot saw it, into
/// repository `repo` (a new or empty directory, or one `BACKUP` wrote; an
/// absolute path on the server) — all of it, or what changed since the
/// repository's last backup, as a backup of its own.
///
/// What changed is found as ZFS's `send` finds it: the backup reads a
/// branch made for it (PLAN.md §M14.13: a copy in constant time), and
/// keeps it until the next backup, which compares its own branch's tree
/// with it ([`Kv::changed_since`]) — the subtrees they share skipped
/// whole — and the values of the keys in what differs.  The kept branch
/// holds on to the pages it shares, as a snapshot does; `FULL`, or the
/// branch dropped, starts a new chain.
///
/// With `keep`, the repository's backups but the last `keep` full ones
/// and those after them are removed once this one is done ([`prune`]).
/// Each backup, done or failed, is counted in the metrics
/// (`nucleus_backup_*`; the alerts read them).
pub fn backup(kv: &Arc<dyn Kv>, to: &str, full: bool, keep: Option<u64>) -> Result<Backup> {
    let started = std::time::Instant::now();
    let r = backup_once(kv, to, full).and_then(|mut b| {
        if let Some(keep) = keep {
            b.removed = prune(kv, to, keep)?.len() as u64;
        }
        Ok(b)
    });
    let now = kv
        .wall_clock()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    crate::metrics::backup_done(r.as_ref().ok().map(|b| b.bytes), started.elapsed(), now);
    r
}

/// Retention (PLAN.md §M14.18 O3c): repository `to`'s backups older
/// than its last `keep` full ones removed — whole chains, oldest first,
/// each backup's manifest before its parts (a directory without one is no
/// backup: a removal cut short leaves none half there).  The names of
/// the backups removed.
pub fn prune(kv: &Arc<dyn Kv>, to: &str, keep: u64) -> Result<Vec<String>> {
    if keep == 0 {
        return Err(Error::invalid_parameter("KEEP is at least 1"));
    }
    let target = open_target(kv, to)?;
    let t = &*target;
    let listed = listing(t)?;
    let mut fulls = Vec::new();
    for (n, name) in &listed {
        if name.ends_with("-full") && read_manifest(t, name)?.is_some() {
            fulls.push(*n);
        }
    }
    let Some(first_kept) = fulls.len().checked_sub(keep as usize).map(|i| fulls[i]) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for (n, name) in listed {
        if n >= first_kept {
            break;
        }
        let gone = |f: &str| {
            let path = format!("{name}/{f}");
            t.delete(&path)
                .map_err(|e| target_error(t, "remove", &path, e))
        };
        gone(MANIFEST)?;
        for f in t
            .list(&name)
            .map_err(|e| target_error(t, "list", &name, e))?
        {
            gone(&f)?;
        }
        removed.push(name);
    }
    Ok(removed)
}

/// What a restore test found (PLAN.md §M14.18 O4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tested {
    /// The backup restored (its directory).
    pub backup: String,
    /// What the restore did.
    pub restored: Restored,
    /// What the checks found wrong: none when the backup is good.
    pub problems: Vec<String>,
}

/// The file a restore test leaves in the backup it tested.
const RESTORE_TEST: &str = "restore-test";

/// A restore test (PLAN.md §M14.18 O4): repository `to`'s last backup
/// restored into `scratch` (a new, empty database, the caller's to throw
/// away), then checked as the crash tests check a recovery — catalog,
/// rows and indexes agree, every foreign key holds ([`crate::check`]),
/// the audit chain holds ([`crate::audit::check`]).  A restore that
/// fails is a problem found too.  The result is left in the backup
/// (`restore-test`: when, and whether it held) — what a schedule reads
/// at its start — and counted in the metrics (`nucleus_restore_test*`).
/// `kv` is the database whose repository it is (a directory is its file
/// system's).
pub fn restore_test(
    kv: &Arc<dyn Kv>,
    scratch: &Arc<dyn Kv>,
    reg: &Registry,
    to: &str,
) -> Result<Option<Tested>> {
    let Some(last) = last_backup(kv, to)? else {
        return Ok(None);
    };
    let started = std::time::Instant::now();
    let mut t = Tested {
        backup: last.name.clone(),
        restored: Restored::default(),
        problems: Vec::new(),
    };
    match restore_to(scratch, reg, to, &Until::Name(last.name.clone())) {
        Err(e) => t
            .problems
            .push(format!("the restore failed: {}", e.message)),
        Ok(r) => {
            t.restored = r;
            let txn = scratch.begin(TxnOptions::read_only())?;
            t.problems.extend(crate::check::check(&*txn, reg)?);
            let chain = crate::audit::check(&*txn)?;
            if !chain.intact {
                t.problems.push(format!(
                    "the audit chain breaks at record {}: {}",
                    chain.broken_at.unwrap_or(0),
                    chain.problem.unwrap_or_default()
                ));
            }
        }
    }
    let now = kv
        .wall_clock()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    crate::metrics::restore_test_done(t.problems.is_empty(), started.elapsed(), now / 1_000_000);
    let target = open_target(kv, to)?;
    let mut record = format!(
        "nucleus restore test 1\ntime {now}\ncommit {}\nresult {}\n",
        t.restored.commit,
        if t.problems.is_empty() {
            "ok"
        } else {
            "failed"
        }
    );
    for p in &t.problems {
        record.push_str(&format!("problem {}\n", p.replace('\n', " ")));
    }
    put(
        &*target,
        &format!("{}/{RESTORE_TEST}", last.name),
        record.as_bytes(),
    )?;
    // And in the database's audit log (when it is not the scratch one).
    if !Arc::ptr_eq(kv, scratch) {
        let detail = format!(
            "{} of {to}: {}",
            last.name,
            if t.problems.is_empty() {
                "it holds".to_string()
            } else {
                t.problems.join("; ")
            }
        );
        let mut r = crate::audit::record_of(
            &crate::access::Who::system(),
            "restore test",
            t.problems.is_empty(),
            if t.problems.is_empty() { "" } else { "XX001" },
            &detail,
        );
        r.at = crate::datetime::from_system(kv.wall_clock());
        let mut attempt = 0;
        loop {
            let mut txn = kv.begin(TxnOptions::default())?;
            match crate::audit::append(txn.as_mut(), r.clone()).and_then(|()| Ok(txn.commit()?)) {
                Err(e) if e.code == "40001" && attempt < 20 => {
                    attempt += 1;
                    kv.back_off(attempt);
                }
                done => {
                    done?;
                    break;
                }
            }
        }
    }
    Ok(Some(t))
}

/// The last restore test of repository `to`: its backup, when (µs since
/// the Unix epoch), and whether it held — of the last 50 backups.
pub fn last_restore_test(kv: &Arc<dyn Kv>, to: &str) -> Result<Option<(String, u64, bool)>> {
    let target = open_target(kv, to)?;
    let t = &*target;
    for (_, name) in listing(t)?.into_iter().rev().take(50) {
        let Some(b) = get(t, &format!("{name}/{RESTORE_TEST}"))? else {
            continue;
        };
        let text = String::from_utf8_lossy(&b);
        let field = |f: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(f)?.strip_prefix(' '))
                .map(str::to_string)
        };
        let time = field("time").and_then(|t| t.parse().ok()).unwrap_or(0);
        return Ok(Some((name, time, field("result").as_deref() == Some("ok"))));
    }
    Ok(None)
}

/// A repository's last finished backup, for a schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Last {
    /// Its directory.
    pub name: String,
    /// When it was made, in microseconds since the Unix epoch.
    pub time: u64,
    /// The incremental backups since the last full one, it included.
    pub since_full: u64,
}

/// Repository `to`'s last finished backup, if any.
pub fn last_backup(kv: &Arc<dyn Kv>, to: &str) -> Result<Option<Last>> {
    let target = open_target(kv, to)?;
    let t = &*target;
    let mut last: Option<Last> = None;
    for (_, name) in listing(t)?.into_iter().rev() {
        let Some(m) = read_manifest(t, &name)? else {
            continue;
        };
        let l = last.get_or_insert_with(|| Last {
            name: name.clone(),
            time: m.time,
            since_full: 0,
        });
        if !m.incremental {
            break;
        }
        l.since_full += 1;
    }
    Ok(last)
}

fn backup_once(kv: &Arc<dyn Kv>, to: &str, full: bool) -> Result<Backup> {
    let target = open_target(kv, to)?;
    let target = &*target;
    let listed = listing(target)?;
    let number = listed.last().map_or(1, |(n, _)| n + 1);
    // The last backup finished, and what it kept to compare with.
    let mut last = None;
    for (_, name) in listed.iter().rev() {
        if let Some(m) = read_manifest(target, name)? {
            last = Some((name.clone(), m));
            break;
        }
    }
    // Encrypted with the key set, or neither: a chain is read with one key.
    let key = backup_key();
    let source = kv.branch_name();
    let branches: HashSet<String> = kv.branches()?.into_iter().map(|b| b.name).collect();
    let mut base = None;
    if !full
        && let Some((name, m)) = &last
        && let Some(branch) = &m.branch
        && m.source.as_deref() == Some(source.as_str())
        && m.key_id.as_deref() == key.as_ref().map(BackupKey::id)
        && branches.contains(branch)
    {
        let b = kv.branch(branch)?;
        let txn = b.begin(TxnOptions::read_only())?;
        // Unchanged since that backup read it.
        if snapshot_commit(&*txn)? == m.commit {
            base = Some((name.clone(), m.commit, branch.clone(), b, txn));
        }
    }
    let incremental = base.is_some();
    let name = backup_name(number, incremental);
    // The branch read, kept for the next backup.
    let made = Made {
        kv,
        name: backup_branch(to, number),
        keep: false,
    };
    let snap = kv.create_branch(&made.name)?;
    let txn = snap.begin(TxnOptions::read_only())?;
    let commit = snapshot_commit(&*txn)?;
    let tables = catalog::tables(&*txn)?;
    let store = subject_keys::subject_keys();
    let pepper = match &store {
        Some(s) if tables.iter().any(|t| !t.subject.is_empty()) => {
            s.pepper(true).map_err(Error::io)?
        }
        _ => None,
    };
    let subjects = subjects(&*txn, &tables, pepper.as_ref())?;
    // Every subject's key, made now if it has none.
    let mut keys: HashMap<Id, Option<(KeyId, Key)>> = HashMap::new();
    if let Some(store) = &store {
        let ids: Vec<Id> = subjects
            .sets
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !ids.is_empty() {
            let got = store.keys(&ids, commit).map_err(Error::io)?;
            keys.extend(ids.into_iter().zip(got));
        }
    }
    let derived: HashMap<u32, HashSet<u32>> = tables
        .iter()
        .map(|t| (t.id, t.indexes.iter().map(|i| i.id).collect()))
        .collect();
    let mut nonces = Nonces {
        base: [0; aead::NONCE_LEN],
        n: 0,
    };
    kv.random_bytes(&mut nonces.base);
    let mut part_nonces = Nonces {
        base: [0; aead::NONCE_LEN],
        n: 0,
    };
    kv.random_bytes(&mut part_nonces.base);
    let mut w = Writer {
        parts: Parts {
            target,
            dir: name.clone(),
            buf: Vec::with_capacity(PART_BYTES + (1 << 16)),
            key: key.clone(),
            nonces: part_nonces,
            done: Vec::new(),
        },
        nonces,
        subjects,
        keys,
        data_keys: HashMap::new(),
        derived,
        out: Backup {
            commit,
            kind: if incremental { "incremental" } else { "full" },
            name: name.clone(),
            ..Backup::default()
        },
    };
    let mut analyzed = BTreeSet::new();
    for e in txn.scan(Bound::Included(&STATS), Bound::Excluded(&[0x00, 0x05]))? {
        let (k, _) = e?;
        if k.len() >= 6 {
            analyzed.insert(u32::from_be_bytes(k[2..6].try_into().expect("4")));
        }
    }
    let mut base_line = String::new();
    match &base {
        None => {
            for e in txn.scan(Bound::Unbounded, Bound::Unbounded)? {
                let (k, v) = e?;
                w.put(&k, &v)?;
            }
        }
        Some((base_name, base_commit, branch, _, base_txn)) => {
            base_line = format!("base {base_name}\n");
            let mut before = Before {
                txn: &**base_txn,
                commit: *base_commit,
                tables: catalog::tables(&**base_txn)?,
                pepper,
                subjects: None,
                keys: HashMap::new(),
            };
            let derived_before: HashMap<u32, HashSet<u32>> = before
                .tables
                .iter()
                .map(|t| (t.id, t.indexes.iter().map(|i| i.id).collect()))
                .collect();
            let ranges = snap
                .changed_since(branch)?
                .unwrap_or_else(|| vec![(Bound::Unbounded, Bound::Unbounded)]);
            for (start, end) in &ranges {
                let mut a = txn.scan(as_ref(start), as_ref(end))?;
                let mut b = base_txn.scan(as_ref(start), as_ref(end))?;
                let (mut x, mut y) = (a.next().transpose()?, b.next().transpose()?);
                loop {
                    let order = match (&x, &y) {
                        (None, None) => break,
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (Some((ka, _)), Some((kb, _))) => ka.cmp(kb),
                    };
                    match order {
                        std::cmp::Ordering::Less => {
                            let (k, v) = x.take().expect("one");
                            w.put(&k, &v)?;
                            x = a.next().transpose()?;
                        }
                        std::cmp::Ordering::Greater => {
                            let (k, _) = y.take().expect("one");
                            if !k.starts_with(&STATS)
                                && !is_derived(&w.derived, &k)
                                && !is_derived(&derived_before, &k)
                            {
                                let sealing = before.sealing(&k)?;
                                w.delete(&k, sealing)?;
                            }
                            y = b.next().transpose()?;
                        }
                        std::cmp::Ordering::Equal => {
                            let ((k, v), (_, old)) =
                                (x.take().expect("one"), y.take().expect("one"));
                            if v != old {
                                w.put(&k, &v)?;
                            }
                            x = a.next().transpose()?;
                            y = b.next().transpose()?;
                        }
                    }
                }
            }
        }
    }
    drop(txn);
    w.parts.flush()?;
    let mut out = w.out;
    out.bytes = w.parts.done.iter().map(|p| p.1).sum();
    // The manifest, last.
    let mut m = format!(
        "{FORMAT}\nkind {}\n{base_line}branch {}\nsource {source}\ncommit {commit}\ntime {}\n\
         entries {}\nsealed {}\nomitted {}\ndeleted {}\n",
        out.kind,
        made.name,
        kv.wall_clock()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        out.entries,
        out.sealed,
        out.omitted,
        out.deleted,
    );
    m.push_str("compression lz4-block\n");
    if let Some(key) = &key {
        m.push_str(&format!("encryption chacha20-poly1305 {}\n", key.id));
    }
    m.push_str("analyzed");
    for t in &analyzed {
        m.push_str(&format!(" {t}"));
    }
    m.push('\n');
    for (name, len, hash) in &w.parts.done {
        m.push_str(&format!("part {name} {len} {}\n", crate::audit::hex(hash)));
    }
    // Authenticated with the key: the manifest names the parts (their
    // hashes), so all of the backup is.
    if let Some(key) = &key {
        let mac = nucleus_crypto::hmac_sha256(&key.key, m.as_bytes());
        m.push_str(&format!("mac {}\n", crate::audit::hex(&mac)));
    }
    put(target, &format!("{name}/{MANIFEST}"), m.as_bytes())?;
    // Done: this backup's branch kept, the repository's older ones
    // (the last backup's, any a failure left) dropped.
    let mut made = made;
    made.keep = true;
    drop(base);
    drop(snap);
    let prefix = backup_branch_prefix(to);
    for b in branches {
        if b.starts_with(&prefix) && b != made.name {
            let _ = kv.drop_branch(&b);
        }
    }
    Ok(out)
}

/// What a restore did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The last backup's snapshot.
    pub commit: u64,
    /// The backups read: a full one, and the incremental ones after it.
    pub backups: u64,
    /// The entries written.
    pub entries: u64,
    /// Of them opened (sealed in the backup, their subjects' keys there).
    pub opened: u64,
    /// The entries deleted (an incremental backup's).
    pub deleted: u64,
    /// Entries left out: a subject's key destroyed.
    pub omitted: u64,
    /// Audit records whose detail was blanked: a subject's key destroyed.
    pub blanked: u64,
    /// Indexes rebuilt.
    pub indexes: u64,
    /// Rows whose keys were set to `NULL` or their defaults, the rows they
    /// referenced left out.
    pub nulled: u64,
}

/// A manifest read.
struct Manifest {
    commit: u64,
    /// When it was made, in microseconds since the Unix epoch.
    time: u64,
    incremental: bool,
    /// The backup an incremental one follows: its directory's name.
    base: Option<String>,
    /// The branch it kept for the next one.
    branch: Option<String>,
    /// The branch it backed up.
    source: Option<String>,
    analyzed: Vec<u32>,
    parts: Vec<(String, u64, String)>,
    /// The entries its parts hold: written, and deleted.
    entries: Option<u64>,
    deleted: u64,
    /// Its parts compressed (LZ4's block format).
    compressed: bool,
    /// Its parts encrypted: the key's id.
    key_id: Option<String>,
    /// What its MAC covers, and the MAC (hexadecimal).
    mac: Option<(String, String)>,
}

impl Manifest {
    /// Checked against the key set: one encrypted needs that key, and its
    /// MAC must match; with a key, one not encrypted is refused.
    fn check(&self, dir: &str, key: Option<&BackupKey>) -> Result<()> {
        match (key, &self.key_id) {
            (None, None) => Ok(()),
            (None, Some(id)) => Err(Error::invalid_parameter(format!(
                "{dir} is encrypted (key {id}): restoring it needs its key (--backup-key)"
            ))),
            (Some(_), None) => Err(Error::invalid_parameter(format!(
                "{dir} is not encrypted, and a backup key is set: a backup made without \
                 it is not restored with it"
            ))),
            (Some(k), Some(id)) if *id != k.id => Err(Error::invalid_parameter(format!(
                "{dir} is encrypted with key {id}, not this one ({})",
                k.id
            ))),
            (Some(k), Some(_)) => {
                let ok = self.mac.as_ref().is_some_and(|(signed, mac)| {
                    let want =
                        crate::audit::hex(&nucleus_crypto::hmac_sha256(&k.key, signed.as_bytes()));
                    nucleus_crypto::constant_time_eq(want.as_bytes(), mac.as_bytes())
                });
                if ok {
                    Ok(())
                } else {
                    Err(Error::data_exception(format!(
                        "{dir}'s manifest does not match its MAC: it was altered"
                    )))
                }
            }
        }
    }

    /// A part as stored, checked against the manifest and opened: decrypted
    /// and decompressed.
    fn open_part(&self, path: &str, b: Vec<u8>, key: Option<&BackupKey>) -> Result<Vec<u8>> {
        let damaged = || {
            Error::data_exception(format!(
                "{path} is damaged: it does not decrypt or decompress"
            ))
        };
        let z = match (&self.key_id, key) {
            (None, _) => b,
            (Some(_), Some(k)) => {
                if b.len() < aead::NONCE_LEN {
                    return Err(damaged());
                }
                let (nonce, sealed) = b.split_at(aead::NONCE_LEN);
                let nonce: [u8; aead::NONCE_LEN] = nonce.try_into().expect("nonce");
                aead::open(&k.key, &nonce, path.as_bytes(), sealed).ok_or_else(damaged)?
            }
            (Some(id), None) => {
                return Err(Error::invalid_parameter(format!("{path}: needs key {id}")));
            }
        };
        if !self.compressed {
            return Ok(z);
        }
        let n = z
            .get(..4)
            .map(|n| u32::from_le_bytes(n.try_into().expect("4")) as usize)
            .ok_or_else(damaged)?;
        let mut out = Vec::with_capacity(n);
        nucleus_kv::lz::decompress(&z[4..], n, &mut out).ok_or_else(damaged)?;
        Ok(out)
    }
}

fn manifest(text: &str) -> Result<Manifest> {
    let bad = |why: &str| Error::invalid_parameter(format!("not a backup's manifest: {why}"));
    let mut lines = text.lines();
    if !matches!(lines.next(), Some(FORMAT | FORMAT_1)) {
        return Err(bad(&format!("no \"{FORMAT}\"")));
    }
    let mut m = Manifest {
        commit: 0,
        time: 0,
        incremental: false,
        base: None,
        branch: None,
        source: None,
        analyzed: Vec::new(),
        parts: Vec::new(),
        compressed: false,
        key_id: None,
        mac: None,
        entries: None,
        deleted: 0,
    };
    if let Some(i) = text.rfind("\nmac ") {
        let mac = text[i + 5..].trim();
        m.mac = Some((text[..=i].to_string(), mac.to_string()));
    }
    let name = |s: &str| -> Result<String> {
        if s.contains(['/', '\\']) || s.starts_with('.') {
            return Err(bad(s));
        }
        Ok(s.to_string())
    };
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["commit", n] => m.commit = n.parse().map_err(|_| bad(line))?,
            ["time", n] => m.time = n.parse().map_err(|_| bad(line))?,
            ["entries", n] => m.entries = Some(n.parse().map_err(|_| bad(line))?),
            ["deleted", n] => m.deleted = n.parse().map_err(|_| bad(line))?,
            ["kind", k] => m.incremental = *k == "incremental",
            ["base", b] => m.base = Some(name(b)?),
            ["branch", b] => m.branch = Some((*b).to_string()),
            ["source", b] => m.source = Some((*b).to_string()),
            ["compression", "lz4-block"] => m.compressed = true,
            ["compression", c] => return Err(bad(&format!("compression {c} is not known"))),
            ["encryption", "chacha20-poly1305", id] => m.key_id = Some((*id).to_string()),
            ["encryption", ..] => return Err(bad(line)),
            ["analyzed", ids @ ..] => {
                for id in ids {
                    m.analyzed.push(id.parse().map_err(|_| bad(line))?);
                }
            }
            ["part", part, len, hash] => {
                m.parts.push((
                    name(part)?,
                    len.parse().map_err(|_| bad(line))?,
                    (*hash).to_string(),
                ));
            }
            _ => {}
        }
    }
    if m.incremental && m.base.is_none() {
        return Err(bad("an incremental backup without its base"));
    }
    Ok(m)
}

/// Which backup of a repository a restore makes the database of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Until {
    /// The last.
    Latest,
    /// The last whose snapshot is commit `n` or before.
    Commit(u64),
    /// The last made by then: microseconds since the Unix epoch.
    Time(u64),
    /// The one in this directory of the repository.
    Name(String),
}

/// The backups a restore reads, in order — a full one, and the
/// incremental ones after it up to the one `until` names — with their
/// directories.
fn chain(t: &dyn Target, start: Option<String>, until: &Until) -> Result<Vec<(String, Manifest)>> {
    let (mut dir, mut m) = match start {
        // A backup named: its own directory.
        Some(name) => {
            let m = read_manifest(t, &name)?.ok_or_else(|| {
                Error::invalid_parameter(format!("{name} in {} is no backup", t.describe()))
            })?;
            (name, m)
        }
        None => {
            let mut found = None;
            for (_, name) in listing(t)?.into_iter().rev() {
                let Some(m) = read_manifest(t, &name)? else {
                    continue;
                };
                let fits = match until {
                    Until::Latest => true,
                    Until::Commit(c) => m.commit <= *c,
                    Until::Time(time) => m.time <= *time,
                    Until::Name(n) => *n == name,
                };
                if fits {
                    found = Some((name, m));
                    break;
                }
            }
            found.ok_or_else(|| {
                Error::invalid_parameter(format!(
                    "{} holds no backup {}",
                    t.describe(),
                    match until {
                        Until::Latest => "finished".to_string(),
                        Until::Commit(c) => format!("of commit {c} or before"),
                        Until::Time(_) => "made by then".to_string(),
                        Until::Name(n) => format!("named {n}"),
                    }
                ))
            })?
        }
    };
    let mut out = Vec::new();
    while let Some(base) = m.base.clone().filter(|_| m.incremental) {
        let next = read_manifest(t, &base)?.ok_or_else(|| {
            Error::data_exception(format!(
                "{dir} follows {base}, which is missing or unfinished"
            ))
        })?;
        // (The same commit: nothing written between.)
        if next.commit > m.commit {
            return Err(Error::data_exception(format!(
                "{dir} follows {base}, a later backup"
            )));
        }
        out.push((
            std::mem::replace(&mut dir, base),
            std::mem::replace(&mut m, next),
        ));
    }
    out.push((dir, m));
    out.reverse();
    Ok(out)
}

/// `spec` without its last part, and that part: a backup's own directory
/// named is that backup of the repository around it.
fn split_last(spec: &str) -> Option<(String, String)> {
    let trimmed = spec.trim_end_matches(['/', '\\']);
    let i = trimmed.rfind(['/', '\\'])?;
    let (parent, name) = (&trimmed[..i], &trimmed[i + 1..]);
    let parent = if parent.is_empty() || parent.ends_with(':') {
        &trimmed[..=i]
    } else {
        parent
    };
    (!name.is_empty()).then(|| (parent.to_string(), name.to_string()))
}

/// Reads a part's entries.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| Error::data_exception("a backup's part ends inside an entry"))?;
        self.at += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2")))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn kids(&mut self) -> Result<Vec<KeyId>> {
        let n = self.u16()? as usize;
        (0..n)
            .map(|_| Ok(self.take(16)?.try_into().expect("16")))
            .collect()
    }

    fn nonce(&mut self) -> Result<[u8; aead::NONCE_LEN]> {
        Ok(self.take(aead::NONCE_LEN)?.try_into().expect("nonce"))
    }
}

/// The key of the entries sealed under the keys `kids` — `None` once one
/// of them is destroyed (or no store is set).
fn restore_key(
    store: Option<&Arc<dyn SubjectKeys>>,
    cache: &mut HashMap<Vec<KeyId>, Option<Key>>,
    kids: Vec<KeyId>,
) -> Result<Option<Key>> {
    if let Some(k) = cache.get(&kids) {
        return Ok(*k);
    }
    let k = match store {
        None => None,
        Some(s) => {
            let got: Option<Vec<Key>> = s.open(&kids).map_err(Error::io)?.into_iter().collect();
            got.map(|ks| subject_keys::data_key(&ks))
        }
    };
    cache.insert(kids, k);
    Ok(k)
}

fn tampered() -> Error {
    Error::data_exception(
        "an entry of the backup does not open with its subjects' keys: \
         the backup was changed, or the keys are another database's",
    )
}

/// Makes a new database of the last backup in repository `from` (or of
/// the backup in directory `from`): [`restore_to`] the latest.
pub fn restore(kv: &Arc<dyn Kv>, reg: &Registry, from: &str) -> Result<Restored> {
    restore_to(kv, reg, from, &Until::Latest)
}

/// Makes a new database of the backup of repository `from` that `until`
/// names (or of the backup in directory `from`): `kv` holds nothing yet
/// (none of its tables made).  The full backup's entries written, then
/// each incremental one's after it — puts and deletes — up to that one;
/// the indexes rebuilt from the rows and the tables that had statistics
/// analyzed again; an entry whose subject's key is destroyed is left
/// out, an audit record's detail blanked, a key referencing a row left
/// out set to `NULL` or its default as declared.
pub fn restore_to(kv: &Arc<dyn Kv>, reg: &Registry, from: &str, until: &Until) -> Result<Restored> {
    let mut target = open_target(kv, from)?;
    let mut start = None;
    // A backup's own directory: that backup of the repository around it.
    if read_manifest(&*target, "")?.is_some() {
        let (parent, name) = split_last(from).ok_or_else(|| {
            Error::invalid_parameter(format!("{from}: a backup's directory needs one around it"))
        })?;
        target = open_target(kv, &parent)?;
        start = Some(name);
    }
    let target = &*target;
    let chain = chain(target, start, until)?;
    {
        let txn = kv.begin(TxnOptions::read_only())?;
        if let Some(e) = txn
            .scan(Bound::Included(&[0x00, 0x01][..]), Bound::Unbounded)?
            .next()
        {
            e?;
            return Err(Error::prerequisite(
                "a backup is restored into a new database: this one holds data",
            ));
        }
    }
    let store = subject_keys::subject_keys();
    let mut cache = HashMap::new();
    let (_, m) = chain.last().expect("one");
    let mut out = Restored {
        commit: m.commit,
        backups: chain.len() as u64,
        ..Restored::default()
    };
    // The tables whose rows were left out, by id.
    let mut lost: HashSet<u32> = HashSet::new();
    let mut txn = kv.begin(TxnOptions::default())?;
    let mut batch = 0usize;
    let key = backup_key();
    for (dir, m) in &chain {
        m.check(dir, key.as_ref())?;
    }
    // The entries each backup's parts hold, against its manifest's count.
    let mut counted: HashMap<&str, u64> = HashMap::new();
    let parts = chain
        .iter()
        .flat_map(|(dir, m)| m.parts.iter().map(move |p| (dir, m, p)));
    for (dir, m, (name, len, hash)) in parts {
        let path = format!("{dir}/{name}");
        let b = get(target, &path)?.ok_or_else(|| {
            Error::data_exception(format!("{path} is missing from {}", target.describe()))
        })?;
        if b.len() as u64 != *len || crate::audit::hex(&sha256(&b)) != *hash {
            return Err(Error::data_exception(format!(
                "{} is not the part the manifest names (its length or SHA-256 differ)",
                path
            )));
        }
        let b = m.open_part(&path, b, key.as_ref())?;
        let mut r = Reader { b: &b, at: 0 };
        let read = counted.entry(dir.as_str()).or_insert(0);
        while r.at < b.len() {
            *read += 1;
            let kind = r.u8()?;
            // A delete (an incremental backup's).
            let gone = match kind {
                DELETE => Some(r.bytes()?.to_vec()),
                SEALED_DELETE => {
                    let kids = r.kids()?;
                    let nonce = r.nonce()?;
                    let sealed = r.bytes()?;
                    let a = aad(SEALED_DELETE, &[], &kids);
                    match restore_key(store.as_ref(), &mut cache, kids)? {
                        Some(key) => {
                            out.opened += 1;
                            Some(aead::open(&key, &nonce, &a, sealed).ok_or_else(tampered)?)
                        }
                        // Its subject forgotten: the entry it deletes left
                        // out too.
                        None => {
                            out.omitted += 1;
                            continue;
                        }
                    }
                }
                _ => None,
            };
            if let Some(k) = gone {
                batch += k.len();
                txn.delete(&k)?;
                out.deleted += 1;
                if batch >= RESTORE_BATCH {
                    txn.commit()?;
                    txn = kv.begin(TxnOptions::default())?;
                    batch = 0;
                }
                continue;
            }
            let (k, v): (Vec<u8>, Vec<u8>) = match kind {
                PLAIN => (r.bytes()?.to_vec(), r.bytes()?.to_vec()),
                SEALED => {
                    let table = r.u32()?;
                    let kids = r.kids()?;
                    let nonce = r.nonce()?;
                    let sealed = r.bytes()?;
                    let a = aad(SEALED, &[&table.to_le_bytes()], &kids);
                    let Some(key) = restore_key(store.as_ref(), &mut cache, kids)? else {
                        out.omitted += 1;
                        if table != u32::MAX {
                            lost.insert(table);
                        }
                        continue;
                    };
                    let plain = aead::open(&key, &nonce, &a, sealed).ok_or_else(tampered)?;
                    let mut p = Reader { b: &plain, at: 0 };
                    let k = p.bytes()?.to_vec();
                    out.opened += 1;
                    (k, plain[p.at..].to_vec())
                }
                SEALED_DETAIL => {
                    let k = r.bytes()?.to_vec();
                    let blank = r.bytes()?.to_vec();
                    let kids = r.kids()?;
                    let nonce = r.nonce()?;
                    let sealed = r.bytes()?;
                    let a = aad(SEALED_DETAIL, &[&k, &blank], &kids);
                    let detail = match restore_key(store.as_ref(), &mut cache, kids)? {
                        Some(key) => {
                            let d = aead::open(&key, &nonce, &a, sealed).ok_or_else(tampered)?;
                            out.opened += 1;
                            Some(String::from_utf8(d).map_err(|_| tampered())?)
                        }
                        None => {
                            out.blanked += 1;
                            None
                        }
                    };
                    let v = crate::audit::join_detail(&blank, detail)?;
                    (k, v)
                }
                other => {
                    return Err(Error::data_exception(format!(
                        "an entry of an unknown kind ({other}) in {}",
                        path
                    )));
                }
            };
            batch += k.len() + v.len();
            txn.put(&k, &v)?;
            out.entries += 1;
            if batch >= RESTORE_BATCH {
                txn.commit()?;
                txn = kv.begin(TxnOptions::default())?;
                batch = 0;
            }
        }
    }
    txn.commit()?;
    for (dir, m) in &chain {
        let read = counted.get(dir.as_str()).copied().unwrap_or(0);
        if let Some(want) = m.entries.map(|n| n + m.deleted)
            && read != want
        {
            return Err(Error::data_exception(format!(
                "{dir} holds {read} entries, its manifest {want}"
            )));
        }
    }
    let system = Arc::new(crate::access::Who::system());
    let now = crate::datetime::from_system(kv.wall_clock());
    crate::access::with_who(&system, || {
        crate::datetime::with_now(now, kv, || rebuild(kv, reg, m, &lost, &mut out))
    })?;
    Ok(out)
}

/// A restore's second half: the indexes, the keys referencing rows left
/// out, the statistics.
fn rebuild(
    kv: &Arc<dyn Kv>,
    reg: &Registry,
    m: &Manifest,
    lost: &HashSet<u32>,
    out: &mut Restored,
) -> Result<()> {
    let tables = {
        let txn = kv.begin(TxnOptions::read_only())?;
        catalog::tables(&*txn)?
    };
    for t in &tables {
        for i in 0..t.indexes.len() {
            let mut txn = kv.begin(TxnOptions::default())?;
            crate::dml::backfill_index(txn.as_mut(), reg, t, i)?;
            txn.commit()?;
            out.indexes += 1;
        }
    }
    // Keys set to NULL or their defaults where the rows they referenced
    // were left out (a partition's rows lost count for its table).
    let lost_names: HashSet<&str> = tables
        .iter()
        .filter(|t| lost.contains(&t.id))
        .flat_map(|t| {
            std::iter::once(t.name.as_str()).chain(t.partition_of.as_ref().map(|(p, _)| p.as_str()))
        })
        .collect();
    let quoted = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    for t in tables.iter().filter(|t| t.partition_of.is_none()) {
        for fk in &t.foreign_keys {
            if follows(fk) || !lost_names.contains(fk.table.as_str()) {
                continue;
            }
            let Some(p) = tables.iter().find(|p| p.name == fk.table) else {
                continue;
            };
            let to = if fk.on_delete == FkAction::SetNull {
                "NULL"
            } else {
                "DEFAULT"
            };
            let set: Vec<String> = fk
                .columns
                .iter()
                .map(|&c| format!("{} = {to}", quoted(&t.columns[c].name)))
                .collect();
            let present: Vec<String> = fk
                .columns
                .iter()
                .map(|&c| format!("{} IS NOT NULL", quoted(&t.columns[c].name)))
                .collect();
            let joined: Vec<String> = fk
                .columns
                .iter()
                .zip(&fk.ref_columns)
                .map(|(&c, &r)| {
                    format!(
                        "p.{} = {}.{}",
                        quoted(&p.columns[r].name),
                        quoted(&t.name),
                        quoted(&t.columns[c].name)
                    )
                })
                .collect();
            let sql = format!(
                "UPDATE {} SET {} WHERE {} AND NOT EXISTS (SELECT 1 FROM {} p WHERE {})",
                quoted(&t.name),
                set.join(", "),
                present.join(" AND "),
                quoted(&p.name),
                joined.join(" AND ")
            );
            let stmt = crate::parser::parse(&sql)?.remove(0);
            let mut txn = kv.begin(TxnOptions::default())?;
            let mut all = crate::session::Collect::default();
            let o = crate::session::execute_statement(txn.as_mut(), reg, &stmt, &[], &mut all)?;
            txn.commit()?;
            if let crate::session::Output::Command(tag) = o {
                out.nulled += tag
                    .rsplit(' ')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
    }
    for id in &m.analyzed {
        if let Some(t) = tables.iter().find(|t| t.id == *id) {
            let mut txn = kv.begin(TxnOptions::default())?;
            crate::stats::analyze(txn.as_mut(), Some(&t.name))?;
            txn.commit()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statements_literals_are_read_unescaped() {
        assert_eq!(
            literals("REMEMBER 'it''s tea' FOR 'ann' IN notes -- 'open"),
            ["it's tea", "ann"]
        );
        assert!(literals("SELECT 1").is_empty());
    }

    #[test]
    fn a_manifest_names_its_parts_within_its_directory() {
        let m = manifest(&format!(
            "{FORMAT}\ncommit 7\nanalyzed 3 4\npart part-000000 10 ab\n"
        ))
        .unwrap();
        assert_eq!((m.commit, m.analyzed, m.parts.len()), (7, vec![3, 4], 1));
        assert!(manifest(&format!("{FORMAT}\npart ../x 1 ab\n")).is_err());
        assert!(manifest("something else").is_err());
    }
}
