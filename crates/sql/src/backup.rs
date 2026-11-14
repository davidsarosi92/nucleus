//! Backups (PLAN.md §M14.24 R6, the slice of §M14.18 it needs):
//! `BACKUP TO 'dir'` writes the database as one snapshot saw it, while
//! others go on writing; `restore` (`nucleus restore`) makes a new
//! database of it.
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
//! The files: parts of about 16 MiB (`part-000000`, …), a sequence of
//! entries each; a `manifest`, written last and renamed into place, names
//! the format, the snapshot, the counts and each part's SHA-256 — no
//! manifest, no backup.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
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
pub const FORMAT: &str = "nucleus backup 1";
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
}

/// The columns `BACKUP` returns.
pub const RESULT: [(&str, DataType); 5] = [
    ("commit", DataType::BigInt),
    ("entries", DataType::BigInt),
    ("sealed", DataType::BigInt),
    ("omitted", DataType::BigInt),
    ("bytes", DataType::BigInt),
];

impl Backup {
    /// As `BACKUP`'s row.
    pub fn row(&self) -> Vec<Value> {
        [
            self.commit,
            self.entries,
            self.sealed,
            self.omitted,
            self.bytes,
        ]
        .map(|n| Value::Int(n as i64))
        .to_vec()
    }
}

fn io(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::io(format!("{what} {}: {e}", path.display()))
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
    fs: &'a dyn FileSystem,
    dir: PathBuf,
    buf: Vec<u8>,
    done: Vec<(String, u64, [u8; 32])>,
}

impl Parts<'_> {
    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let name = format!("part-{:06}", self.done.len());
        let path = self.dir.join(&name);
        let f = self
            .fs
            .open(&path, OpenMode::CreateNew)
            .map_err(|e| io("could not create", &path, e))?;
        f.write_all_at(&self.buf, 0)
            .and_then(|()| f.sync())
            .map_err(|e| io("could not write", &path, e))?;
        self.done
            .push((name, self.buf.len() as u64, sha256(&self.buf)));
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

