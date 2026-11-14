//! Subjects' keys (PLAN.md §M14.24 R6): what a backup seals each entry
//! about a subject with, kept apart from the data and its backups, and
//! destroyed by `FORGET SUBJECT` — the backups taken before then hold the
//! subject's rows, but no longer anything that opens them
//! (crypto-shredding).
//!
//! A subject is named by its *id*: HMAC-SHA256 of its text (its value
//! cast to `text`, as `FORGET SUBJECT` compares it) under a *pepper* kept
//! with the keys — so a backup alone shows neither whose rows it holds
//! nor, by guessing names, whether someone's are there.  Each subject has
//! a random 32-byte key; an entry about several subjects (a message
//! between two people) is sealed under the SHA-256 of all their keys, in
//! id order: forgetting any one of them makes it unreadable, as it
//! deletes the row.
//!
//! Each key has an id of its own (16 random bytes) that a backup names it
//! by: a subject forgotten, then written again, gets a new key, and the
//! old backups still name the old one — destroyed.
//!
//! A destroyed key leaves a mark with the commit of the `FORGET` that
//! destroyed it: a backup whose snapshot is older (begun before the
//! erasure, still running after it) gets no new key for the subject — the
//! rows it would seal are left out — while one taken after gets a new key
//! for the subject's new rows.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use nucleus_crypto::{hmac_sha256, sha256};

/// A subject's id: HMAC-SHA256 of its text under the pepper.
pub type Id = [u8; 32];
/// A key: 32 random bytes.
pub type Key = [u8; 32];
/// A key's own id: 16 random bytes.
pub type KeyId = [u8; 16];

/// Where subjects' keys are kept.
pub trait SubjectKeys: Send + Sync {
    /// The pepper ids are made under; with `create`, made (and kept) if
    /// there is none yet.
    fn pepper(&self, create: bool) -> Result<Option<Key>, String>;

    /// The keys of the subjects `ids` for a backup whose snapshot is
    /// `snapshot`, in their order, with their own ids: those missing made
    /// and kept before this returns — but not one destroyed by an erasure
    /// committed after that snapshot (`None`).
    fn keys(&self, ids: &[Id], snapshot: u64) -> Result<Vec<Option<(KeyId, Key)>>, String>;

    /// The keys named `kids` (a restore), in their order: `None` for one
    /// destroyed.
    fn open(&self, kids: &[KeyId]) -> Result<Vec<Option<Key>>, String>;

    /// Destroys the keys of `ids` (an erasure committed at `commit`),
    /// durably before this returns; how many there were.
    fn destroy(&self, ids: &[Id], commit: u64) -> Result<u64, String>;

    /// Where they are, for messages (never a key).
    fn describe(&self) -> String;
}

/// A subject's state in a [`KeyTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Key(KeyId),
    /// Destroyed by the erasure committed then.
    Destroyed(u64),
}

/// The keys as a store keeps them, in memory: what [`MemoryKeys`] and a
/// file of keys (the server's) share — the rules and the text form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyTable {
    pepper: Option<Key>,
    subjects: BTreeMap<Id, State>,
    keys: BTreeMap<KeyId, Key>,
}

/// The text form's first line.
const HEADER: &str = "nucleus subject keys 1";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N {
        return None;
    }
    let mut k = [0u8; N];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

impl KeyTable {
    /// The pepper; with `create`, made from `random` if there is none.
    /// Whether it changed comes back too.
    pub fn pepper(
        &mut self,
        create: bool,
        random: &mut dyn FnMut(&mut [u8]),
    ) -> (Option<Key>, bool) {
        if self.pepper.is_none() && create {
            let mut p = [0u8; 32];
            random(&mut p);
            self.pepper = Some(p);
            return (self.pepper, true);
        }
        (self.pepper, false)
    }

