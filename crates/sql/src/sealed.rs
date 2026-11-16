//! Sealed rows (PLAN.md §M14.30 K4a): a table storage that keeps each
//! row as [`crate::storage::RowStorage`] does, its stored value sealed
//! under the key of its subjects — so destroying a subject's key
//! (`FORGET SUBJECT`) leaves every copy of its rows unreadable at once:
//! the versions history keeps, branches made before, the log, pages not
//! yet scrubbed, backups.
//!
//! The keys are R6's ([`crate::subject_keys`]): a row about the subjects
//! named by its subject columns (`SET SUBJECT`) is sealed under their
//! [`data_key`], the key ids written in the value's header in the ids'
//! order.  AES-256-GCM-SIV ([`nucleus_kernels::aes`], the CPU's
//! instructions) under a key derived from it, the row key and the header
//! as associated data, so a sealed value opens only where it was written.
//!
//! A stored value: the tag [`TAG`], a version, the number of key ids and
//! the ids (16 bytes each), the nonce, the sealed length (`u32`), then
//! the sealed bytes — the whole row's encoding, every value inline — up to
//! [`crate::large::CHUNK`] of them; the rest in chunks under the table's
//! [`crate::large::LARGE_INDEX`], column [`CHUNKS_COLUMN`].  A row whose
//! key was destroyed reads as absent, as forgetting it deleted it.  A row
//! still stored plain (the table being converted, `SET STORAGE sealed`)
//! reads as the `row` storage reads it.
//!
//! What stays in the clear is said in PLAN.md: the primary key, the
//! subject columns (the only secondary indexes a sealed table takes —
//! [`check_index`]), how many rows and versions each subject has.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Bound;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use nucleus_kernels::aes;
use nucleus_kv::{KvTxn, prefix_end};

use crate::catalog::{self, Table};
use crate::enums::Labels;
use crate::error::{Error, Result};
use crate::exec::pk_suffix;
use crate::storage::{RowIter, Scan, TableStorage};
use crate::subject_keys::{self, Id, Key, KeyId, data_key};
use crate::types::{DataType, Row, Value};

/// The storage's name.
pub const SEALED: &str = "sealed";

/// A sealed value's first byte (a plain row's is its encoding's version,
/// 1 to 3).
pub const TAG: u8 = 0xE5;
/// The format's version.
const VERSION: u8 = 1;
/// The column number a sealed row's chunks are kept under.
pub const CHUNKS_COLUMN: u16 = u16::MAX;

/// Whether a stored value is sealed.
pub fn is_sealed(stored: &[u8]) -> bool {
    stored.first() == Some(&TAG)
}

/// The columns that say whose a row of `t` is: its own, or for a
/// partition its partitioned table's (by name).
fn subject_columns(txn: &dyn KvTxn, t: &Table) -> Result<Vec<usize>> {
    if !t.subject.is_empty() {
        return Ok(t.subject.clone());
    }
    if let Some((parent, _)) = &t.partition_of
        && let Some(p) = catalog::get_table(txn, parent)?
        && !p.subject.is_empty()
    {
        return p
            .subject
            .iter()
            .map(|&c| {
                t.column(&p.columns[c].name).ok_or_else(|| {
                    Error::internal(format!(
                        "{}: no column {} of {parent}",
                        t.name, p.columns[c].name
                    ))
                })
            })
            .collect();
    }
    Err(Error::prerequisite(format!(
        "\"{}\" is stored sealed and says nothing of whose its rows are: \
         ALTER TABLE {} SET SUBJECT (column, …) first",
        t.name, t.name
    )))
}

fn store() -> Result<Arc<dyn subject_keys::SubjectKeys>> {
    subject_keys::subject_keys().ok_or_else(|| {
        Error::prerequisite(
            "a sealed table's rows are sealed under their subjects' keys, and no store of \
             subject keys is set (nucleus_sql::subject_keys::set_subject_keys)",
        )
    })
}

/// A value as `FORGET SUBJECT` compares it: cast to text.
pub(crate) fn subject_text(v: &Value) -> String {
    match v.clone().cast(DataType::Text) {
        Ok(Value::Text(s)) => s.to_string(),
        _ => v.render(),
    }
}

/// The AES key rows are sealed with under `data`, the subjects' data key
/// (the same key seals a backup's entries with ChaCha20-Poly1305: one
/// derived for each use).
fn row_key(data: &Key) -> aes::Key {
    aes::Key::new(&nucleus_crypto::hmac_sha256(data, b"nucleus sealed rows 1"))
}

