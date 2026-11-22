//! Engines behind the store (PLAN.md §M14.28 L1): a key range — a table,
//! an index, a partition — routed to an engine other than the B+tree.
//!
//! A branch's [`Tables`] are its B+tree and one engine for each route;
//! the routes are the store's, apart and sorted, and everything outside
//! them — the reserved keys, the catalog, every table not routed — is the
//! B+tree's.  Writes stay one WAL record a commit whatever engines it
//! touches (the record is the store's, before any engine sees it), so a
//! commit across engines is atomic without two-phase commit; recovery
//! replays the record into each engine its keys route to.  Reads see one
//! ordered map: the engines hold apart key ranges, so a scan across them
//! is their pieces one after another — no merge.
//!
//! An engine keeps its state in memory between checkpoints, as the
//! B+tree's dirty pages are; a checkpoint saves it in a file named by the
//! branch's checkpoint LSN (written and synced before the checkpoint's
//! commit, the older ones removed after it), so recovery opens the file
//! of the checkpoint it starts from and replays the WAL after it.
//!
//! [`EngineKind::Map`] is the trivial engine the tests prove the boundary
//! with: a sorted map in memory, saved whole at each checkpoint.  The LSM
//! tree (§M14.28 L2) is the engine this boundary is for.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nucleus_platform::{FileSystem, OpenMode};

use crate::btree::{self, BTree, KeyRange, LeafFilter};
use crate::crc32c::crc32c;
use crate::{Error, PageId, Result};

/// Puts (`Some`) and deletes (`None`), sorted by key, each key once.
pub type Ops = [(Vec<u8>, Option<Vec<u8>>)];

/// An engine behind the store: an ordered map of a key range.
pub trait Engine: Send + Sync {
    /// The value of `key`.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    /// The entries in `(start, end)`, ascending or (`reverse`) descending.
    fn scan<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        reverse: bool,
    ) -> Result<Box<dyn Entries + 'a>>;
    /// Applies `ops` (sorted, each key once).
    fn apply(&mut self, ops: &Ops) -> Result<()>;
    /// [`apply`](Self::apply), then what `filter` finds garbage among the
    /// written keys' other entries the engine holds in memory dropped — a
    /// commit's [`LeafFilter`], as the B+tree's leaves it rewrites see it
    /// (§M14.28 L5).  Older entries of the keys may lie elsewhere, so it is
    /// shown them as [`LeafFilter::drops_above`].  Not logged, as the
    /// B+tree's drops are not.
    fn apply_filtered(&mut self, ops: &Ops, filter: &dyn LeafFilter) -> Result<()> {
        let _ = filter;
        self.apply(ops)
    }
    /// [`apply`](Self::apply) or [`apply_filtered`](Self::apply_filtered)
    /// with `ops` handed over: an engine that keeps them may take their
    /// keys and values without copying them (a commit's).
    fn apply_owned(
        &mut self,
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        filter: Option<&dyn LeafFilter>,
    ) -> Result<()> {
        match filter {
            Some(f) => self.apply_filtered(&ops, f),
            None => self.apply(&ops),
        }
    }
    /// Removes every entry in `[start, end)`.
    fn remove_range(&mut self, start: &[u8], end: &[u8]) -> Result<()>;
    /// [`remove_range`](Self::remove_range) of each of `ranges` (ascending
    /// by start), which an engine may do at once.
    fn remove_ranges(&mut self, ranges: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        for (start, end) in ranges {
            self.remove_range(start, end)?;
        }
        Ok(())
    }
    /// Up to `parts - 1` keys inside `(start, end)` cutting it into parts
    /// of about equal size, each of at least `min_blocks` of the engine's
    /// blocks; ascending ([`BTree::split_keys`]'s, for a parallel scan's
    /// morsels).  None by default: one part.
    fn split_keys(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_blocks: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let _ = (start, end, parts, min_blocks);
        Ok(Vec::new())
    }
    /// Whether it holds no entry.
    fn is_empty(&self) -> bool;
    /// A copy, for a branch made from this one's.
    fn fork(&self) -> Box<dyn Engine>;
    /// Sets apart what the coming [`saver`](Self::saver) will save, under
    /// the branch's write lock (cheap: readers wait for it).
    fn prepare(&mut self) -> Result<()> {
        Ok(())
    }
    /// Its state as of the checkpoint, to save: taken under the branch's
    /// write lock (cheap — what [`prepare`](Self::prepare) set apart and
    /// shared pointers), run without it, writes going on meanwhile
    /// (§M14.28 L4a).
    fn saver(&self) -> Saver;
    /// The files in `dir` it needs: those its state names, and those its
    /// last saved (or loaded) file names.  The store removes the others no
    /// branch's engine names.
    fn files(&self) -> Vec<String> {
        Vec::new()
    }
    /// Work due in the background — an LSM tree's compaction — planned
    /// under the branch's read lock; `None` when nothing is due.
    fn job(&self) -> Option<Job> {
        None
    }
    /// Work the writer does before it goes on — an LSM tree's memtable,
    /// grown by a bulk load, written as a run (§M14.28 L4b) — planned and
    /// run with the branch's writer lock held (nothing changes the engine
    /// meanwhile but background installs), the readers going on; `None`
    /// when nothing is due.
    fn spill(&self) -> Option<Job> {
        None
    }
    /// A job's result taken in, under the branch's write lock.
    fn install(&mut self, _done: Done) -> Result<()> {
        Ok(())
    }
}