    /// [`SubjectKeys::keys`]; whether the table changed comes back too.
    pub fn keys(
        &mut self,
        ids: &[Id],
        snapshot: u64,
        random: &mut dyn FnMut(&mut [u8]),
    ) -> (Vec<Option<(KeyId, Key)>>, bool) {
        let mut changed = false;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let make = match self.subjects.get(id) {
                Some(State::Key(kid)) => {
                    out.push(self.keys.get(kid).map(|k| (*kid, *k)));
                    continue;
                }
                None => true,
                Some(State::Destroyed(at)) => snapshot > *at,
            };
            if make {
                let mut kid = [0u8; 16];
                let mut k = [0u8; 32];
                // Never an id already given (16 random bytes: in theory).
                while kid == [0u8; 16] || self.keys.contains_key(&kid) {
                    random(&mut kid);
                }
                random(&mut k);
                self.subjects.insert(*id, State::Key(kid));
                self.keys.insert(kid, k);
                changed = true;
                out.push(Some((kid, k)));
            } else {
                out.push(None);
            }
        }
        (out, changed)
    }

    /// [`SubjectKeys::open`].
    pub fn open(&self, kids: &[KeyId]) -> Vec<Option<Key>> {
        kids.iter().map(|k| self.keys.get(k).copied()).collect()
    }

    /// [`SubjectKeys::destroy`]: every id marked, the latest erasure's
    /// commit kept.
    pub fn destroy(&mut self, ids: &[Id], commit: u64) -> u64 {
        let mut n = 0;
        for id in ids {
            let at = match self.subjects.get(id) {
                Some(State::Key(kid)) => {
                    self.keys.remove(kid);
                    n += 1;
                    commit
                }
                Some(State::Destroyed(at)) => (*at).max(commit),
                None => commit,
            };
            self.subjects.insert(*id, State::Destroyed(at));
        }
        n
    }

    /// The text form: a header, the pepper, a line a subject (`k id
    /// key-id key`, or `d id commit` once destroyed).
    pub fn to_text(&self) -> String {
        let mut s = format!("{HEADER}\n");
        if let Some(p) = &self.pepper {
            s.push_str(&format!("pepper {}\n", hex(p)));
        }
        for (id, st) in &self.subjects {
            match st {
                State::Key(kid) => s.push_str(&format!(
                    "k {} {} {}\n",
                    hex(id),
                    hex(kid),
                    hex(&self.keys[kid])
                )),
                State::Destroyed(at) => s.push_str(&format!("d {} {at}\n", hex(id))),
            }
        }
        s
    }

    /// Reads [`to_text`](Self::to_text)'s form.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some(HEADER) {
            return Err(format!("not a file of subject keys (no \"{HEADER}\")"));
        }
        let mut t = KeyTable::default();
        for (n, line) in lines.enumerate() {
            let bad = || format!("line {}: malformed", n + 2);
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                [] => {}
                ["pepper", p] => t.pepper = Some(unhex(p).ok_or_else(bad)?),
                ["k", id, kid, k] => {
                    let kid: KeyId = unhex(kid).ok_or_else(bad)?;
                    t.subjects
                        .insert(unhex(id).ok_or_else(bad)?, State::Key(kid));
                    t.keys.insert(kid, unhex(k).ok_or_else(bad)?);
                }
                ["d", id, at] => {
                    t.subjects.insert(
                        unhex(id).ok_or_else(bad)?,
                        State::Destroyed(at.parse().map_err(|_| bad())?),
                    );
                }
                _ => return Err(bad()),
            }
        }
        Ok(t)
    }
}

/// Keys kept in memory (tests; an application keeping them itself): gone
/// with the process.
pub struct MemoryKeys {
    table: Mutex<KeyTable>,
    random: Box<dyn Fn(&mut [u8]) + Send + Sync>,
}

impl MemoryKeys {
    /// Keys made from `random`'s bytes (a database's
    /// [`nucleus_kv::Kv::random_bytes`]).
    pub fn new(random: impl Fn(&mut [u8]) + Send + Sync + 'static) -> Self {
        Self {
            table: Mutex::new(KeyTable::default()),
            random: Box::new(random),
        }
    }