/// A fresh nonce: a base made once per process, unpredictable, and a
/// counter — never the same twice in a process, and AES-GCM-SIV keeps
/// its secrecy should two processes' ever meet.
fn nonce() -> [u8; aes::NONCE_LEN] {
    static BASE: OnceLock<[u8; aes::NONCE_LEN]> = OnceLock::new();
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let base = BASE.get_or_init(|| {
        let mut b = [0u8; aes::NONCE_LEN];
        if crate::datetime::random_bytes(&mut b).is_err() {
            // No statement running: the host's hasher seeds and the time.
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            b[..8].copy_from_slice(&h.finish().to_le_bytes());
            let h2 = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            b[8..].copy_from_slice(&h2.to_le_bytes()[..4]);
        }
        b
    });
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let mut out = *base;
    for (o, c) in out[4..].iter_mut().zip(n.to_le_bytes()) {
        *o ^= c;
    }
    out
}

/// What a sealed value's header holds.
struct Header<'a> {
    kids: Vec<KeyId>,
    nonce: [u8; aes::NONCE_LEN],
    /// The subject columns' values, sealed under the store's key for who a
    /// row is ([`who_key`]; the tag's included).
    who: &'a [u8],
    /// The sealed length (the tag's included).
    len: usize,
    /// The header's bytes (associated data).
    raw: &'a [u8],
    /// The sealed bytes kept with it.
    inline: &'a [u8],
}

fn header(stored: &[u8]) -> Result<Header<'_>> {
    let bad = || Error::internal("a sealed row's header does not read");
    if stored.len() < 3 || stored[0] != TAG {
        return Err(bad());
    }
    if stored[1] != VERSION {
        return Err(Error::feature(format!(
            "a sealed row of version {}: written by a later nucleus",
            stored[1]
        )));
    }
    let n = stored[2] as usize;
    let at = 3 + 16 * n;
    let who_at = at + aes::NONCE_LEN + 2;
    if n == 0 || stored.len() < who_at {
        return Err(bad());
    }
    let who_len = u16::from_le_bytes(stored[who_at - 2..who_at].try_into().expect("2")) as usize;
    let end = who_at + who_len + 4;
    if who_len < aes::TAG_LEN || stored.len() < end {
        return Err(bad());
    }
    let kids = stored[3..at].as_chunks::<16>().0.to_vec();
    let nonce = stored[at..at + aes::NONCE_LEN].try_into().expect("12");
    let who = &stored[who_at..who_at + who_len];
    let len = u32::from_le_bytes(stored[end - 4..end].try_into().expect("4")) as usize;
    if len < aes::TAG_LEN || stored.len() - end > len {
        return Err(bad());
    }
    Ok(Header {
        kids,
        nonce,
        who,
        len,
        raw: &stored[..end],
        inline: &stored[end..],
    })
}

fn aad(key: &[u8], header: &[u8]) -> Vec<u8> {
    let mut a = Vec::with_capacity(key.len() + header.len() + 4);
    a.extend_from_slice(&(key.len() as u32).to_le_bytes());
    a.extend_from_slice(key);
    a.extend_from_slice(header);
    a
}

fn chunks_prefix(t: &Table, pk: &[u8]) -> Vec<u8> {
    let mut k = t.index_prefix(crate::large::LARGE_INDEX);
    k.extend_from_slice(pk);
    k.extend_from_slice(&CHUNKS_COLUMN.to_be_bytes());
    k
}