/// Saves an engine's state as of a checkpoint, durably: `path` the file
/// the checkpoint's LSN names, `dir` the directory of files it may share
/// with other branches' engines (an LSM tree's runs), `name` unique to
/// the branch, route and checkpoint, for the files it writes there.
pub type Saver = Box<dyn FnOnce(&dyn FileSystem, &Path, &Path, &str) -> Result<()> + Send>;

/// A job's result, for [`Engine::install`]; `None` when it stopped early.
pub type Done = Box<dyn std::any::Any + Send>;

/// Work an engine does off the locks ([`Engine::job`]).
pub type Job = Box<dyn FnOnce(&dyn JobRoom) -> Result<Option<Done>> + Send>;

/// What a [`Job`] works with.
pub trait JobRoom {
    /// The file system.
    fn fs(&self) -> &dyn FileSystem;
    /// The directory of the engines' shared files.
    fn dir(&self) -> &Path;
    /// A name for a new file in [`dir`](Self::dir), not in use, the file
    /// kept from removal until the job's result is installed.
    fn name(&self) -> Result<String>;
    /// `bytes` more written: waits as the I/O budget asks
    /// ([`crate::StoreOptions::compaction_bytes_per_sec`]); false when the
    /// job is to stop.
    fn wrote(&self, bytes: u64) -> bool;
    /// What a merge may drop as garbage the layer above knows of (the
    /// transaction layer's versions no snapshot sees, §M14.28 L4d): made
    /// now, for this job.
    fn filter(&self) -> Option<Box<dyn crate::btree::LeafFilter + Send>> {
        None
    }
}

/// Makes a compaction's [`crate::btree::LeafFilter`] as of now
/// ([`crate::Store::set_compaction_filter`]); `None`: drop nothing.
pub type FilterMaker =
    Arc<dyn Fn() -> Option<Box<dyn crate::btree::LeafFilter + Send>> + Send + Sync>;

/// A scan's entries, the key first and the value only when asked.
pub trait Entries {
    /// The next entry's key into `key`; false at the end.
    fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool>;
    /// The value of the entry [`next_key_into`](Self::next_key_into) gave
    /// last.
    fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()>;
    /// Entries a batch at a time, as [`btree::Range::leaf_entries`] gives
    /// a leaf's: each one's key into `key` (the one before in `prev`, the
    /// two swapped), its value when at most `max` bytes long, to `each`,
    /// which may decline one (it stays the next); false once they are
    /// done.  `None` where the engine gives an entry at a time only — for
    /// the rest of the scan then.
    fn leaf_entries(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<Option<bool>> {
        let _ = (key, prev, max, each);
        Ok(None)
    }
}

/// The engines there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    /// A sorted map in memory, saved whole at each checkpoint: the
    /// boundary's proof in the tests, not for tables of any size.
    Map,
    /// The LSM tree (§M14.28 L2, [`crate::lsm`]).
    Lsm,
}

impl EngineKind {
    fn code(self) -> u8 {
        match self {
            EngineKind::Map => 1,
            EngineKind::Lsm => 2,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(EngineKind::Map),
            2 => Some(EngineKind::Lsm),
            _ => None,
        }
    }

    /// An empty engine of this kind.
    pub fn empty(self) -> Box<dyn Engine> {
        match self {
            EngineKind::Map => Box::new(MapEngine::default()),
            EngineKind::Lsm => Box::new(crate::lsm::LsmEngine::new()),
        }
    }

    /// The engine saved in `path` (its shared files in `dir`).
    pub fn load(self, fs: &dyn FileSystem, dir: &Path, path: &Path) -> Result<Box<dyn Engine>> {
        match self {
            EngineKind::Map => Ok(Box::new(MapEngine::load(fs, path)?)),
            EngineKind::Lsm => Ok(Box::new(crate::lsm::LsmEngine::load(fs, dir, path)?)),
        }
    }
}

/// A key range routed to an engine: `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Its number, the store's own, never used again.
    pub id: u32,
    /// The engine that holds the range.
    pub kind: EngineKind,
    /// The first key of the range.
    pub start: Vec<u8>,
    /// The key past the range.
    pub end: Vec<u8>,
}

fn corrupt(reason: &'static str) -> Error {
    Error::Corrupt {
        page: PageId(0),
        reason,
    }
}

// ------------------------------------------------------------ files

/// Writes `bytes` (then their CRC-32C) to `path` durably: a new file
/// beside it, synced, renamed over it, the directory synced.
pub(crate) fn write_file(fs: &dyn FileSystem, path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    if fs.exists(&tmp)? {
        fs.remove_file(&tmp)?;
    }
    let f = fs.open(&tmp, OpenMode::CreateNew)?;
    let mut all = bytes.to_vec();
    all.extend_from_slice(&crc32c(bytes).to_le_bytes());
    let mut at = 0;
    while at < all.len() {
        let n = f.write_at(&all[at..], at as u64)?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::other("short write")));
        }
        at += n;
    }
    f.sync()?;
    drop(f);
    fs.rename(&tmp, path)?;
    fs.sync_dir(path.parent().unwrap_or(Path::new("/")))?;
    Ok(())
}