    /// The keys as they are (tests).
    pub fn table(&self) -> KeyTable {
        self.table.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl SubjectKeys for MemoryKeys {
    fn pepper(&self, create: bool) -> Result<Option<Key>, String> {
        let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
        Ok(t.pepper(create, &mut |b| (self.random)(b)).0)
    }

    fn keys(&self, ids: &[Id], snapshot: u64) -> Result<Vec<Option<(KeyId, Key)>>, String> {
        let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
        Ok(t.keys(ids, snapshot, &mut |b| (self.random)(b)).0)
    }

    fn open(&self, kids: &[KeyId]) -> Result<Vec<Option<Key>>, String> {
        Ok(self
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open(kids))
    }

    fn destroy(&self, ids: &[Id], commit: u64) -> Result<u64, String> {
        let mut t = self.table.lock().unwrap_or_else(|e| e.into_inner());
        Ok(t.destroy(ids, commit))
    }

    fn describe(&self) -> String {
        "keys kept in memory".into()
    }
}

static KEYS: RwLock<Option<Arc<dyn SubjectKeys>>> = RwLock::new(None);

/// Sets the process's store of subject keys (`None`: none — a backup of
/// a database with subject columns is refused, and `FORGET` has no keys
/// to destroy).
pub fn set_subject_keys(k: Option<Arc<dyn SubjectKeys>>) {
    *KEYS.write().unwrap_or_else(|e| e.into_inner()) = k;
}

/// The process's store of subject keys.
pub fn subject_keys() -> Option<Arc<dyn SubjectKeys>> {
    KEYS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The id of the subject `text` under `pepper`.
pub fn id(pepper: &Key, text: &str) -> Id {
    hmac_sha256(pepper, text.as_bytes())
}

/// The key an entry about the subjects whose keys are `keys` (in their
/// ids' order) is sealed with.
pub fn data_key(keys: &[Key]) -> Key {
    let mut b = Vec::with_capacity(32 + 32 * keys.len());
    b.extend_from_slice(b"nucleus subject data key 1\0");
    for k in keys {
        b.extend_from_slice(k);
    }
    sha256(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter() -> impl FnMut(&mut [u8]) {
        let mut n = 0u8;
        move |b: &mut [u8]| {
            n += 1;
            b.fill(n);
        }
    }

    #[test]
    fn a_destroyed_key_is_not_made_again_for_an_older_snapshot() {
        let mut t = KeyTable::default();
        let mut r = counter();
        let a = [1; 32];
        let (k, changed) = t.keys(&[a], 10, &mut r);
        let (kid, _) = k[0].unwrap();
        assert!(changed);
        assert_eq!(t.open(&[kid]), vec![Some(k[0].unwrap().1)]);
        assert_eq!(t.destroy(&[a], 20), 1);
        assert_eq!(t.open(&[kid]), vec![None]);
        // A backup begun before the erasure: nothing.
        assert_eq!(t.keys(&[a], 15, &mut r).0, vec![None]);
        // One after: a new key, under an id of its own; the old stays gone.
        let (k2, _) = t.keys(&[a], 25, &mut r);
        let (kid2, key2) = k2[0].unwrap();
        assert!(kid2 != kid && key2 != k[0].unwrap().1);
        assert_eq!(t.open(&[kid, kid2]), vec![None, Some(key2)]);
        // Destroying an id never keyed still marks it.
        let b = [2; 32];
        assert_eq!(t.destroy(&[b], 30), 0);
        assert_eq!(t.keys(&[b], 29, &mut r).0, vec![None]);
    }

    #[test]
    fn the_text_form_reads_back() {
        let mut t = KeyTable::default();
        let mut r = counter();
        t.pepper(true, &mut r);
        t.keys(&[[3; 32], [4; 32]], 1, &mut r);
        t.destroy(&[[4; 32]], 7);
        let text = t.to_text();
        assert_eq!(KeyTable::from_text(&text).unwrap(), t);
        assert!(KeyTable::from_text("something else").is_err());
    }
}