/// The subjects' keys a row of `t` is sealed under, made if new (and kept
/// before this returns), with their ids — refused when the row names no
/// subject, or one forgotten after the writer's snapshot (`40001`: it
/// runs again and finds the rows gone).
fn keys_for(txn: &dyn KvTxn, t: &Table, row: &Row) -> Result<(Vec<KeyId>, Rc<aes::Key>)> {
    let store = store()?;
    let cols = subject_columns(txn, t)?;
    let pepper = match kept(|k| k.pepper) {
        Some(p) => p,
        None => {
            let p = store
                .pepper(true)
                .map_err(Error::io)?
                .ok_or_else(|| Error::internal("no pepper made"))?;
            kept(|k| k.pepper = Some(p));
            p
        }
    };
    let mut ids: Vec<Id> = Vec::with_capacity(cols.len());
    for v in cols.iter().filter_map(|&c| row.get(c)) {
        if *v == Value::Null {
            continue;
        }
        let text = subject_text(v);
        let id = match kept(|k| k.ids.get(&text).copied()) {
            Some(id) => id,
            None => {
                let id = subject_keys::id(&pepper, &text);
                kept(|k| {
                    if k.ids.len() >= KEPT_SETS {
                        k.ids.clear();
                    }
                    k.ids.insert(text, id);
                });
                id
            }
        };
        ids.push(id);
    }
    if ids.is_empty() {
        return Err(Error::not_null_violation(format!(
            "a row of \"{}\", stored sealed, needs a subject: its subject columns are all NULL",
            t.name
        )));
    }
    ids.sort();
    ids.dedup();
    let snapshot: u64 = txn
        .read_ts()
        .to_string()
        .parse()
        .map_err(|_| Error::internal("a snapshot's commit"))?;
    // Which subjects' users hold their keys (known not to, kept).
    let mut held: Vec<Option<(KeyId, Vec<u8>)>> = vec![None; ids.len()];
    let unknown: Vec<usize> = (0..ids.len())
        .filter(|&i| !kept(|k| k.plain.contains(&ids[i])))
        .collect();
    if !unknown.is_empty() {
        let asked: Vec<Id> = unknown.iter().map(|&i| ids[i]).collect();
        let got = store.held_of(&asked).map_err(Error::io)?;
        for (&i, h) in unknown.iter().zip(got) {
            match h {
                Some(h) => held[i] = Some(h),
                None => {
                    kept(|k| k.plain.insert(ids[i]));
                }
            }
        }
    }
    let plain_ids: Vec<Id> = (0..ids.len())
        .filter(|&i| held[i].is_none())
        .map(|i| ids[i])
        .collect();
    let mut plain = store
        .keys(&plain_ids, snapshot)
        .map_err(Error::io)?
        .into_iter();
    let any_held = held.iter().any(Option::is_some);
    let mut kids = Vec::with_capacity(ids.len());
    let mut keys = Vec::with_capacity(ids.len());
    for (id, h) in ids.iter().zip(held) {
        let (kid, key) = match h {
            Some((kid, wrapped)) => (kid, held_key(t, id, &kid, &wrapped)?),
            None => match plain.next().flatten() {
                Some(k) => k,
                None => {
                    return Err(Error::serialization(format!(
                        "a subject of this row of \"{}\" was forgotten after the transaction \
                         began",
                        t.name
                    )));
                }
            },
        };
        kids.push(kid);
        keys.push(key);
    }
    if any_held {
        // Never kept: a thread serves other sessions after this one.
        return Ok((kids, Rc::new(row_key(&data_key(&keys)))));
    }
    if kids.len() > u8::MAX as usize {
        return Err(Error::program_limit(format!(
            "a row of \"{}\" names {} subjects; a sealed row takes 255",
            t.name,
            kids.len()
        )));
    }
    let kid_bytes = kids.concat();
    let aes = match kept(|k| k.by_kids.get(&kid_bytes).cloned().flatten()) {
        Some(a) => a,
        None => {
            let a = Rc::new(row_key(&data_key(&keys)));
            kept(|k| k.keep(kid_bytes, Some(Rc::clone(&a))));
            a
        }
    };
    Ok((kids, aes))
}

/// The key of subject `id` (key id `kid`), its user's to hold, opened by
/// what the session presented — refused (`28000`) when it presented
/// nothing for the subject, or what does not open it.
fn held_key(t: &Table, id: &Id, kid: &KeyId, wrapped: &[u8]) -> Result<Key> {
    let Some(secret) = presented(id) else {
        return Err(Error::invalid_authorization(format!(
            "a row of \"{}\" is sealed under a key its subject's user holds, and this session \
             has not presented it",
            t.name
        )));
    };
    subject_keys::unwrap(&subject_keys::user_key(&secret, id), kid, wrapped).ok_or_else(|| {
        Error::invalid_authorization(format!(
            "the key presented for a subject of a row of \"{}\" does not open its key",
            t.name
        ))
    })
}

/// The secrets a session has presented for subjects whose users hold
/// their keys (PLAN.md §M14.30 K4b), by subject id: kept in the session's
/// memory only, never written, never shown.
#[derive(Default, Clone)]
pub struct Presented(HashMap<Id, [u8; 32]>);