/// Reads a file [`write_file`] wrote: its bytes, the checksum checked.
pub(crate) fn read_file(fs: &dyn FileSystem, path: &Path) -> Result<Vec<u8>> {
    let f = fs.open(path, OpenMode::ReadOnly)?;
    let len = f.size()? as usize;
    let mut all = vec![0; len];
    let mut at = 0;
    while at < len {
        let n = f.read_at(&mut all[at..], at as u64)?;
        if n == 0 {
            return Err(corrupt("an engine's file ends early"));
        }
        at += n;
    }
    if len < 4 {
        return Err(corrupt("an engine's file too short"));
    }
    let (body, sum) = all.split_at(len - 4);
    if crc32c(body).to_le_bytes() != sum {
        return Err(corrupt("an engine's file fails its checksum"));
    }
    all.truncate(len - 4);
    Ok(all)
}

/// Reads length-prefixed fields.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(corrupt("an engine's file ends early"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

const ROUTES_MAGIC: &[u8; 8] = b"ncroute1";
const MAP_MAGIC: &[u8; 8] = b"ncmap\0\0\x01";

/// The directory of a store's engines' files.
pub(crate) fn engines_dir(dir: &Path) -> PathBuf {
    dir.join("engines")
}

/// The file of route `route`'s engine on branch `branch` as of its
/// checkpoint at `lsn`.
pub(crate) fn engine_file(dir: &Path, branch: u32, route: u32, lsn: u64) -> PathBuf {
    engines_dir(dir).join(format!("{branch}-{route}-{lsn}"))
}

/// The routes of the store in `dir` (none if it has no routes file).
pub(crate) fn read_routes(fs: &dyn FileSystem, dir: &Path) -> Result<Vec<Route>> {
    let path = engines_dir(dir).join("routes");
    if !fs.exists(&path)? {
        return Ok(Vec::new());
    }
    let bytes = read_file(fs, &path)?;
    let mut r = Reader(&bytes);
    if r.take(8)? != ROUTES_MAGIC {
        return Err(corrupt("not a routes file"));
    }
    let n = r.u32()?;
    let mut routes = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let id = r.u32()?;
        let kind = EngineKind::from_code(r.take(1)?[0]).ok_or(corrupt("an unknown engine"))?;
        let start = r.bytes()?.to_vec();
        let end = r.bytes()?.to_vec();
        routes.push(Route {
            id,
            kind,
            start,
            end,
        });
    }
    Ok(routes)
}

/// Writes the store's routes, durably.
pub(crate) fn write_routes(fs: &dyn FileSystem, dir: &Path, routes: &[Route]) -> Result<()> {
    let edir = engines_dir(dir);
    if !fs.exists(&edir)? {
        fs.create_dir_all(&edir)?;
        fs.sync_dir(dir)?;
    }
    let mut out = ROUTES_MAGIC.to_vec();
    out.extend_from_slice(&(routes.len() as u32).to_le_bytes());
    for r in routes {
        out.extend_from_slice(&r.id.to_le_bytes());
        out.push(r.kind.code());
        put_bytes(&mut out, &r.start);
        put_bytes(&mut out, &r.end);
    }
    write_file(fs, &edir.join("routes"), &out)
}

// ------------------------------------------------------------ the map

/// [`EngineKind::Map`]: a sorted map, saved whole.
#[derive(Default, Clone)]
pub struct MapEngine {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MapEngine {
    fn load(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        let bytes = read_file(fs, path)?;
        let mut r = Reader(&bytes);
        if r.take(8)? != MAP_MAGIC {
            return Err(corrupt("not a map engine's file"));
        }
        let n = r.u32()?;
        let mut map = BTreeMap::new();
        for _ in 0..n {
            let k = r.bytes()?.to_vec();
            let v = r.bytes()?.to_vec();
            map.insert(k, v);
        }
        Ok(Self { map })
    }
}

/// Whether `(start, end)` holds no key at all — `BTreeMap::range` refuses
/// such bounds.
fn empty_bounds(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Included(s), Bound::Included(e)) => s > e,
        (Bound::Included(s), Bound::Excluded(e))
        | (Bound::Excluded(s), Bound::Included(e))
        | (Bound::Excluded(s), Bound::Excluded(e)) => s >= e,
        _ => false,
    }
}

struct MapEntries<'a> {
    it: Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a>,
    cur: Option<&'a Vec<u8>>,
}

impl Entries for MapEntries<'_> {
    fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool> {
        match self.it.next() {
            Some((k, v)) => {
                key.clear();
                key.extend_from_slice(k);
                self.cur = Some(v);
                Ok(true)
            }
            None => {
                self.cur = None;
                Ok(false)
            }
        }
    }

    fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()> {
        val.clear();
        val.extend_from_slice(self.cur.map_or(&[][..], |v| v.as_slice()));
        Ok(())
    }
}

impl Engine for MapEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan<'a>(
        &'a self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        reverse: bool,
    ) -> Result<Box<dyn Entries + 'a>> {
        let it: Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a> =
            if empty_bounds(start, end) {
                Box::new(std::iter::empty())
            } else {
                let r = self.map.range::<[u8], _>((start, end));
                if reverse {
                    Box::new(r.rev())
                } else {
                    Box::new(r)
                }
            };
        Ok(Box::new(MapEntries { it, cur: None }))
    }

    fn apply(&mut self, ops: &Ops) -> Result<()> {
        for (k, v) in ops {
            match v {
                Some(v) => {
                    self.map.insert(k.clone(), v.clone());
                }
                None => {
                    self.map.remove(k);
                }
            }
        }
        Ok(())
    }

    fn remove_range(&mut self, start: &[u8], end: &[u8]) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let keys: Vec<Vec<u8>> = self
            .map
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            self.map.remove(&k);
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn fork(&self) -> Box<dyn Engine> {
        Box::new(self.clone())
    }

    fn saver(&self) -> Saver {
        // The whole map encoded now: this engine is the boundary's proof,
        // not for tables of any size.
        let mut out = MAP_MAGIC.to_vec();
        out.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
        for (k, v) in &self.map {
            put_bytes(&mut out, k);
            put_bytes(&mut out, v);
        }
        Box::new(move |fs, _dir, path, _name| write_file(fs, path, &out))
    }
}

