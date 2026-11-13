//! Finding bytes in a store's files (PLAN.md §M14.24 R2c): the proof an
//! erasure asks for — that none of the values it removed is still in any
//! file of the directory.
//!
//! Every file is searched as it is, and the data file once more as the
//! B+tree wrote it: each page that holds a leaf — reachable or not, a
//! free page keeps its last contents until it is reused or zeroed — has
//! its keys put together (a leaf stores their common prefix once, so a
//! key's bytes need not be contiguous in the page), its compressed values
//! decompressed and its values in overflow chains read whole.  Values
//! encoded past recognition (bit-packed numbers, a layer's own
//! compression) are not found: what a layer encodes, it must remove by
//! construction; the search checks the bytes a value keeps as it is.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use nucleus_platform::{File, FileSystem, OpenMode};

use crate::error::Result;
use crate::node::{LEAF, ValRef, View, decode_overflow};
use crate::page::{PAGE_SIZE, Page, PageId};

/// Where a needle was found.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Found {
    /// The file.
    pub file: PathBuf,
    /// The byte offset — in the data file, of the page it was found in.
    pub offset: u64,
    /// Which needle.
    pub needle: usize,
}

/// The shortest needle searched for: shorter ones would be found by
/// chance.
pub const MIN_NEEDLE: usize = 4;

/// Needles by their first four bytes, with a filter on the first two.
struct Needles<'a> {
    all: &'a [Vec<u8>],
    pairs: Vec<u64>,
    by_head: HashMap<[u8; 4], Vec<usize>>,
    longest: usize,
}

impl<'a> Needles<'a> {
    fn new(all: &'a [Vec<u8>]) -> Self {
        let mut pairs = vec![0u64; 1 << 10];
        let mut by_head: HashMap<[u8; 4], Vec<usize>> = HashMap::new();
        let mut longest = 0;
        for (i, n) in all.iter().enumerate() {
            if n.len() < MIN_NEEDLE {
                continue;
            }
            let pair = u16::from_le_bytes([n[0], n[1]]) as usize;
            pairs[pair / 64] |= 1 << (pair % 64);
            by_head.entry([n[0], n[1], n[2], n[3]]).or_default().push(i);
            longest = longest.max(n.len());
        }
        Self {
            all,
            pairs,
            by_head,
            longest,
        }
    }

    /// Calls `hit(at, needle)` for each needle in `hay` starting before
    /// `limit`.
    fn search(&self, hay: &[u8], limit: usize, mut hit: impl FnMut(usize, usize)) {
        if self.by_head.is_empty() || hay.len() < MIN_NEEDLE {
            return;
        }
        let end = limit.min(hay.len() - MIN_NEEDLE + 1);
        for at in 0..end {
            let pair = u16::from_le_bytes([hay[at], hay[at + 1]]) as usize;
            if self.pairs[pair / 64] & (1 << (pair % 64)) == 0 {
                continue;
            }
            let head = [hay[at], hay[at + 1], hay[at + 2], hay[at + 3]];
            if let Some(ids) = self.by_head.get(&head) {
                for &i in ids {
                    if hay[at..].starts_with(&self.all[i]) {
                        hit(at, i);
                    }
                }
            }
        }
    }
}

/// Every place in the files under `dir` where one of `needles` (those of
/// [`MIN_NEEDLE`] bytes or more) is — once per file offset, or per data
/// file page.  The lock file is skipped.
pub fn find(fs: &dyn FileSystem, dir: &Path, needles: &[Vec<u8>]) -> Result<Vec<Found>> {
    let n = Needles::new(needles);
    let mut out = BTreeSet::new();
    let mut files = Vec::new();
    walk(fs, dir, &mut files)?;
    for path in files {
        if path.file_name().is_some_and(|f| f == "LOCK") {
            continue;
        }
        let file = fs.open(&path, OpenMode::ReadOnly)?;
        let data = path.parent() == Some(dir) && path.file_name().is_some_and(|f| f == "data");
        search_file(file.as_ref(), &n, |at, needle| {
            let offset = if data { at - at % PAGE_SIZE as u64 } else { at };
            out.insert(Found {
                file: path.clone(),
                offset,
                needle,
            });
        })?;
        if data {
            search_pages(file.as_ref(), &n, |page, needle| {
                out.insert(Found {
                    file: path.clone(),
                    offset: page * PAGE_SIZE as u64,
                    needle,
                });
            })?;
        }
    }
    Ok(out.into_iter().collect())
}