impl Presented {
    /// Presented: `secret` for subject `id`.
    pub fn insert(&mut self, id: Id, secret: [u8; 32]) {
        self.0.insert(id, secret);
    }

    /// Whether nothing is presented.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Presented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Presented({} subjects)", self.0.len())
    }
}

impl Drop for Presented {
    fn drop(&mut self) {
        for s in self.0.values_mut() {
            s.fill(0);
            std::hint::black_box(&s);
        }
    }
}

thread_local! {
    static PRESENTED: RefCell<Option<Arc<Presented>>> = const { RefCell::new(None) };
    static HELD: std::cell::Cell<Held> = const { std::cell::Cell::new(Held::Refused) };
}

/// Runs `f` with what a session presented in place (its statements).
pub fn with_presented<T>(p: &Arc<Presented>, f: impl FnOnce() -> T) -> T {
    let before = PRESENTED.with(|c| c.replace(Some(Arc::clone(p))));
    let out = f();
    PRESENTED.with(|c| *c.borrow_mut() = before);
    out
}

/// How a row whose key its user holds reads when the user has not
/// presented it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Held {
    /// Refused (`28000`): a statement of a session.
    Refused,
    /// Absent: a backup, which copies the row sealed as it is.
    Absent,
    /// As far as it can be read without the key — its primary key and
    /// subject columns, the others `NULL`: the work no user is present
    /// for, which finds a subject's rows and deletes them (`FORGET`), or
    /// passes them over (expiry, whose time is sealed; the embedding
    /// queue, whose text is).
    Who,
}

/// Runs `f` with rows whose keys their users hold, not presented, read as
/// `held` says.
pub fn with_held<T>(held: Held, f: impl FnOnce() -> T) -> T {
    let before = HELD.with(|c| c.replace(held));
    let out = f();
    HELD.with(|c| c.set(before));
    out
}

fn presented(id: &Id) -> Option<[u8; 32]> {
    PRESENTED.with(|c| c.borrow().as_ref().and_then(|p| p.0.get(id).copied()))
}

/// The keys kept in memory by this thread: the pepper, and the AES key of
/// each set of key ids (`None`: one destroyed) — dropped whenever the
/// keys change ([`subject_keys::changed`]: a key destroyed), so a row
/// never opens after its key is gone.
#[derive(Default)]
struct Kept {
    epoch: u64,
    pepper: Option<Key>,
    /// Subjects' ids by their text.
    ids: HashMap<String, Id>,
    /// The key for who a row is ([`who_key`]).
    who: Option<Rc<aes::Key>>,
    /// Subjects whose users do not hold their keys.
    plain: std::collections::HashSet<Id>,
    by_kids: HashMap<Vec<u8>, Option<Rc<aes::Key>>>,
}

impl Kept {
    fn keep(&mut self, kids: Vec<u8>, key: Option<Rc<aes::Key>>) {
        if self.by_kids.len() >= KEPT_SETS {
            self.by_kids.clear();
        }
        self.by_kids.insert(kids, key);
    }
}

/// The sets of keys a thread keeps.
const KEPT_SETS: usize = 4096;

thread_local! {
    static KEPT: RefCell<Kept> = RefCell::new(Kept::default());
}

fn kept<T>(f: impl FnOnce(&mut Kept) -> T) -> T {
    let now = subject_keys::epoch();
    KEPT.with(|k| {
        let mut k = k.borrow_mut();
        if k.epoch != now {
            *k = Kept {
                epoch: now,
                ..Kept::default()
            };
        }
        f(&mut k)
    })
}