// ------------------------------------------------------------ tables

/// A branch's tables: its B+tree and the engines of the store's routes
/// (in the routes' order).  What [`crate::Store::read`] hands out.
pub struct Tables {
    pub(crate) tree: BTree,
    pub(crate) routes: Vec<Route>,
    pub(crate) engines: Vec<Box<dyn Engine>>,
}

/// A piece of a scan: a key range and who holds it (`None`: the B+tree).
#[derive(Clone, Debug)]
struct Piece {
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    engine: Option<usize>,
}

/// Whether a range starting at `start` may hold a key below `k`.
fn starts_before(start: &Bound<Vec<u8>>, k: &[u8]) -> bool {
    match start {
        Bound::Unbounded => true,
        Bound::Included(s) | Bound::Excluded(s) => s.as_slice() < k,
    }
}

/// Whether a range ending at `end` may hold `k` or a key above it.
fn ends_after(end: &Bound<Vec<u8>>, k: &[u8]) -> bool {
    match end {
        Bound::Unbounded => true,
        Bound::Included(e) => e.as_slice() >= k,
        Bound::Excluded(e) => e.as_slice() > k,
    }
}

/// The lower of two upper bounds.
fn min_end(a: Bound<Vec<u8>>, b: Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    let key = |x: &Bound<Vec<u8>>| match x {
        Bound::Included(k) | Bound::Excluded(k) => Some(k.clone()),
        Bound::Unbounded => None,
    };
    match (key(&a), key(&b)) {
        (None, _) => b,
        (_, None) => a,
        (Some(x), Some(y)) if x < y => a,
        (Some(x), Some(y)) if y < x => b,
        _ => {
            if matches!(a, Bound::Excluded(_)) {
                a
            } else {
                b
            }
        }
    }
}