fn walk(fs: &dyn FileSystem, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut names = fs.list_dir(dir)?;
    names.sort();
    for name in names {
        let path = dir.join(name);
        match fs.list_dir(&path) {
            Ok(_) => walk(fs, &path, out)?,
            Err(_) => out.push(path),
        }
    }
    Ok(())
}

/// The file's bytes, a mebibyte at a time (overlapping by the longest
/// needle).
fn search_file(file: &dyn File, n: &Needles, mut hit: impl FnMut(u64, usize)) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    let size = file.size()?;
    let overlap = n.longest.saturating_sub(1);
    let mut buf = vec![0u8; CHUNK + overlap];
    let mut at = 0u64;
    while at < size {
        let len = ((size - at) as usize).min(buf.len());
        file.read_exact_at(&mut buf[..len], at)?;
        // Needles starting in this chunk proper; the overlap is the next's.
        let limit = if at + (len as u64) < size { CHUNK } else { len };
        n.search(&buf[..len], limit, |i, needle| hit(at + i as u64, needle));
        at += CHUNK as u64;
    }
    Ok(())
}

fn read_page(file: &dyn File, id: u64, size: u64) -> Result<Option<Page>> {
    if (id + 1) * PAGE_SIZE as u64 > size {
        return Ok(None);
    }
    let mut buf = vec![0u8; PAGE_SIZE];
    file.read_exact_at(&mut buf, id * PAGE_SIZE as u64)?;
    Ok(Some(Page::from_bytes(&buf)))
}

/// An overflow chain's bytes, as far as it reads (a free page's chain may
/// be broken).
fn chain(file: &dyn File, first: PageId, len: u64, size: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut next = first;
    let mut seen = BTreeSet::new();
    while next.0 != 0 && (out.len() as u64) < len && seen.insert(next.0) {
        let Some(page) = read_page(file, next.0, size)? else {
            break;
        };
        let Ok((after, data)) = decode_overflow(&page) else {
            break;
        };
        out.extend_from_slice(data);
        next = after;
    }
    out.truncate(len as usize);
    Ok(out)
}

/// The data file's leaves, decoded: keys whole, values decompressed and
/// read from their overflow chains.  `hit(page, needle)`.
fn search_pages(file: &dyn File, n: &Needles, mut hit: impl FnMut(u64, usize)) -> Result<()> {
    let size = file.size()?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    for id in 0..size / PAGE_SIZE as u64 {
        let Some(page) = read_page(file, id, size)? else {
            break;
        };
        // (An overflow page alone is found by the raw search; its chain's
        // whole value through its leaf.)
        if page.page_type() != LEAF {
            continue;
        }
        let Ok(view) = View::new(&page) else {
            continue;
        };
        for i in 0..view.len() {
            if let Ok(k) = view.key(i) {
                key.clear();
                k.append_to(&mut key);
                n.search(&key, usize::MAX, |_, needle| hit(id, needle));
            }
            let Ok(v) = view.value(i) else { continue };
            value.clear();
            match v {
                ValRef::Inline(_) => continue,
                ValRef::Packed { raw, block } => {
                    if crate::lz::decompress(block, raw as usize, &mut value).is_none() {
                        continue;
                    }
                }
                ValRef::Overflow { first, len, raw } => {
                    let stored = chain(file, first, len, size)?;
                    match raw {
                        None => value = stored,
                        Some(raw) => {
                            if crate::lz::decompress(&stored, raw as usize, &mut value).is_none() {
                                continue;
                            }
                        }
                    }
                }
            }
            n.search(&value, usize::MAX, |_, needle| hit(id, needle));
        }
    }
    Ok(())
}