/// Seals `row` for row key `key`: the value stored under it and the
/// chunks past [`crate::large::CHUNK`].
fn seal(txn: &dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let (kids, aes_key) = keys_for(txn, t, row)?;
    let mut body = crate::encoding::encode_row(row);
    let nonce = nonce();
    let len = body.len() + aes::TAG_LEN;
    // Who the row is: its subject columns' values, sealed apart.
    let cols = subject_columns(txn, t)?;
    let subjects: Row = cols.iter().map(|&c| row[c].clone()).collect();
    let mut who = crate::encoding::encode_row(&subjects);
    let who_tag = aes::seal_in_place(&*who_key()?, &nonce, key, &mut who);
    who.extend_from_slice(&who_tag);
    let who_len = u16::try_from(who.len()).map_err(|_| too_long(t))?;
    let mut out = Vec::with_capacity(3 + 16 * kids.len() + aes::NONCE_LEN + 6 + who.len() + len);
    out.extend_from_slice(&[TAG, VERSION, kids.len() as u8]);
    for k in &kids {
        out.extend_from_slice(k);
    }
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&who_len.to_le_bytes());
    out.extend_from_slice(&who);
    out.extend_from_slice(&(u32::try_from(len).map_err(|_| too_long(t))?).to_le_bytes());
    let tag = aes::seal_in_place(&aes_key, &nonce, &aad(key, &out), &mut body);
    body.extend_from_slice(&tag);
    let keep = body.len().min(crate::large::CHUNK);
    out.extend_from_slice(&body[..keep]);
    let chunks = body[keep..]
        .chunks(crate::large::CHUNK)
        .map(<[u8]>::to_vec)
        .collect();
    Ok((out, chunks))
}

fn too_long(t: &Table) -> Error {
    Error::program_limit(format!("a row of \"{}\" is too long to seal", t.name))
}

/// The row encoding sealed in `stored` (row key `key`): `None` when a key
/// it was sealed under is destroyed.
/// What opening a sealed row gave.
enum Opened {
    /// Its row encoding.
    Body(Vec<u8>),
    /// A key it was sealed under is destroyed.
    Gone,
    /// A key its user holds, not presented ([`Held`] not `Refused`).
    Held,
}

fn open(txn: &dyn KvTxn, t: &Table, key: &[u8], stored: &[u8]) -> Result<Opened> {
    let h = header(stored)?;
    let kid_bytes = &h.raw[3..3 + 16 * h.kids.len()];
    let aes_key = match kept(|k| k.by_kids.get(kid_bytes).cloned()) {
        Some(a) => a,
        None => {
            let store = store()?;
            let got = store.open(&h.kids).map_err(Error::io)?;
            if got.iter().all(Option::is_some) {
                let keys: Vec<Key> = got.into_iter().flatten().collect();
                let a = Some(Rc::new(row_key(&data_key(&keys))));
                kept(|k| k.keep(kid_bytes.to_vec(), a.clone()));
                a
            } else {
                let held = store.held(&h.kids).map_err(Error::io)?;
                let mut keys = Vec::with_capacity(h.kids.len());
                let mut gone = false;
                let mut any_held = false;
                let mut not_presented = false;
                let mode = HELD.with(|c| c.get());
                for ((kid, k), hk) in h.kids.iter().zip(got).zip(held) {
                    match (k, hk) {
                        (Some(k), _) => keys.push(k),
                        (None, Some((id, w))) => {
                            any_held = true;
                            if presented(&id).is_none() && mode != Held::Refused {
                                not_presented = true;
                            } else {
                                keys.push(held_key(t, &id, kid, &w)?);
                            }
                        }
                        (None, None) => gone = true,
                    }
                }
                if gone {
                    if !any_held {
                        kept(|k| k.keep(kid_bytes.to_vec(), None));
                    }
                    return Ok(Opened::Gone);
                }
                if not_presented {
                    return Ok(Opened::Held);
                }
                let a = Some(Rc::new(row_key(&data_key(&keys))));
                if !any_held {
                    kept(|k| k.keep(kid_bytes.to_vec(), a.clone()));
                }
                a
            }
        }
    };
    let Some(aes_key) = aes_key else {
        return Ok(Opened::Gone);
    };
    let mut body = Vec::with_capacity(h.len);
    body.extend_from_slice(h.inline);
    if body.len() < h.len {
        let p = chunks_prefix(t, pk_suffix(t, key));
        let end = prefix_end(&p).expect("chunk prefix");
        for e in txn.scan(Bound::Included(&p), Bound::Excluded(&end))? {
            body.extend_from_slice(&e?.1);
        }
    }
    if body.len() != h.len {
        return Err(Error::internal(format!(
            "{}: a sealed row has {} of its {} bytes",
            t.name,
            body.len(),
            h.len
        )));
    }
    let tag: [u8; aes::TAG_LEN] = body[h.len - aes::TAG_LEN..].try_into().expect("16");
    body.truncate(h.len - aes::TAG_LEN);
    if !aes::open_in_place(&aes_key, &h.nonce, &aad(key, h.raw), &mut body, &tag) {
        return Err(Error::internal(format!(
            "{}: a sealed row does not open under its subjects' keys (altered, or moved)",
            t.name
        )));
    }
    Ok(Opened::Body(body))
}