fn borrow(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn own(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    b.map(<[u8]>::to_vec)
}

impl Tables {
    pub(crate) fn new(tree: BTree, routes: Vec<Route>, engines: Vec<Box<dyn Engine>>) -> Self {
        debug_assert_eq!(routes.len(), engines.len());
        Self {
            tree,
            routes,
            engines,
        }
    }

    /// The B+tree.
    pub fn tree(&self) -> &BTree {
        &self.tree
    }

    /// The B+tree's invariants ([`BTree::check`]).
    pub fn check(&self) -> Result<btree::TreeCheck> {
        self.tree.check()
    }

    /// The routes of the store, sorted.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// The engine `key` routes to (`None`: the B+tree).
    fn engine_of(&self, key: &[u8]) -> Option<usize> {
        let i = self.routes.partition_point(|r| r.start.as_slice() <= key);
        (i > 0 && key < self.routes[i - 1].end.as_slice()).then(|| i - 1)
    }

    /// `(start, end)` in pieces, each one engine's (or the B+tree's), in
    /// ascending order.
    fn pieces(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Vec<Piece> {
        let end = own(end);
        let mut cur = own(start);
        let mut out = Vec::new();
        for (i, r) in self.routes.iter().enumerate() {
            if !ends_after(&end, &r.start) {
                break;
            }
            if !starts_before(&cur, &r.end) {
                continue;
            }
            if starts_before(&cur, &r.start) {
                out.push(Piece {
                    start: cur,
                    end: min_end(end.clone(), Bound::Excluded(r.start.clone())),
                    engine: None,
                });
                cur = Bound::Included(r.start.clone());
            }
            out.push(Piece {
                start: cur,
                end: min_end(end.clone(), Bound::Excluded(r.end.clone())),
                engine: Some(i),
            });
            cur = Bound::Included(r.end.clone());
            if !ends_after(&end, &r.end) {
                return out;
            }
        }
        out.push(Piece {
            start: cur,
            end,
            engine: None,
        });
        out
    }

    /// The value of `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.engine_of(key) {
            None => self.tree.get(key),
            Some(i) => self.engines[i].get(key),
        }
    }

    /// [`BTree::seek_group`] where no route is (true); false where there
    /// are, `found` untouched: a caller seeks each key.
    pub fn seek_group(&self, keys: &[&[u8]], found: &mut Vec<btree::Found>) -> Result<bool> {
        if !self.routes.is_empty() {
            return Ok(false);
        }
        self.tree.seek_group(keys, found)?;
        Ok(true)
    }

    /// The entries with keys in `(start, end)`, ascending.
    pub fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        if self.routes.is_empty() {
            return Ok(Scan::Tree(self.tree.range(start, end)?));
        }
        Ok(Scan::Pieces(Box::new(Pieces::new(
            self,
            self.pieces(start, end),
            false,
        ))))
    }

    /// The entries with keys in `(start, end)`, descending.
    pub fn range_rev(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Scan<'_>> {
        if self.routes.is_empty() {
            return Ok(Scan::Tree(self.tree.range_rev(start, end)?));
        }
        let mut pieces = self.pieces(start, end);
        pieces.reverse();
        Ok(Scan::Pieces(Box::new(Pieces::new(self, pieces, true))))
    }

    /// A cursor, positioned before the first entry.
    pub fn cursor(&self) -> Result<TablesCursor<'_>> {
        if self.routes.is_empty() {
            return Ok(TablesCursor::Tree(self.tree.cursor()?));
        }
        Ok(TablesCursor::Routed {
            tables: self,
            scan: Box::new(self.range(Bound::Unbounded, Bound::Unbounded)?),
        })
    }

    /// [`BTree::split_keys`] where `(start, end)` is the B+tree's alone;
    /// none (one part) where it reaches into another engine.
    pub fn split_keys(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_leaves: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let end_b = end.map_or(Bound::Unbounded, Bound::Excluded);
        let pieces = self.pieces(Bound::Included(start), end_b);
        match pieces.as_slice() {
            // Within one engine's range (a table of its own): its keys.
            [
                Piece {
                    start: s,
                    end: e,
                    engine: Some(i),
                },
            ] => {
                let e = match e {
                    Bound::Included(k) | Bound::Excluded(k) => Some(k.as_slice()),
                    Bound::Unbounded => None,
                };
                self.engines[*i].split_keys(bound_key(s, start), e, parts, min_leaves)
            }
            // Across engines: one part.
            p if p.iter().any(|p| p.engine.is_some()) => Ok(Vec::new()),
            _ => self.tree.split_keys(start, end, parts, min_leaves),
        }
    }

    /// Where these tables and `base`'s may differ: [`BTree::diff_ranges`]
    /// for the B+tree, and each route's whole range where either side's
    /// engine holds anything (an engine keeps no birth to compare by).
    pub fn diff_ranges(&self, base: &Tables, origin: u64) -> Result<Vec<KeyRange>> {
        let mut ranges = self.tree.diff_ranges(&base.tree, origin)?;
        for (i, r) in self.routes.iter().enumerate() {
            let theirs = base.engines.get(i).is_some_and(|e| !e.is_empty());
            if !self.engines[i].is_empty() || theirs {
                ranges.push((Some(r.start.clone()), Some(r.end.clone())));
            }
        }
        Ok(btree::merge_ranges(ranges))
    }

    /// Applies `ops`, sorted with each key once, `filter` seeing the
    /// B+tree's leaves.
    pub(crate) fn apply_sorted(
        &mut self,
        ops: &Ops,
        filter: Option<&dyn LeafFilter>,
    ) -> Result<()> {
        let to_tree = |tree: &mut BTree, ops: &Ops| match filter {
            Some(f) => tree.apply_sorted_filtered(ops, f),
            None => tree.apply_sorted(ops),
        };
        if self.routes.is_empty() {
            return to_tree(&mut self.tree, ops);
        }
        let mut tree_ops = Vec::new();
        let mut engine_ops: Vec<Vec<(Vec<u8>, Option<Vec<u8>>)>> =
            vec![Vec::new(); self.routes.len()];
        for op in ops {
            match self.engine_of(&op.0) {
                None => tree_ops.push(op.clone()),
                Some(i) => engine_ops[i].push(op.clone()),
            }
        }
        if !tree_ops.is_empty() || (filter.is_some() && !ops.is_empty()) {
            to_tree(&mut self.tree, &tree_ops)?;
        }
        for (e, ops) in self.engines.iter_mut().zip(engine_ops) {
            if !ops.is_empty() {
                e.apply_owned(ops, filter)?;
            }
        }
        Ok(())
    }

    /// Each range's keys that end with one of `tails`' old ends rewritten
    /// to end with its new one, values kept: the B+tree's in place where
    /// they keep their order ([`BTree::rekey`]), the rest — those it gave
    /// back, an engine's — moved (each deleted, its value put under the
    /// new key).  Applied the same way when a log is replayed: the same
    /// keys rewritten, wherever they lie (issue #4).
    pub(crate) fn rekey(
        &mut self,
        ranges: &[(Vec<u8>, Option<Vec<u8>>)],
        tails: &btree::Tails,
    ) -> Result<()> {
        if tails.is_empty() {
            return Ok(());
        }
        let mut moves: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (start, end) in ranges {
            let end_b = end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
            for piece in self.pieces(Bound::Included(start), end_b) {
                let key = |b: &Bound<Vec<u8>>| match b {
                    Bound::Included(k) | Bound::Excluded(k) => Some(k.clone()),
                    Bound::Unbounded => None,
                };
                match piece.engine {
                    None => {
                        let from = key(&piece.start).unwrap_or_default();
                        let to = key(&piece.end);
                        moves.extend(self.tree.rekey(&from, to.as_deref(), tails)?);
                    }
                    Some(i) => {
                        let mut scan = self.engines[i].scan(
                            borrow(&piece.start),
                            borrow(&piece.end),
                            false,
                        )?;
                        let mut k = Vec::new();
                        while scan.next_key_into(&mut k)? {
                            if let Some(t) = tails.new_end(&k) {
                                let mut new = k.clone();
                                let at = new.len() - t.len();
                                new[at..].copy_from_slice(t);
                                moves.push((k.clone(), new));
                            }
                        }
                    }
                }
            }
        }
        if moves.is_empty() {
            return Ok(());
        }
        // Each moved: its delete and its put, in key order (a new key may
        // sort before or after other old ones).
        let mut ops = Vec::with_capacity(2 * moves.len());
        for (old, new) in moves {
            let Some(v) = self.get(&old)? else {
                continue;
            };
            ops.push((old, None));
            ops.push((new, Some(v)));
        }
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        self.apply_sorted_owned(ops, None)
    }

    /// [`apply_sorted`](Self::apply_sorted) of ops handed over: the
    /// B+tree's moved into its cells, not copied.
    pub(crate) fn apply_sorted_owned(
        &mut self,
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        filter: Option<&dyn LeafFilter>,
    ) -> Result<()> {
        if self.routes.is_empty() {
            return self.tree.apply_sorted_owned(ops, filter);
        }
        let mut tree_ops = Vec::new();
        let mut engine_ops: Vec<Vec<(Vec<u8>, Option<Vec<u8>>)>> =
            vec![Vec::new(); self.routes.len()];
        for op in ops {
            match self.engine_of(&op.0) {
                None => tree_ops.push(op),
                Some(i) => engine_ops[i].push(op),
            }
        }
        self.tree.apply_sorted_owned(tree_ops, filter)?;
        for (e, ops) in self.engines.iter_mut().zip(engine_ops) {
            if !ops.is_empty() {
                e.apply_owned(ops, filter)?;
            }
        }
        Ok(())
    }

    /// Sets `key` (one op, in its engine).
    pub(crate) fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        match self.engine_of(key) {
            None => self.tree.insert(key, value),
            Some(i) => self.engines[i].apply(&[(key.to_vec(), Some(value.to_vec()))]),
        }
    }

    /// Removes `key` (one op, in its engine).
    pub(crate) fn remove(&mut self, key: &[u8]) -> Result<()> {
        match self.engine_of(key) {
            None => self.tree.remove(key).map(|_| ()),
            Some(i) => self.engines[i].apply(&[(key.to_vec(), None)]),
        }
    }

    /// [`remove_range`](Self::remove_range) of each of `ranges`: the
    /// B+tree's entries in all of them removed in one sorted pass (a
    /// replayed sweep's covered ranges, one or a few entries each — a
    /// descent per entry from the root made a crash's recovery replay
    /// at ~13 MB/s), the engines' each by its own.
    pub(crate) fn remove_ranges(&mut self, ranges: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let mut keys = Vec::new();
        let mut engine_ranges: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![Vec::new(); self.engines.len()];
        for (start, end) in ranges {
            if start >= end {
                continue;
            }
            for p in self.pieces(Bound::Included(start), Bound::Excluded(end)) {
                match p.engine {
                    Some(i) => {
                        let (s, e) = (bound_key(&p.start, start), bound_key(&p.end, end));
                        engine_ranges[i].push((s.to_vec(), e.to_vec()));
                    }
                    None => {
                        for e in self.tree.range(borrow(&p.start), borrow(&p.end))? {
                            keys.push((e?.0, None));
                        }
                    }
                }
            }
        }
        if !keys.windows(2).all(|w| w[0].0 < w[1].0) {
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            keys.dedup_by(|a, b| a.0 == b.0);
        }
        for (e, r) in self.engines.iter_mut().zip(engine_ranges) {
            if !r.is_empty() {
                e.remove_ranges(&r)?;
            }
        }
        self.tree.apply_sorted_owned(keys, None)
    }

    /// Each engine's [`Engine::prepare`], under the branch's write lock.
    pub(crate) fn prepare_engines(&mut self) -> Result<()> {
        for e in &mut self.engines {
            e.prepare()?;
        }
        Ok(())
    }

    /// The shared files its engines name.
    pub(crate) fn engine_files(&self) -> Vec<String> {
        self.engines.iter().flat_map(|e| e.files()).collect()
    }

    /// Whether `[start, end)` holds no entry of the B+tree.
    pub(crate) fn tree_empty_in(&self, start: &[u8], end: &[u8]) -> Result<bool> {
        Ok(self
            .tree
            .range(Bound::Included(start), Bound::Excluded(end))?
            .next()
            .is_none())
    }
}