/// `BACKUP TO 'dir'`: the database as one snapshot saw it, into `dir`
/// (a new or empty directory, an absolute path on the server).
pub fn backup(kv: &Arc<dyn Kv>, to: &str) -> Result<Backup> {
    let fs = kv
        .files()
        .ok_or_else(|| Error::feature("this database has no files to back up beside"))?;
    let dir = PathBuf::from(to);
    if !dir.is_absolute() {
        return Err(Error::invalid_parameter(format!(
            "BACKUP TO needs an absolute path, not '{to}'"
        )));
    }
    if fs
        .exists(&dir)
        .map_err(|e| io("could not look at", &dir, e))?
        && !fs
            .list_dir(&dir)
            .map_err(|e| io("could not list", &dir, e))?
            .is_empty()
    {
        return Err(Error::invalid_parameter(format!(
            "{to} is not empty: a backup goes to a new or empty directory"
        )));
    }
    let txn = kv.begin(TxnOptions::read_only())?;
    let commit: u64 = txn
        .read_ts()
        .to_string()
        .parse()
        .map_err(|_| Error::internal("a snapshot's commit"))?;
    let tables = catalog::tables(&*txn)?;
    let store = subject_keys::subject_keys();
    let pepper = match &store {
        Some(s) if tables.iter().any(|t| !t.subject.is_empty()) => {
            s.pepper(true).map_err(Error::io)?
        }
        _ => None,
    };
    let mut subjects = subjects(&*txn, &tables, pepper.as_ref())?;
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
    // A set's key and its keys' ids (`None`: one of them destroyed).
    let mut data_keys: HashMap<u32, Option<(Key, Vec<KeyId>)>> = HashMap::new();
    let mut data_key = |subjects: &Subjects, set: u32| -> Option<(Key, Vec<KeyId>)> {
        data_keys
            .entry(set)
            .or_insert_with(|| {
                let ks: Option<Vec<(KeyId, Key)>> = subjects.sets[set as usize]
                    .iter()
                    .map(|id| keys.get(id).copied().flatten())
                    .collect();
                ks.map(|ks| {
                    let (kids, keys): (Vec<KeyId>, Vec<Key>) = ks.into_iter().unzip();
                    (subject_keys::data_key(&keys), kids)
                })
            })
            .clone()
    };
    // The indexes left out, by table id.
    let derived: HashMap<u32, HashSet<u32>> = tables
        .iter()
        .map(|t| (t.id, t.indexes.iter().map(|i| i.id).collect()))
        .collect();
    fs.create_dir_all(&dir)
        .map_err(|e| io("could not create", &dir, e))?;
    let mut parts = Parts {
        fs,
        dir: dir.clone(),
        buf: Vec::with_capacity(PART_BYTES + (1 << 16)),
        done: Vec::new(),
    };
    let mut nonces = Nonces {
        base: [0; aead::NONCE_LEN],
        n: 0,
    };
    kv.random_bytes(&mut nonces.base);
    let mut out = Backup {
        commit,
        ..Backup::default()
    };
    let mut analyzed = BTreeSet::new();
    for e in txn.scan(Bound::Unbounded, Bound::Unbounded)? {
        let (k, v) = e?;
        if k.starts_with(&STATS) {
            if k.len() >= 6 {
                analyzed.insert(u32::from_be_bytes(k[2..6].try_into().expect("4")));
            }
            continue;
        }
        if k.first() == Some(&DATA)
            && k.len() >= catalog::INDEX_PREFIX_LEN
            && derived
                .get(&u32::from_be_bytes(k[1..5].try_into().expect("4")))
                .is_some_and(|ix| ix.contains(&u32::from_be_bytes(k[5..9].try_into().expect("4"))))
        {
            continue;
        }
        // An entry about a row of subjects: sealed.
        if let Some((table, pk, is_row)) = row_of(&k)
            && let Some(set) = subjects.of(table, pk)
        {
            let Some((key, kids)) = data_key(&subjects, set) else {
                out.omitted += 1;
                continue;
            };
            let mut plain = Vec::with_capacity(k.len() + v.len() + 8);
            plain.extend_from_slice(&(k.len() as u32).to_le_bytes());
            plain.extend_from_slice(&k);
            plain.extend_from_slice(&v);
            let row_table = if is_row { table } else { u32::MAX };
            let nonce = nonces.next();
            let sealed = aead::seal(
                &key,
                &nonce,
                &aad(SEALED, &[&row_table.to_le_bytes()], &kids),
                &plain,
            );
            parts.buf.push(SEALED);
            parts.buf.extend_from_slice(&row_table.to_le_bytes());
            put_kids(&mut parts.buf, &kids);
            parts.buf.extend_from_slice(&nonce);
            parts.bytes(&sealed);
            out.entries += 1;
            out.sealed += 1;
            parts.end_entry()?;
            continue;
        }
        // An audit record naming a subject: its detail sealed.
        if k.starts_with(&AUDIT)
            && !subjects.texts.is_empty()
            && let Some((blank, detail)) = crate::audit::split_detail(&v)?
        {
            // Its literals: a subject named, or a text of a subject's rows.
            let mut ids: Vec<Id> = Vec::new();
            for l in literals(&detail) {
                if let Some(id) = subjects.texts.get(&l) {
                    ids.push(*id);
                }
                if l.len() >= MIN_TEXT
                    && let Some(&set) = subjects.heads.get(&head(&l))
                {
                    ids.extend_from_slice(&subjects.sets[set as usize]);
                }
            }
            if !ids.is_empty() {
                let set = subjects.number(ids);
                match data_key(&subjects, set) {
                    Some((key, kids)) => {
                        let nonce = nonces.next();
                        let sealed = aead::seal(
                            &key,
                            &nonce,
                            &aad(SEALED_DETAIL, &[&k, &blank], &kids),
                            detail.as_bytes(),
                        );
                        parts.buf.push(SEALED_DETAIL);
                        parts.bytes(&k);
                        parts.bytes(&blank);
                        put_kids(&mut parts.buf, &kids);
                        parts.buf.extend_from_slice(&nonce);
                        parts.bytes(&sealed);
                        out.sealed += 1;
                    }
                    // Forgotten meanwhile: blank.
                    None => {
                        parts.buf.push(PLAIN);
                        parts.bytes(&k);
                        parts.bytes(&crate::audit::join_detail(&blank, None)?);
                    }
                }
                out.entries += 1;
                parts.end_entry()?;
                continue;
            }
        }
        parts.buf.push(PLAIN);
        parts.bytes(&k);
        parts.bytes(&v);
        out.entries += 1;
        parts.end_entry()?;
    }
    drop(txn);
    parts.flush()?;
    out.bytes = parts.done.iter().map(|p| p.1).sum();
    // The manifest, last.
    let mut m = format!(
        "{FORMAT}\ncommit {commit}\ntime {}\nentries {}\nsealed {}\nomitted {}\n",
        kv.wall_clock()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        out.entries,
        out.sealed,
        out.omitted,
    );
    m.push_str("analyzed");
    for t in &analyzed {
        m.push_str(&format!(" {t}"));
    }
    m.push('\n');
    for (name, len, hash) in &parts.done {
        m.push_str(&format!("part {name} {len} {}\n", crate::audit::hex(hash)));
    }
    let tmp = dir.join("manifest.tmp");
    let f = fs
        .open(&tmp, OpenMode::CreateNew)
        .map_err(|e| io("could not create", &tmp, e))?;
    f.write_all_at(m.as_bytes(), 0)
        .and_then(|()| f.sync())
        .map_err(|e| io("could not write", &tmp, e))?;
    drop(f);
    let path = dir.join(MANIFEST);
    fs.rename(&tmp, &path)
        .and_then(|()| fs.sync_dir(&dir))
        .map_err(|e| io("could not write", &path, e))?;
    Ok(out)
}