/// The row stored as `stored` under `key`, the columns `wanted` marks
/// decoded: `None` when its key is destroyed.
fn read(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    stored: &[u8],
    labels: &mut Labels,
    wanted: Option<&[bool]>,
) -> Result<Option<Row>> {
    if !is_sealed(stored) {
        // Plain: written before the table was sealed.
        return crate::large::read_some(txn, t, key, stored, labels, wanted).map(Some);
    }
    // Only subject columns wanted (an index on them filled, a subject's
    // rows found): from the header, whoever holds the row's key.
    if let Some(w) = wanted
        && w.iter().any(|&x| x)
    {
        let cols = subject_columns(txn, t)?;
        if w.iter()
            .enumerate()
            .all(|(c, &need)| !need || cols.contains(&c))
        {
            return who_of(txn, t, key, stored, &cols);
        }
    }
    match open(txn, t, key, stored)? {
        Opened::Gone => Ok(None),
        Opened::Body(plain) => {
            crate::large::read_some(txn, t, key, &plain, labels, wanted).map(Some)
        }
        Opened::Held => match HELD.with(|c| c.get()) {
            Held::Who => {
                let cols = subject_columns(txn, t)?;
                who_of(txn, t, key, stored, &cols)
            }
            _ => Ok(None),
        },
    }
}

/// The store's key for who a row is: what the subject columns' values in
/// a sealed row's header are sealed with, derived from the pepper — kept
/// with the subjects' keys, outside the data and its backups, so a backup
/// alone does not say whose rows it holds, while the database can always
/// tell (as its subject index does) without the subjects' keys.
fn who_key() -> Result<Rc<aes::Key>> {
    if let Some(k) = kept(|k| k.who.clone()) {
        return Ok(k);
    }
    let pepper = store()?
        .pepper(true)
        .map_err(Error::io)?
        .ok_or_else(|| Error::internal("no pepper made"))?;
    let k = Rc::new(aes::Key::new(&nucleus_crypto::hmac_sha256(
        &pepper,
        b"nucleus sealed row subjects 1",
    )));
    kept(|kept| kept.who = Some(Rc::clone(&k)));
    Ok(k)
}

/// A sealed row's subject columns alone, the others `NULL`, read from its
/// header: `None` when a key it was sealed under is destroyed (a key its
/// user holds is not needed).
fn who_of(
    txn: &dyn KvTxn,
    t: &Table,
    key: &[u8],
    stored: &[u8],
    cols: &[usize],
) -> Result<Option<Row>> {
    let h = header(stored)?;
    let store = store()?;
    let open = store.open(&h.kids).map_err(Error::io)?;
    if open.iter().any(Option::is_none) {
        let held = store.held(&h.kids).map_err(Error::io)?;
        if open
            .iter()
            .zip(&held)
            .any(|(o, h)| o.is_none() && h.is_none())
        {
            return Ok(None);
        }
    }
    let _ = txn;
    let mut who = h.who[..h.who.len() - aes::TAG_LEN].to_vec();
    let tag: [u8; aes::TAG_LEN] = h.who[h.who.len() - aes::TAG_LEN..].try_into().expect("16");
    if !aes::open_in_place(&*who_key()?, &h.nonce, key, &mut who, &tag) {
        return Err(Error::internal(format!(
            "{}: a sealed row's subjects do not open (altered, or moved)",
            t.name
        )));
    }
    let values = crate::encoding::decode_row(&who, cols.len())?;
    let mut row = vec![Value::Null; t.columns.len()];
    for (&c, v) in cols.iter().zip(values) {
        row[c] = v;
    }
    // The primary key: from the row's key.
    if !t.primary_key.is_empty() {
        let (pk, _) = crate::encoding::decode_key(pk_suffix(t, key), t.primary_key.len())?;
        for (&c, v) in t.primary_key.iter().zip(pk) {
            row[c] = v;
        }
    }
    Ok(Some(row))
}