/// A piece's bound's key (`or` where unbounded: never, inside a route).
fn bound_key<'a>(b: &'a Bound<Vec<u8>>, or: &'a [u8]) -> &'a [u8] {
    match b {
        Bound::Included(k) | Bound::Excluded(k) => k,
        Bound::Unbounded => or,
    }
}

/// One piece's entries.
enum Part<'a> {
    Tree(btree::Range<'a>),
    Other(Box<dyn Entries + 'a>),
}

impl Part<'_> {
    fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool> {
        match self {
            Part::Tree(r) => r.next_key_into(key),
            Part::Other(e) => e.next_key_into(key),
        }
    }

    fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()> {
        match self {
            Part::Tree(r) => r.value_into(val),
            Part::Other(e) => e.value_into(val),
        }
    }

    fn leaf_entries(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<Option<bool>> {
        match self {
            Part::Tree(r) => r.leaf_entries(key, prev, max, each),
            Part::Other(e) => e.leaf_entries(key, prev, max, each),
        }
    }
}

/// A scan over several pieces, each opened when reached.
pub struct Pieces<'a> {
    tables: &'a Tables,
    pieces: Vec<Piece>,
    next: usize,
    cur: Option<Part<'a>>,
    reverse: bool,
}

impl<'a> Pieces<'a> {
    fn new(tables: &'a Tables, pieces: Vec<Piece>, reverse: bool) -> Self {
        Self {
            tables,
            pieces,
            next: 0,
            cur: None,
            reverse,
        }
    }