/// What a restore did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The backup's snapshot.
    pub commit: u64,
    /// The entries written.
    pub entries: u64,
    /// Of them opened (sealed in the backup, their subjects' keys there).
    pub opened: u64,
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
    analyzed: Vec<u32>,
    parts: Vec<(String, u64, String)>,
}

fn manifest(text: &str) -> Result<Manifest> {
    let bad = |why: &str| Error::invalid_parameter(format!("not a backup's manifest: {why}"));
    let mut lines = text.lines();
    if lines.next() != Some(FORMAT) {
        return Err(bad(&format!("no \"{FORMAT}\"")));
    }
    let mut m = Manifest {
        commit: 0,
        analyzed: Vec::new(),
        parts: Vec::new(),
    };
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["commit", n] => m.commit = n.parse().map_err(|_| bad(line))?,
            ["analyzed", ids @ ..] => {
                for id in ids {
                    m.analyzed.push(id.parse().map_err(|_| bad(line))?);
                }
            }
            ["part", name, len, hash] => {
                if name.contains(['/', '\\']) || name.starts_with('.') {
                    return Err(bad(line));
                }
                m.parts.push((
                    (*name).to_string(),
                    len.parse().map_err(|_| bad(line))?,
                    (*hash).to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(m)
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

/// Makes a new database of the backup in `from`: `kv` holds nothing yet
/// (none of its tables made).  The entries written, the indexes rebuilt
/// from the rows and the tables that had statistics analyzed again; an
/// entry whose subject's key is destroyed is left out, an audit record's
/// detail blanked, a key referencing a row left out set to `NULL` or its
/// default as declared.
pub fn restore(kv: &Arc<dyn Kv>, reg: &Registry, from: &Path) -> Result<Restored> {
    let fs = kv
        .files()
        .ok_or_else(|| Error::feature("this database has no files to restore from"))?;
    let read = |path: &Path| -> Result<Vec<u8>> {
        let f = fs
            .open(path, OpenMode::ReadOnly)
            .map_err(|e| io("could not open", path, e))?;
        let len = f.size().map_err(|e| io("could not read", path, e))?;
        let mut b = vec![0u8; len as usize];
        f.read_exact_at(&mut b, 0)
            .map_err(|e| io("could not read", path, e))?;
        Ok(b)
    };
    let text = read(&from.join(MANIFEST))?;
    let m = manifest(&String::from_utf8_lossy(&text))?;
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
    let mut out = Restored {
        commit: m.commit,
        ..Restored::default()
    };
    // The tables whose rows were left out, by id.
    let mut lost: HashSet<u32> = HashSet::new();
    let mut txn = kv.begin(TxnOptions::default())?;
    let mut batch = 0usize;
    for (name, len, hash) in &m.parts {
        let path = from.join(name);
        let b = read(&path)?;
        if b.len() as u64 != *len || crate::audit::hex(&sha256(&b)) != *hash {
            return Err(Error::data_exception(format!(
                "{} is not the part the manifest names (its length or SHA-256 differ)",
                path.display()
            )));
        }
        let mut r = Reader { b: &b, at: 0 };
        while r.at < b.len() {
            let (k, v): (Vec<u8>, Vec<u8>) = match r.u8()? {
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
                        path.display()
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
    let system = Arc::new(crate::access::Who::system());
    let now = crate::datetime::from_system(kv.wall_clock());
    crate::access::with_who(&system, || {
        crate::datetime::with_now(now, kv, || rebuild(kv, reg, &m, &lost, &mut out))
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