/// Removes what is stored under `key` (a sealed row and its chunks, or a
/// plain row and its values stored out of line).
fn remove_stored(txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
    let Some(b) = txn.get(key)? else {
        return Ok(());
    };
    if !is_sealed(&b) {
        return crate::large::remove(txn, t, key);
    }
    let h = header(&b)?;
    if h.inline.len() < h.len {
        let p = chunks_prefix(t, pk_suffix(t, key));
        let end = prefix_end(&p).expect("chunk prefix");
        catalog::delete_range(txn, &p, Some(&end))?;
    }
    txn.delete(key)?;
    Ok(())
}

fn put_chunks(txn: &mut dyn KvTxn, t: &Table, key: &[u8], chunks: &[Vec<u8>]) -> Result<()> {
    let p = chunks_prefix(t, pk_suffix(t, key));
    for (n, c) in chunks.iter().enumerate() {
        let mut k = p.clone();
        k.extend_from_slice(&(n as u32).to_be_bytes());
        txn.put(&k, c)?;
    }
    Ok(())
}

/// Whether `t`'s definition suits its storage: a sealed table's indexes
/// cover only its subject columns — they are in the clear in an index, as
/// they are meant to be found by; another column's values (or an
/// expression's) would be copied out of the seal.  Checked whenever a
/// table's definition is stored, so every way of making an index meets
/// it.
pub fn check_table(txn: &dyn KvTxn, t: &Table) -> Result<()> {
    if t.storage != SEALED {
        return Ok(());
    }
    if t.indexes.is_empty() {
        return Ok(());
    }
    let subjects = subject_columns(txn, t)?;
    for ix in &t.indexes {
        let bad = ix
            .columns
            .iter()
            .zip(&ix.exprs)
            .find(|(c, e)| e.is_some() || !subjects.contains(c));
        if let Some((&c, e)) = bad {
            let what = match e {
                Some(_) => "an expression".to_string(),
                None => format!("column \"{}\"", t.columns[c].name),
            };
            return Err(Error::feature(format!(
                "index \"{}\" on {what} of \"{}\": the table is stored sealed, and the index \
                 would hold its values in the clear; a sealed table's indexes cover only its \
                 subject columns",
                ix.name, t.name
            )));
        }
    }
    Ok(())
}

/// Whether `t` is becoming `row` (`SET STORAGE row`): its writes stored
/// plain.
fn unsealing(t: &Table) -> bool {
    t.copy == crate::column_store::CopyState::Unsealing
}

/// One part of sealing `t`'s rows (`seal`) or opening them: up to `max`
/// rows after row key `after` stored the other way written again — a row
/// whose key is destroyed, met while opening, removed (it was forgotten;
/// its index entries go after, [`drop_dangling`]).  The last row key
/// looked at (`None` at the table's end), and how many rows were removed.
pub fn rewrite(
    txn: &mut dyn KvTxn,
    t: &Table,
    after: Option<&[u8]>,
    max: usize,
    seal_them: bool,
) -> Result<(Option<Vec<u8>>, usize)> {
    let mut removed = 0;
    let start = t.index_prefix(0);
    let end = prefix_end(&start).expect("an index prefix");
    let from = match after {
        Some(k) => Bound::Excluded(k),
        None => Bound::Included(start.as_slice()),
    };
    let entries: Vec<(Vec<u8>, Vec<u8>)> = txn
        .scan(from, Bound::Excluded(&end))?
        .take(max)
        .collect::<nucleus_kv::Result<_>>()?;
    let mut labels = Labels::default();
    for (k, v) in &entries {
        if is_sealed(v) == seal_them {
            continue;
        }
        let row = read(&*txn, t, k, v, &mut labels, None)?;
        remove_stored(txn, t, k)?;
        let Some(row) = row else {
            removed += 1;
            continue;
        };
        if seal_them {
            let (value, chunks) = seal(&*txn, t, k, &row)?;
            txn.put(k, &value)?;
            put_chunks(txn, t, k, &chunks)?;
        } else {
            crate::large::write(txn, t, k, &row, None)?;
        }
    }
    let last = if entries.len() < max {
        None
    } else {
        entries.last().map(|(k, _)| k.clone())
    };
    Ok((last, removed))
}