    fn open(&self, p: &Piece) -> Result<Part<'a>> {
        let (s, e) = (borrow(&p.start), borrow(&p.end));
        Ok(match p.engine {
            None if self.reverse => Part::Tree(self.tables.tree.range_rev(s, e)?),
            None => Part::Tree(self.tables.tree.range(s, e)?),
            Some(i) => Part::Other(self.tables.engines[i].scan(s, e, self.reverse)?),
        })
    }

    fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool> {
        loop {
            if let Some(part) = &mut self.cur {
                if part.next_key_into(key)? {
                    return Ok(true);
                }
                self.cur = None;
            }
            let Some(p) = self.pieces.get(self.next) else {
                return Ok(false);
            };
            let part = self.open(p)?;
            self.next += 1;
            self.cur = Some(part);
        }
    }

    fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()> {
        match &mut self.cur {
            Some(part) => part.value_into(val),
            None => {
                val.clear();
                Ok(())
            }
        }
    }

    /// [`Entries::leaf_entries`] of the piece reached, the next opened
    /// when it is done.
    fn leaf_entries(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<Option<bool>> {
        if self.cur.is_none() {
            let Some(p) = self.pieces.get(self.next) else {
                return Ok(Some(false));
            };
            let part = self.open(p)?;
            self.next += 1;
            self.cur = Some(part);
        }
        let part = self.cur.as_mut().expect("a part");
        match part.leaf_entries(key, prev, max, each)? {
            Some(false) => {
                self.cur = None;
                Ok(Some(self.next < self.pieces.len()))
            }
            more => Ok(more),
        }
    }
}

/// [`Tables::range`]'s entries: the B+tree's range as it was where no
/// route is, the pieces one after another where there are.
pub enum Scan<'a> {
    /// No route: the B+tree's range.
    Tree(btree::Range<'a>),
    /// The pieces, one engine's each.
    Pieces(Box<Pieces<'a>>),
}

impl Scan<'_> {
    /// The next entry into `key` and `val`; false at the end.
    pub fn next_into(&mut self, key: &mut Vec<u8>, val: &mut Vec<u8>) -> Result<bool> {
        match self {
            Scan::Tree(r) => r.next_into(key, val),
            Scan::Pieces(p) => {
                if !p.next_key_into(key)? {
                    return Ok(false);
                }
                p.value_into(val)?;
                Ok(true)
            }
        }
    }

    /// The next entry's key into `key`, its value left for
    /// [`value_into`](Self::value_into); false at the end.
    pub fn next_key_into(&mut self, key: &mut Vec<u8>) -> Result<bool> {
        match self {
            Scan::Tree(r) => r.next_key_into(key),
            Scan::Pieces(p) => p.next_key_into(key),
        }
    }

    /// The value of the entry [`next_key_into`](Self::next_key_into) gave
    /// last.
    pub fn value_into(&mut self, val: &mut Vec<u8>) -> Result<()> {
        match self {
            Scan::Tree(r) => r.value_into(val),
            Scan::Pieces(p) => p.value_into(val),
        }
    }

    /// The value of the entry [`next_key_into`](Self::next_key_into) gave
    /// last into `val` when it is at most `max` bytes long (true); else
    /// false, read only as far as the engine must to tell
    /// ([`btree::Range::small_value_into`]; the pieces' engines read it).
    pub fn small_value_into(&mut self, val: &mut Vec<u8>, max: usize) -> Result<bool> {
        match self {
            Scan::Tree(r) => r.small_value_into(val, max),
            Scan::Pieces(p) => {
                p.value_into(val)?;
                Ok(val.len() <= max)
            }
        }
    }

    /// A leaf's entries at a time ([`btree::Range::leaf_entries`]): false
    /// once the scan is done; `None` where the engine gives an entry at a
    /// time only (the pieces, a descending range).
    pub fn leaf_entries(
        &mut self,
        key: &mut Vec<u8>,
        prev: &mut Vec<u8>,
        max: usize,
        each: &mut dyn FnMut(&[u8], &[u8], Option<&[u8]>) -> Result<bool>,
    ) -> Result<Option<bool>> {
        match self {
            Scan::Tree(r) => r.leaf_entries(key, prev, max, each),
            Scan::Pieces(p) => p.leaf_entries(key, prev, max, each),
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Scan::Tree(r) => r.next(),
            Scan::Pieces(_) => {
                let (mut k, mut v) = (Vec::new(), Vec::new());
                match self.next_into(&mut k, &mut v) {
                    Ok(true) => Some(Ok((k, v))),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                }
            }
        }
    }
}

/// [`Tables::cursor`]: the B+tree's cursor where no route is; where
/// there are, a scan from each seek.  After [`next_within`] refuses a key
/// the cursor must be sought again.
///
/// [`next_within`]: TablesCursor::next_within
pub enum TablesCursor<'a> {
    /// No route: the B+tree's cursor.
    Tree(btree::Cursor<'a>),
    /// Routes: a scan from the last seek.
    Routed {
        /// The tables sought in.
        tables: &'a Tables,
        /// The scan from the last seek.
        scan: Box<Scan<'a>>,
    },
}

impl TablesCursor<'_> {
    /// Moves before the first entry `>= key` (cheaply when it is near).
    pub fn seek_near(&mut self, key: &[u8]) -> Result<()> {
        match self {
            TablesCursor::Tree(c) => c.seek_near(key),
            TablesCursor::Routed { tables, scan } => {
                **scan = tables.range(Bound::Included(key), Bound::Unbounded)?;
                Ok(())
            }
        }
    }

    /// The next entry into `key` and `val` when `keep` holds for its key:
    /// false at the end and at a key `keep` refuses.
    pub fn next_within(
        &mut self,
        key: &mut Vec<u8>,
        val: &mut Vec<u8>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> Result<bool> {
        match self {
            TablesCursor::Tree(c) => c.next_within(key, val, keep),
            TablesCursor::Routed { scan, .. } => {
                if !scan.next_key_into(key)? || !keep(key) {
                    return Ok(false);
                }
                scan.value_into(val)?;
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::RangeBounds;

    use nucleus_platform::Platform;

    use super::*;

    fn tables(routes: Vec<Route>) -> Tables {
        let pager = std::sync::Arc::new(
            crate::Pager::create(
                nucleus_platform::sim::SimPlatform::new(1).fs(),
                Path::new("/data"),
            )
            .unwrap(),
        );
        let pool = std::sync::Arc::new(crate::BufferPool::new(pager, 64));
        let tree = BTree::create(pool).unwrap();
        let engines = routes.iter().map(|r| r.kind.empty()).collect();
        Tables::new(tree, routes, engines)
    }

    fn route(id: u32, s: &[u8], e: &[u8]) -> Route {
        Route {
            id,
            kind: EngineKind::Map,
            start: s.to_vec(),
            end: e.to_vec(),
        }
    }

    /// Keys land in the engine their route names, and every scan — either
    /// way, bounded anywhere — sees one ordered map.
    #[test]
    fn routed_keys_read_as_one_map() {
        let mut t = tables(vec![route(1, b"c", b"f"), route(2, b"m", b"p")]);
        let keys: Vec<Vec<u8>> = (b'a'..=b'z').map(|c| vec![c]).collect();
        let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = keys
            .iter()
            .map(|k| (k.clone(), Some(k.repeat(2))))
            .collect();
        t.apply_sorted(&ops, None).unwrap();
        assert_eq!(t.engines[0].get(b"d").unwrap(), Some(b"dd".to_vec()));
        assert_eq!(t.tree.get(b"d").unwrap(), None);
        assert_eq!(t.tree.get(b"g").unwrap(), Some(b"gg".to_vec()));
        for k in &keys {
            assert_eq!(t.get(k).unwrap(), Some(k.repeat(2)));
        }
        let bounds = |k: Option<u8>, inc: bool| match k {
            None => Bound::Unbounded,
            Some(c) if inc => Bound::Included(vec![c]),
            Some(c) => Bound::Excluded(vec![c]),
        };
        let edges: Vec<Option<u8>> = std::iter::once(None)
            .chain(b"abcefgmopqz".iter().copied().map(Some))
            .collect();
        for &s in &edges {
            for &e in &edges {
                for (si, ei) in [(true, true), (true, false), (false, true), (false, false)] {
                    let (sb, eb) = (bounds(s, si), bounds(e, ei));
                    let want: Vec<Vec<u8>> = keys
                        .iter()
                        .filter(|k| (borrow(&sb), borrow(&eb)).contains(&k.as_slice()))
                        .cloned()
                        .collect();
                    let got: Vec<Vec<u8>> = t
                        .range(borrow(&sb), borrow(&eb))
                        .unwrap()
                        .map(|e| e.unwrap().0)
                        .collect();
                    assert_eq!(got, want, "{sb:?} {eb:?}");
                    let mut rev: Vec<Vec<u8>> = t
                        .range_rev(borrow(&sb), borrow(&eb))
                        .unwrap()
                        .map(|e| e.unwrap().0)
                        .collect();
                    rev.reverse();
                    assert_eq!(rev, want, "rev {sb:?} {eb:?}");
                }
            }
        }
        // A cursor seeks across engines.
        let mut c = t.cursor().unwrap();
        let (mut k, mut v) = (Vec::new(), Vec::new());
        c.seek_near(b"e").unwrap();
        assert!(c.next_within(&mut k, &mut v, &|_| true).unwrap());
        assert_eq!((k.as_slice(), v.as_slice()), (&b"e"[..], &b"ee"[..]));
        assert!(c.next_within(&mut k, &mut v, &|_| true).unwrap());
        assert_eq!(k, b"f");
        drop(c);
        // Ranges removed across them, at once (unsorted, one empty).
        t.remove_ranges(&[
            (b"m".to_vec(), b"n".to_vec()),
            (b"b".to_vec(), b"g".to_vec()),
            (b"x".to_vec(), b"x".to_vec()),
            (b"f".to_vec(), b"m".to_vec()),
            (b"q".to_vec(), b"s".to_vec()),
        ])
        .unwrap();
        let left: Vec<u8> = t
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|e| e.unwrap().0[0])
            .collect();
        assert_eq!(
            left,
            [b"ano".as_slice(), b"p", &(b's'..=b'z').collect::<Vec<u8>>()].concat()
        );
    }

    /// A map engine saved and loaded holds the same; a damaged file is
    /// refused.
    #[test]
    fn a_map_engine_saved_and_loaded() {
        let p = nucleus_platform::sim::SimPlatform::new(3);
        let fs = p.fs();
        fs.create_dir_all(Path::new("/e")).unwrap();
        let mut m = MapEngine::default();
        m.apply(&[
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), Some(vec![])),
        ])
        .unwrap();
        let path = Path::new("/e/m");
        m.saver()(fs, Path::new("/e"), path, "x").unwrap();
        let back = MapEngine::load(fs, path).unwrap();
        assert_eq!(back.map, m.map);
        let f = fs.open(path, OpenMode::ReadWrite).unwrap();
        f.write_at(b"Z", 13).unwrap();
        assert!(MapEngine::load(fs, path).is_err());
        let routes = vec![route(7, b"x", b"y")];
        write_routes(fs, Path::new("/e"), &routes).unwrap();
        assert_eq!(read_routes(fs, Path::new("/e")).unwrap(), routes);
    }
}