/// One part of removing the index entries of `t`'s rows that are gone
/// (forgotten rows removed by [`rewrite`]): up to `max` entries after
/// `after` (an index entry's key) looked at.  The last one looked at, or
/// `None` when every index is done.
pub fn drop_dangling(
    txn: &mut dyn KvTxn,
    t: &Table,
    after: Option<&[u8]>,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let base = t.index_prefix(0);
    let mut seen = 0;
    // In key order: an index's prefix ends in its id.
    let mut ids: Vec<u32> = t.indexes.iter().map(|ix| ix.id).collect();
    ids.sort_unstable();
    for id in ids {
        let p = t.index_prefix(id);
        let end = prefix_end(&p).expect("an index prefix");
        let from = match after {
            Some(k) if k.starts_with(&p) => Bound::Excluded(k),
            Some(k) if k > p.as_slice() => continue,
            _ => Bound::Included(p.as_slice()),
        };
        let entries: Vec<(Vec<u8>, Vec<u8>)> = txn
            .scan(from, Bound::Excluded(&end))?
            .take(max - seen)
            .collect::<nucleus_kv::Result<_>>()?;
        seen += entries.len();
        for (k, pk) in &entries {
            let mut row_key = base.clone();
            row_key.extend_from_slice(pk);
            if txn.get(&row_key)?.is_none() {
                txn.delete(k)?;
            }
        }
        if seen == max {
            return Ok(entries.last().map(|(k, _)| k.clone()));
        }
    }
    Ok(None)
}

/// The storage.
pub struct SealedStorage;

impl TableStorage for SealedStorage {
    fn name(&self) -> &str {
        SEALED
    }

    fn index_ids(&self, _: &Table) -> Vec<u32> {
        vec![0, crate::large::LARGE_INDEX]
    }

    fn write(
        &self,
        txn: &mut dyn KvTxn,
        t: &Table,
        key: &[u8],
        row: &Row,
        old: Option<&Row>,
    ) -> Result<()> {
        if old.is_some() {
            remove_stored(txn, t, key)?;
        }
        if unsealing(t) {
            return crate::large::write(txn, t, key, row, None);
        }
        let (value, chunks) = seal(&*txn, t, key, row)?;
        txn.put(key, &value)?;
        put_chunks(txn, t, key, &chunks)
    }

    fn insert(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8], row: &Row) -> Result<bool> {
        if unsealing(t) {
            return crate::large::insert(txn, t, key, row);
        }
        let (value, chunks) = seal(&*txn, t, key, row)?;
        if !txn.put_new(key, &value)? {
            return Ok(false);
        }
        put_chunks(txn, t, key, &chunks)?;
        Ok(true)
    }

    fn remove(&self, txn: &mut dyn KvTxn, t: &Table, key: &[u8]) -> Result<()> {
        remove_stored(txn, t, key)
    }

    fn exists(&self, txn: &dyn KvTxn, _: &Table, key: &[u8], validated: bool) -> Result<bool> {
        Ok(if validated {
            txn.get_validated(key)?
        } else {
            txn.get(key)?
        }
        .is_some())
    }

    fn get(
        &self,
        txn: &dyn KvTxn,
        t: &Table,
        key: &[u8],
        wanted: Option<&[bool]>,
        labels: &mut Labels,
    ) -> Result<Option<Row>> {
        match txn.get(key)? {
            None => Ok(None),
            Some(v) => read(txn, t, key, &v, labels, wanted),
        }
    }

    fn rows<'a>(&self, scan: Scan<'a>) -> Result<RowIter<'a>> {
        let end = scan
            .end
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut entries = crate::exec::Entries::over(
            scan.txn,
            scan.validated,
            Bound::Included(&scan.start),
            end,
        )?;
        let mut labels = Labels::default();
        let (txn, t, wanted) = (scan.txn, scan.table, scan.wanted);
        Ok(Box::new(std::iter::from_fn(move || {
            let row = (|| loop {
                let Some((k, v)) = entries.next()? else {
                    return Ok(None);
                };
                if let Some(row) = read(txn, t, k, v, &mut labels, wanted)? {
                    return Ok(Some((k.to_vec(), row)));
                }
            })();
            row.transpose()
        })))
    }

    /// The rows counted, none offered: the statistics (and the sample
    /// `ANALYZE` keeps) would hold the sealed values in the clear.
    fn sample(&self, scan: Scan<'_>, sample: &mut crate::stats::Sample) -> Result<()> {
        let end = scan
            .end
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut entries = crate::exec::Entries::over(
            scan.txn,
            scan.validated,
            Bound::Included(&scan.start),
            end,
        )?;
        while entries.next()?.is_some() {
            sample.count()?;
        }
        Ok(())
    }
}
