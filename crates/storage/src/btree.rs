//! A copy-on-write B+tree over the buffer pool.
//!
//! Keys and values are byte strings; keys compare bytewise (PLAN.md,
//! principle 2 — callers encode their keys so that bytewise order is the
//! order they want).  Keys are at most [`MAX_KEY`] bytes; values of any
//! size, large ones in overflow pages.
//!
//! **Copy-on-write.**  A node is changed in place only if its page was
//! allocated since the last commit; otherwise it is copied to a new page and
//! the old one freed, and the parent's pointer changes — up to a new root.
//! So the tree as of the last commit stays intact on disk while this one is
//! built beside it: committing the pager with the new root in a meta slot is
//! an atomic checkpoint.  The pager enforces the rule, so a mistake here
//! fails loudly instead of corrupting the committed tree.
//!
//! **Balance.**  An overfull node splits by bytes.  A non-root node that
//! falls under [`MIN_FILL`] bytes is merged with a sibling, or, if the two
//! do not fit in one page, the pair is split evenly.  An internal root with
//! one child is replaced by the child.  [`BTree::check`] verifies all of it.
//!
//! **Concurrency.**  `&self` methods read, `&mut self` methods write; wrap
//! the tree in a `RwLock` to share it.  There are no sibling links (they do
//! not survive copy-on-write); cursors keep their path instead.

use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::Arc;

use crate::buffer::{BufferPool, PageRef};
use crate::error::{Error, Result};
use crate::node::{
    Internal, LEAF, Leaf, MAX_INLINE_CELL, MAX_KEY, MIN_FILL, Node, OVERFLOW, OVERFLOW_CAPACITY,
    Val, ValRef, View, balance_point, cell_size, decode_overflow, encode_leaf, encode_overflow,
    key_prefix, leaf_cell_size, leaf_size,
};
use crate::page::{PAYLOAD_SIZE, Page, PageId};

/// Values this long or longer are compressed when that saves enough
/// (PLAN.md §M14.20 V5).  Not shorter ones: a row of a few hundred bytes
/// saves little, and every read of it would decompress it (a TPC-H scan
/// spent a third of its time so).
pub const COMPRESS_MIN: usize = 1024;

/// A B+tree rooted at a page.
#[derive(Debug)]
pub struct BTree {
    pool: Arc<BufferPool>,
    root: PageId,
    /// Threads a sorted batch's leaves may be prepared on
    /// ([`BTree::set_workers`]).
    workers: usize,
}

/// The result of changing a subtree: its (possibly new) root page and its
/// encoded size, or two pages and the separator between them.
enum Out {
    One(PageId, usize),
    Two(PageId, Vec<u8>, PageId),
}

/// What [`BTree::check`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeCheck {
    /// Levels, leaves included.
    pub depth: usize,
    /// Entries.
    pub entries: u64,
    /// Every page the tree uses: nodes and overflow pages.
    pub pages: BTreeSet<u64>,
}

impl BTree {
    /// Creates an empty tree.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let root = {
            let r = pool.create(crate::node::LEAF)?;
            Node::Leaf(Leaf::default()).encode(&mut *r.write()?);
            r.id()
        };
        Ok(Self {
            pool,
            root,
            workers: 1,
        })
    }

    /// Opens the tree rooted at `root`.
    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Self {
        Self {
            pool,
            root,
            workers: 1,
        }
    }

    /// Lets a large sorted batch ([`apply_sorted`](Self::apply_sorted))
    /// prepare its leaves on up to `n` threads (1, the default: on the
    /// caller's).  The tree it makes is the same, page for page.
    pub fn set_workers(&mut self, n: usize) {
        self.workers = n.max(1);
    }

    /// The root page.  Changes as the tree is modified.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// The buffer pool.
    pub fn pool(&self) -> &Arc<BufferPool> {
        &self.pool
    }

    fn load(&self, id: PageId) -> Result<Node> {
        let r = self.pool.fetch(id)?;
        let page = r.read();
        Node::decode(&page).map_err(|reason| Error::Corrupt { page: id, reason })
    }

    /// Writes `node` as the new version of page `id`: in place if allowed,
    /// else into a new page (freeing `id`).  Returns where it went.
    fn store(&self, id: PageId, node: &Node) -> Result<PageId> {
        if self.pool.is_writable(id) {
            let r = self.pool.fetch(id)?;
            node.encode(&mut *r.write()?);
            return Ok(id);
        }
        let new = self.store_new(node)?;
        self.pool.free(id)?;
        Ok(new)
    }

    fn store_new(&self, node: &Node) -> Result<PageId> {
        let r = self.pool.create(node.page_type())?;
        node.encode(&mut *r.write()?);
        Ok(r.id())
    }

    fn finish(&self, id: PageId, node: Node) -> Result<Out> {
        let size = node.size();
        if size <= PAYLOAD_SIZE {
            return Ok(Out::One(self.store(id, &node)?, size));
        }
        let (left, sep, right) = node.split();
        let l = self.store(id, &left)?;
        let r = self.store_new(&right)?;
        Ok(Out::Two(l, sep, r))
    }

    // ---------------------------------------------------------------- values

    fn make_val(&self, key: &[u8], value: &[u8]) -> Result<Val> {
        // Compressed when that saves an eighth or more.
        let mut block = Vec::new();
        if value.len() >= COMPRESS_MIN {
            crate::lz::compress(value, &mut block);
        }
        let packed = !block.is_empty() && block.len() + 4 <= value.len() - value.len() / 8;
        let val = if packed {
            Val::Packed {
                raw: value.len() as u32,
                block,
            }
        } else {
            Val::Inline(value.to_vec())
        };
        if leaf_cell_size(key, &val) - 2 <= MAX_INLINE_CELL {
            return Ok(val);
        }
        let (stored, raw) = match val {
            Val::Packed { block, .. } => (block, Some(value.len() as u64)),
            _ => (value.to_vec(), None),
        };
        let mut next = PageId(0);
        for chunk in stored.chunks(OVERFLOW_CAPACITY).rev() {
            let r = self.pool.create(OVERFLOW)?;
            encode_overflow(&mut *r.write()?, next, chunk);
            next = r.id();
        }
        Ok(Val::Overflow {
            first: next,
            len: stored.len() as u64,
            raw,
        })
    }

    /// Appends the value `v` holds to `out`: decompressed, its overflow
    /// chain read.  `page`: the leaf, for errors.
    fn read_ref(&self, page: PageId, v: ValRef, out: &mut Vec<u8>) -> Result<()> {
        let damaged = || Error::Corrupt {
            page,
            reason: "a compressed value does not decompress",
        };
        match v {
            ValRef::Inline(b) => out.extend_from_slice(b),
            ValRef::Packed { raw, block } => {
                crate::lz::decompress(block, raw as usize, out).ok_or_else(damaged)?;
            }
            ValRef::Overflow {
                first,
                len,
                raw: None,
            } => self.read_overflow(first, len, out)?,
            ValRef::Overflow {
                first,
                len,
                raw: Some(raw),
            } => {
                let mut block = Vec::new();
                self.read_overflow(first, len, &mut block)?;
                crate::lz::decompress(&block, raw as usize, out).ok_or_else(damaged)?;
            }
        }
        Ok(())
    }

    /// The pages of an overflow chain, checked against its length.
    fn overflow_pages(&self, first: PageId, len: u64) -> Result<Vec<(PageId, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut id = first;
        let mut total = 0u64;
        while id != PageId(0) {
            let r = self.pool.fetch(id)?;
            let page = r.read();
            let (next, data) =
                decode_overflow(&page).map_err(|reason| Error::Corrupt { page: id, reason })?;
            total += data.len() as u64;
            if total > len || out.len() as u64 > len / OVERFLOW_CAPACITY as u64 + 1 {
                return Err(Error::Corrupt {
                    page: id,
                    reason: "overflow chain longer than its value",
                });
            }
            out.push((id, data.to_vec()));
            id = next;
        }
        if total != len {
            return Err(Error::Corrupt {
                page: first,
                reason: "overflow chain shorter than its value",
            });
        }
        Ok(out)
    }

    /// Appends the value in the overflow chain at `first` to `out`.
    fn read_overflow(&self, first: PageId, len: u64, out: &mut Vec<u8>) -> Result<()> {
        out.reserve(len as usize);
        for (_, data) in self.overflow_pages(first, len)? {
            out.extend_from_slice(&data);
        }
        Ok(())
    }

    fn free_val(&self, val: &Val) -> Result<()> {
        if let Val::Overflow { first, len, .. } = val {
            for (id, _) in self.overflow_pages(*first, *len)? {
                self.pool.free(id)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ read

    /// The value of `key`, if present (each page on the way read in
    /// place).
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut id = self.root;
        loop {
            let r = self.pool.fetch(id)?;
            let page = r.read();
            let corrupt = |reason| Error::Corrupt { page: id, reason };
            let v = View::new(&page).map_err(corrupt)?;
            if !v.is_leaf() {
                id = v
                    .child(v.child_index(key).map_err(corrupt)?)
                    .map_err(corrupt)?;
                continue;
            }
            let i = v.partition(|k| k.order(key).is_lt()).map_err(corrupt)?;
            if i < v.len() && v.key(i).map_err(corrupt)?.is(key) {
                let val = v.value(i).map_err(corrupt)?;
                let mut out = Vec::new();
                if let ValRef::Overflow { first, len, raw } = val {
                    drop(page);
                    self.read_ref(id, ValRef::Overflow { first, len, raw }, &mut out)?;
                } else {
                    self.read_ref(id, val, &mut out)?;
                }
                return Ok(Some(out));
            }
            return Ok(None);
        }
    }

    /// Up to `parts - 1` keys inside `(start, end)` (`end` `None`: no
    /// upper bound) that cut the range into parts of about as many leaves
    /// each — none smaller than `min_leaves` leaves; ascending.  Read from
    /// the internal nodes over the range, down to the level above the
    /// leaves (or to where a level spans more than 4096 nodes), so it
    /// costs a few pages.  What parallel scans split their work by.
    pub fn split_keys(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        parts: usize,
        min_leaves: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let inside = |k: &[u8]| k > start && end.is_none_or(|e| k < e);
        let mut level = vec![self.root];
        let mut keys: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut next = Vec::new();
            let mut found = Vec::new();
            let mut leaves = false;
            for &id in &level {
                let r = self.pool.fetch(id)?;
                let page = r.read();
                let corrupt = |reason| Error::Corrupt { page: id, reason };
                let v = View::new(&page).map_err(corrupt)?;
                if v.is_leaf() {
                    leaves = true;
                    break;
                }
                // Child i holds the keys in [key(i - 1), key(i)) (an
                // internal node's keys are whole: no prefix).
                for i in 0..=v.len() {
                    let lo = if i == 0 {
                        None
                    } else {
                        Some(v.key(i - 1).map_err(corrupt)?.rest)
                    };
                    let hi = if i == v.len() {
                        None
                    } else {
                        Some(v.key(i).map_err(corrupt)?.rest)
                    };
                    let after_start = hi.is_none_or(|h| h > start);
                    let before_end = match (lo, end) {
                        (Some(l), Some(e)) => l < e,
                        _ => true,
                    };
                    if after_start && before_end {
                        next.push(v.child(i).map_err(corrupt)?);
                        if let Some(l) = lo
                            && inside(l)
                        {
                            found.push(l.to_vec());
                        }
                    }
                }
            }
            if leaves {
                break; // `keys` are the separators of the leaves
            }
            keys = found;
            if next.len() > 4096 {
                break;
            }
            level = next;
        }
        // Here each key starts a leaf (or a subtree of several).
        let parts = parts.min((keys.len() + 1) / min_leaves.max(1)).max(1);
        Ok((1..parts)
            .map(|j| keys[j * keys.len() / parts].clone())
            .collect())
    }

    /// A cursor, positioned before the first entry.
    pub fn cursor(&self) -> Result<Cursor<'_>> {
        let mut c = self.unplaced();
        c.seek(&[])?;
        Ok(c)
    }

    /// A cursor not yet in a leaf: every use seeks first.
    fn unplaced(&self) -> Cursor<'_> {
        Cursor {
            tree: self,
            path: Vec::new(),
            leaf: None,
            leaf_len: 0,
            pos: 0,
        }
    }

    /// The entries with keys in `(start, end)`, in ascending order.
    pub fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Range<'_>> {
        let mut cursor = self.unplaced();
        match start {
            Bound::Unbounded => cursor.seek(&[])?,
            Bound::Included(k) => cursor.seek(k)?,
            Bound::Excluded(k) => cursor.seek_after(k)?,
        }
        Ok(Range {
            cursor,
            end: end.map(<[u8]>::to_vec),
            reverse: false,
            done: false,
        })
    }

    /// The entries with keys in `(start, end)`, in descending order.
    pub fn range_rev(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Range<'_>> {
        let mut cursor = self.unplaced();
        match end {
            Bound::Unbounded => cursor.seek_end()?,
            Bound::Included(k) => cursor.seek_after(k)?,
            Bound::Excluded(k) => cursor.seek(k)?,
        }
        Ok(Range {
            cursor,
            end: start.map(<[u8]>::to_vec),
            reverse: true,
            done: false,
        })
    }

    // ----------------------------------------------------------------- write

    /// Sets `key` to `value`, replacing any old value.
    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY {
            return Err(Error::KeyTooLarge(key.len()));
        }
        let val = self.make_val(key, value)?;
        let out = self.insert_at(self.root, key, val)?;
        self.set_root(out)
    }

    fn set_root(&mut self, out: Out) -> Result<()> {
        match out {
            Out::One(id, _) => self.root = id,
            Out::Two(l, sep, r) => {
                let root = Node::Internal(Internal {
                    keys: vec![sep],
                    children: vec![l, r],
                });
                self.root = self.store_new(&root)?;
            }
        }
        // An internal root with one child is replaced by the child.
        loop {
            match self.load(self.root)? {
                Node::Internal(n) if n.keys.is_empty() => {
                    let old = self.root;
                    self.root = n.children[0];
                    self.pool.free(old)?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn insert_at(&mut self, id: PageId, key: &[u8], val: Val) -> Result<Out> {
        match self.load(id)? {
            Node::Leaf(mut leaf) => {
                match leaf.search(key) {
                    Ok(i) => {
                        let old = std::mem::replace(&mut leaf.entries[i].1, val);
                        self.free_val(&old)?;
                    }
                    Err(i) => leaf.entries.insert(i, (key.to_vec(), val)),
                }
                self.finish(id, Node::Leaf(leaf))
            }
            Node::Internal(mut node) => {
                let i = node.child_index(key);
                let out = self.insert_at(node.children[i], key, val)?;
                if self.absorb(&mut node, i, out)? {
                    self.finish(id, Node::Internal(node))
                } else {
                    Ok(Out::One(id, node.size()))
                }
            }
        }
    }

    /// Sets each key to its value, as [`insert`](Self::insert) one at a
    /// time would — but `entries` sorted by key and without repeats, each
    /// node is read and written once for all the keys that reach it, and
    /// full leaves are packed to about 7/8 (a run of new keys at the end of
    /// the tree fills pages instead of leaving them half empty).
    pub fn insert_sorted(&mut self, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let ops: Vec<(Vec<u8>, Option<Vec<u8>>)> = entries
            .iter()
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect();
        self.apply_sorted(&ops)
    }

    /// Sets (`Some`) and removes (`None`) keys — `ops` sorted by key and
    /// without repeats — as one at a time would, each node read and written
    /// once for all the keys that reach it.
    pub fn apply_sorted(&mut self, ops: &[(Vec<u8>, Option<Vec<u8>>)]) -> Result<()> {
        if let Some((k, _)) = ops.iter().find(|(k, _)| k.len() > MAX_KEY) {
            return Err(Error::KeyTooLarge(k.len()));
        }
        debug_assert!(ops.windows(2).all(|w| w[0].0 < w[1].0));
        if ops.is_empty() {
            return Ok(());
        }
        let vals = ops
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    v.as_ref().map(|v| self.make_val(k, v)).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut prep = Prep::new(self, &vals)?;
        let mut pieces = self.insert_sorted_at(self.root, &vals, 0, &mut prep)?;
        // Over one page: new roots, a level at a time.
        while pieces.len() > 1 {
            let mut keys = Vec::with_capacity(pieces.len() - 1);
            let mut children = Vec::with_capacity(pieces.len());
            for (i, (sep, page, _)) in pieces.into_iter().enumerate() {
                if i > 0 {
                    keys.push(sep);
                }
                children.push(page);
            }
            pieces = Vec::new();
            for (sep, node) in fit(Node::Internal(Internal { keys, children })) {
                let size = node.size();
                pieces.push((sep, self.store_new(&node)?, size));
            }
        }
        self.root = pieces[0].1;
        // An internal root left with one child is replaced by the child.
        loop {
            match self.load(self.root)? {
                Node::Internal(n) if n.keys.is_empty() => {
                    let old = self.root;
                    self.root = n.children[0];
                    self.pool.free(old)?;
                }
                _ => return Ok(()),
            }
        }
    }

    /// The pages a subtree became after `entries` (from `at` in the batch)
    /// went in: each with the separator before it (empty for the first)
    /// and its size.
    fn insert_sorted_at(
        &mut self,
        id: PageId,
        entries: &[(Vec<u8>, Option<Val>)],
        at: usize,
        prep: &mut Prep,
    ) -> Result<Vec<(Vec<u8>, PageId, usize)>> {
        // A leaf prepared on a worker: installed.
        if let Some(p) = prep.take(self, id, at..at + entries.len())? {
            return self.install_leaf(id, p);
        }
        // A leaf is merged as its page holds it, nothing decoded.
        let old = {
            let r = self.pool.fetch(id)?;
            let page = r.read();
            let v = View::new(&page).map_err(|reason| Error::Corrupt { page: id, reason })?;
            v.is_leaf().then(|| page.clone())
        };
        if let Some(old) = old {
            return self.merge_leaf(id, &old, entries);
        }
        match self.load(id)? {
            Node::Leaf(_) => Err(Error::Corrupt {
                page: id,
                reason: "a leaf read as an internal node",
            }),
            Node::Internal(node) => {
                let mut from = at;
                let mut groups = child_groups(&node, entries)
                    .into_iter()
                    .map(|(i, g)| {
                        let start = from;
                        from += g.len();
                        (i, g, start)
                    })
                    .peekable();
                let mut keys = Vec::with_capacity(node.keys.len());
                let mut children = Vec::with_capacity(node.children.len());
                // Children removals (or smaller values) left under the
                // minimum fill.
                let mut small = Vec::with_capacity(node.children.len());
                for (c, &child) in node.children.iter().enumerate() {
                    if c > 0 {
                        keys.push(node.keys[c - 1].clone());
                    }
                    if groups.peek().is_some_and(|(i, _, _)| *i == c) {
                        let (_, g, start) = groups.next().expect("peeked");
                        for (k, (sep, page, size)) in self
                            .insert_sorted_at(child, g, start, prep)?
                            .into_iter()
                            .enumerate()
                        {
                            if k > 0 {
                                keys.push(sep);
                            }
                            children.push(page);
                            small.push(size < MIN_FILL);
                        }
                    } else {
                        children.push(child);
                        small.push(false);
                    }
                }
                let mut node = Internal { keys, children };
                self.normalize(&mut node, small)?;
                let pieces = fit(Node::Internal(node));
                self.store_pieces(id, pieces)
            }
        }
    }

    /// The pages leaf `id` (its page `old`, copied out: the first piece
    /// may be written over it) became after `entries` (sorted; `None`
    /// removes) went in — its cells and the new ones merged by reference,
    /// cut into pieces as [`pack_leaf`] cuts, and encoded straight into
    /// the pages.
    fn merge_leaf(
        &self,
        id: PageId,
        old: &Page,
        entries: &[(Vec<u8>, Option<Val>)],
    ) -> Result<Vec<(Vec<u8>, PageId, usize)>> {
        let v = View::new(old).map_err(|reason| Error::Corrupt { page: id, reason })?;
        let (arena, ends) = old_keys(id, &v)?;
        let (merged, freed) = self.merge_cells(id, &v, &arena, &ends, entries)?;
        for page in freed {
            self.pool.free(page)?;
        }
        let mut out = Vec::new();
        for (p, range) in pack_cells(&merged).into_iter().enumerate() {
            let cells = &merged[range];
            let page = self.leaf_page(id, p, |page| encode_leaf(page, cells))?;
            let sep = if p == 0 {
                Vec::new()
            } else {
                cells[0].0.to_vec()
            };
            out.push((sep, page, cells_size(cells)));
        }
        Ok(out)
    }

    /// Leaf `id`'s cells (`v`, its old keys whole in `arena` up to `ends`)
    /// and `entries` merged by reference, and the overflow pages of the
    /// values replaced or removed (to free).
    #[allow(clippy::type_complexity)]
    fn merge_cells<'a>(
        &self,
        id: PageId,
        v: &View<'a>,
        arena: &'a [u8],
        ends: &[usize],
        entries: &'a [(Vec<u8>, Option<Val>)],
    ) -> Result<(Vec<(&'a [u8], ValRef<'a>)>, Vec<PageId>)> {
        let corrupt = |reason| Error::Corrupt { page: id, reason };
        let n = ends.len();
        let old_key = |i: usize| &arena[if i == 0 { 0 } else { ends[i - 1] }..ends[i]];
        let mut merged: Vec<(&[u8], ValRef)> = Vec::with_capacity(n + entries.len());
        let mut freed = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < n || j < entries.len() {
            let take_new = if i == n {
                true
            } else if j == entries.len() {
                false
            } else {
                match old_key(i).cmp(&entries[j].0) {
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => {
                        // Replaced (or removed): its overflow pages go.
                        if let ValRef::Overflow { first, len, .. } = v.value(i).map_err(corrupt)? {
                            for (page, _) in self.overflow_pages(first, len)? {
                                freed.push(page);
                            }
                        }
                        i += 1;
                        true
                    }
                }
            };
            if take_new {
                if let Some(val) = &entries[j].1 {
                    merged.push((&entries[j].0, val.as_ref()));
                }
                j += 1;
            } else {
                merged.push((old_key(i), v.value(i).map_err(corrupt)?));
                i += 1;
            }
        }
        Ok((merged, freed))
    }

    /// The page piece `p` of leaf `id` goes to — the first over `id` when
    /// it is writable, else (and the rest) a new one, `id` freed — written
    /// by `write`.
    fn leaf_page(&self, id: PageId, p: usize, write: impl FnOnce(&mut Page)) -> Result<PageId> {
        if p == 0 && self.pool.is_writable(id) {
            let r = self.pool.fetch(id)?;
            write(&mut *r.write()?);
            return Ok(id);
        }
        let r = self.pool.create(LEAF)?;
        write(&mut *r.write()?);
        if p == 0 {
            self.pool.free(id)?;
        }
        Ok(r.id())
    }

    /// [`merge_leaf`](Self::merge_leaf)'s work that only reads, for a
    /// worker: the pieces encoded into pages of their own, the overflow
    /// pages to free.
    fn prepare_leaf(&self, id: PageId, entries: &[(Vec<u8>, Option<Val>)]) -> Result<Prepared> {
        let old = {
            let r = self.pool.fetch(id)?;
            let page = r.read();
            page.clone()
        };
        let v = View::new(&old).map_err(|reason| Error::Corrupt { page: id, reason })?;
        if !v.is_leaf() {
            return Err(Error::Corrupt {
                page: id,
                reason: "a leaf read as an internal node",
            });
        }
        let (arena, ends) = old_keys(id, &v)?;
        let (merged, freed) = self.merge_cells(id, &v, &arena, &ends, entries)?;
        let pieces = pack_cells(&merged)
            .into_iter()
            .enumerate()
            .map(|(p, range)| {
                let cells = &merged[range];
                let mut page = Page::new(LEAF);
                encode_leaf(&mut page, cells);
                let sep = if p == 0 {
                    Vec::new()
                } else {
                    cells[0].0.to_vec()
                };
                (sep, page, cells_size(cells))
            })
            .collect();
        Ok(Prepared { freed, pieces })
    }

    /// Writes a prepared leaf as [`merge_leaf`](Self::merge_leaf) would:
    /// the same pages freed and taken, in the same order.
    fn install_leaf(&self, id: PageId, p: Prepared) -> Result<Vec<(Vec<u8>, PageId, usize)>> {
        for page in p.freed {
            self.pool.free(page)?;
        }
        let mut out = Vec::with_capacity(p.pieces.len());
        for (i, (sep, page, size)) in p.pieces.into_iter().enumerate() {
            let at = self.leaf_page(id, i, |w| {
                w.set_page_type(LEAF);
                w.payload_mut().copy_from_slice(page.payload());
            })?;
            out.push((sep, at, size));
        }
        Ok(out)
    }

    /// The leaves under `id` that sorted `entries` (from `at` in the batch)
    /// reach, in order, each with its run of the batch — grouped as
    /// [`insert_sorted_at`](Self::insert_sorted_at) groups them.
    fn leaf_jobs(
        &self,
        id: PageId,
        entries: &[(Vec<u8>, Option<Val>)],
        at: usize,
        out: &mut Vec<(PageId, std::ops::Range<usize>)>,
    ) -> Result<()> {
        let leaf = {
            let r = self.pool.fetch(id)?;
            let page = r.read();
            View::new(&page)
                .map_err(|reason| Error::Corrupt { page: id, reason })?
                .is_leaf()
        };
        if leaf {
            out.push((id, at..at + entries.len()));
            return Ok(());
        }
        let Node::Internal(node) = self.load(id)? else {
            return Err(Error::Corrupt {
                page: id,
                reason: "an internal node read as a leaf",
            });
        };
        let mut from = at;
        for (i, g) in child_groups(&node, entries) {
            self.leaf_jobs(node.children[i], g, from, out)?;
            from += g.len();
        }
        Ok(())
    }

    /// `jobs` (runs of `batch`) prepared on the tree's workers.
    #[allow(clippy::type_complexity)]
    fn prepare_window(
        &self,
        jobs: &[(PageId, std::ops::Range<usize>)],
        batch: &[(Vec<u8>, Option<Val>)],
    ) -> Result<std::collections::HashMap<PageId, (std::ops::Range<usize>, Prepared)>> {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        PARALLEL_WINDOWS.fetch_add(1, Ordering::Relaxed);
        let next = AtomicUsize::new(0);
        let results: Vec<Mutex<Option<Result<Prepared>>>> =
            jobs.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|sc| {
            for _ in 0..self.workers.min(jobs.len()) {
                sc.spawn(|| {
                    loop {
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        let Some((id, range)) = jobs.get(j) else {
                            break;
                        };
                        let r = self.prepare_leaf(*id, &batch[range.clone()]);
                        *results[j].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    }
                });
            }
        });
        let mut out = std::collections::HashMap::with_capacity(jobs.len());
        for ((id, range), r) in jobs.iter().zip(results) {
            let r = r
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every job ran");
            out.insert(*id, (range.clone(), r?));
        }
        Ok(out)
    }

    /// Merges or evens out the children of `node` under the minimum fill
    /// (`small` marks them) until each is full enough or `node` has one
    /// child.  Several neighbours may be small at once after a batch of
    /// removals, so a merge is checked again; two internal children merged
    /// have their own children evened out (a small grandchild may meet
    /// others at the seam).
    fn normalize(&mut self, node: &mut Internal, mut small: Vec<bool>) -> Result<()> {
        let mut i = 0;
        while i < node.children.len() {
            if !small[i] || node.children.len() == 1 {
                i += 1;
                continue;
            }
            let a = if i + 1 < node.children.len() {
                i
            } else {
                i - 1
            };
            let b = a + 1;
            let combined = match (self.load(node.children[a])?, self.load(node.children[b])?) {
                (Node::Leaf(mut x), Node::Leaf(y)) => {
                    x.entries.extend(y.entries);
                    Node::Leaf(x)
                }
                (Node::Internal(mut x), Node::Internal(y)) => {
                    x.keys.push(node.keys[a].clone());
                    x.keys.extend(y.keys);
                    x.children.extend(y.children);
                    let grand = x
                        .children
                        .iter()
                        .map(|&c| Ok(self.load(c)?.size() < MIN_FILL))
                        .collect::<Result<Vec<bool>>>()?;
                    if grand.iter().any(|&g| g) {
                        self.normalize(&mut x, grand)?;
                    }
                    Node::Internal(x)
                }
                _ => {
                    return Err(Error::Corrupt {
                        page: node.children[a],
                        reason: "siblings of different kinds",
                    });
                }
            };
            let size = combined.size();
            if size <= PAYLOAD_SIZE {
                node.children[a] = self.store(node.children[a], &combined)?;
                self.pool.free(node.children[b])?;
                node.keys.remove(a);
                node.children.remove(b);
                small.remove(b);
                small[a] = size < MIN_FILL;
                i = a;
            } else {
                let (l, sep, r) = combined.split();
                node.children[a] = self.store(node.children[a], &l)?;
                node.children[b] = self.store(node.children[b], &r)?;
                node.keys[a] = sep;
                small[a] = false;
                small[b] = false;
                i = b + 1;
            }
        }
        Ok(())
    }

    /// Stores a node's pieces: the first as the new version of `id`, the
    /// rest in new pages.
    fn store_pieces(
        &self,
        id: PageId,
        pieces: Vec<(Vec<u8>, Node)>,
    ) -> Result<Vec<(Vec<u8>, PageId, usize)>> {
        let mut out = Vec::with_capacity(pieces.len());
        for (i, (sep, node)) in pieces.into_iter().enumerate() {
            let size = node.size();
            let page = if i == 0 {
                self.store(id, &node)?
            } else {
                self.store_new(&node)?
            };
            out.push((sep, page, size));
        }
        Ok(out)
    }

    /// Applies a child's result to its parent `node`, rebalancing an
    /// underfull child.  Returns whether `node` changed.
    fn absorb(&mut self, node: &mut Internal, i: usize, out: Out) -> Result<bool> {
        match out {
            Out::One(c, size) => {
                let changed = node.children[i] != c;
                node.children[i] = c;
                if size < MIN_FILL && node.children.len() > 1 {
                    self.rebalance(node, i)?;
                    return Ok(true);
                }
                Ok(changed)
            }
            Out::Two(l, sep, r) => {
                node.children[i] = l;
                node.keys.insert(i, sep);
                node.children.insert(i + 1, r);
                Ok(true)
            }
        }
    }

    /// Merges child `i` of `node` with a sibling, or evens the pair out if
    /// they do not fit in one page.
    fn rebalance(&mut self, node: &mut Internal, i: usize) -> Result<()> {
        let a = if i + 1 < node.children.len() {
            i
        } else {
            i - 1
        };
        let b = a + 1;
        let combined = match (self.load(node.children[a])?, self.load(node.children[b])?) {
            (Node::Leaf(mut x), Node::Leaf(y)) => {
                x.entries.extend(y.entries);
                Node::Leaf(x)
            }
            (Node::Internal(mut x), Node::Internal(y)) => {
                x.keys.push(node.keys[a].clone());
                x.keys.extend(y.keys);
                x.children.extend(y.children);
                Node::Internal(x)
            }
            _ => {
                return Err(Error::Corrupt {
                    page: node.children[a],
                    reason: "siblings of different kinds",
                });
            }
        };
        if combined.size() <= PAYLOAD_SIZE {
            node.children[a] = self.store(node.children[a], &combined)?;
            self.pool.free(node.children[b])?;
            node.keys.remove(a);
            node.children.remove(b);
        } else {
            let (l, sep, r) = combined.split();
            node.children[a] = self.store(node.children[a], &l)?;
            node.children[b] = self.store(node.children[b], &r)?;
            node.keys[a] = sep;
        }
        Ok(())
    }

    /// Removes `key`; returns whether it was present.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool> {
        match self.remove_at(self.root, key)? {
            None => Ok(false),
            Some(out) => {
                self.set_root(out)?;
                Ok(true)
            }
        }
    }

    fn remove_at(&mut self, id: PageId, key: &[u8]) -> Result<Option<Out>> {
        match self.load(id)? {
            Node::Leaf(mut leaf) => {
                let Ok(i) = leaf.search(key) else {
                    return Ok(None);
                };
                let (_, val) = leaf.entries.remove(i);
                self.free_val(&val)?;
                Ok(Some(self.finish(id, Node::Leaf(leaf))?))
            }
            Node::Internal(mut node) => {
                let i = node.child_index(key);
                let Some(out) = self.remove_at(node.children[i], key)? else {
                    return Ok(None);
                };
                if self.absorb(&mut node, i, out)? {
                    Ok(Some(self.finish(id, Node::Internal(node))?))
                } else {
                    Ok(Some(Out::One(id, node.size())))
                }
            }
        }
    }

    /// Frees every page of the tree (it must not be used afterwards).
    pub fn destroy(self) -> Result<()> {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            match self.load(id)? {
                Node::Internal(n) => stack.extend(n.children),
                Node::Leaf(leaf) => {
                    for (_, v) in &leaf.entries {
                        self.free_val(v)?;
                    }
                }
            }
            self.pool.free(id)?;
        }
        Ok(())
    }

    // ----------------------------------------------------------------- check

    /// Verifies the tree's invariants: keys strictly ascending and within
    /// their separators, every leaf at the same depth, every node fitting
    /// its page and (except the root) at least [`MIN_FILL`] full, overflow
    /// chains matching their lengths, no page used twice.
    pub fn check(&self) -> Result<TreeCheck> {
        let mut out = TreeCheck::default();
        let mut leaf_depth = None;
        self.check_node(self.root, None, None, 1, true, &mut leaf_depth, &mut out)?;
        out.depth = leaf_depth.unwrap_or(1);
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn check_node(
        &self,
        id: PageId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        depth: usize,
        is_root: bool,
        leaf_depth: &mut Option<usize>,
        out: &mut TreeCheck,
    ) -> Result<()> {
        let bad = |reason| Err(Error::Corrupt { page: id, reason });
        if !out.pages.insert(id.0) {
            return bad("page used twice in the tree");
        }
        let node = self.load(id)?;
        let size = node.size();
        if size > PAYLOAD_SIZE {
            return bad("node larger than a page");
        }
        if !is_root && size < MIN_FILL {
            return bad("node under the minimum fill");
        }
        let keys: Vec<&[u8]> = match &node {
            Node::Leaf(l) => l.entries.iter().map(|(k, _)| k.as_slice()).collect(),
            Node::Internal(n) => n.keys.iter().map(Vec::as_slice).collect(),
        };
        if keys.windows(2).any(|w| w[0] >= w[1]) {
            return bad("keys not strictly ascending");
        }
        if keys
            .iter()
            .any(|k| lo.is_some_and(|lo| *k < lo) || hi.is_some_and(|hi| *k >= hi))
        {
            return bad("key outside its separators");
        }
        match &node {
            Node::Leaf(leaf) => {
                if *leaf_depth.get_or_insert(depth) != depth {
                    return bad("leaves at different depths");
                }
                out.entries += leaf.entries.len() as u64;
                for (_, v) in &leaf.entries {
                    if let Val::Overflow { first, len, .. } = v {
                        for (page, _) in self.overflow_pages(*first, *len)? {
                            if !out.pages.insert(page.0) {
                                return Err(Error::Corrupt {
                                    page,
                                    reason: "overflow page used twice",
                                });
                            }
                        }
                    }
                }
            }
            Node::Internal(n) => {
                if n.children.len() != n.keys.len() + 1 || (is_root && n.keys.is_empty()) {
                    return bad("internal node with the wrong number of children");
                }
                for (i, child) in n.children.iter().enumerate() {
                    let clo = if i == 0 {
                        lo
                    } else {
                        Some(n.keys[i - 1].as_slice())
                    };
                    let chi = if i == n.keys.len() {
                        hi
                    } else {
                        Some(n.keys[i].as_slice())
                    };
                    self.check_node(*child, clo, chi, depth + 1, false, leaf_depth, out)?;
                }
            }
        }
        Ok(())
    }
}

/// A position between two entries of a tree; moves both ways.
pub struct Cursor<'t> {
    tree: &'t BTree,
    /// The internal pages above the current leaf: each with the child we
    /// are in and its number of children.
    path: Vec<(PageId, usize, usize)>,
    /// The current leaf, pinned, read in place.
    leaf: Option<PageRef<'t>>,
    leaf_len: usize,
    /// The cursor sits before entry `pos` of the leaf.
    pos: usize,
}

impl std::fmt::Debug for Cursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl<'t> Cursor<'t> {
    /// Descends from `id` to a leaf, taking at each internal node the child
    /// `pick` chooses (pages read in place); `at` then places the cursor in
    /// the leaf.
    fn descend(
        &mut self,
        mut id: PageId,
        pick: impl Fn(&View) -> std::result::Result<usize, &'static str>,
        at: impl Fn(&View) -> std::result::Result<usize, &'static str>,
    ) -> Result<()> {
        loop {
            let r = self.tree.pool.fetch(id)?;
            let page = r.read();
            let corrupt = |reason| Error::Corrupt { page: id, reason };
            let v = View::new(&page).map_err(corrupt)?;
            if v.is_leaf() {
                self.leaf_len = v.len();
                self.pos = at(&v).map_err(corrupt)?;
                drop(page);
                self.leaf = Some(r);
                return Ok(());
            }
            let i = pick(&v).map_err(corrupt)?;
            let child = v.child(i).map_err(corrupt)?;
            self.path.push((id, i, v.len() + 1));
            id = child;
        }
    }

    /// Moves before the first entry `>= key`.
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        self.path.clear();
        self.descend(
            self.tree.root,
            |v| v.child_index(key),
            |v| v.partition(|k| k.order(key).is_lt()),
        )
    }

    /// [`seek`](Self::seek), without a descent when `key` falls inside
    /// the current leaf (after its first key, at or before its last): for
    /// seeks to keys near one another, most stay in the leaf they are in.
    pub fn seek_near(&mut self, key: &[u8]) -> Result<()> {
        if let Some(r) = &self.leaf
            && self.leaf_len > 0
        {
            let page = r.read();
            let corrupt = |reason| Error::Corrupt {
                page: r.id(),
                reason,
            };
            let v = View::new(&page).map_err(corrupt)?;
            if v.key(0).map_err(corrupt)?.order(key).is_lt()
                && !v
                    .key(self.leaf_len - 1)
                    .map_err(corrupt)?
                    .order(key)
                    .is_lt()
            {
                self.pos = v.partition(|k| k.order(key).is_lt()).map_err(corrupt)?;
                return Ok(());
            }
        }
        self.seek(key)
    }

    /// Moves before the first entry `> key`.
    pub fn seek_after(&mut self, key: &[u8]) -> Result<()> {
        self.path.clear();
        self.descend(
            self.tree.root,
            |v| v.child_index(key),
            |v| v.partition(|k| k.order(key).is_le()),
        )
    }

    /// Moves after the last entry.
    pub fn seek_end(&mut self) -> Result<()> {
        self.path.clear();
        self.descend(self.tree.root, |v| Ok(v.len()), |v| Ok(v.len()))
    }

    /// Entry `i` of the current leaf.
    fn entry(&self, i: usize) -> Result<(Vec<u8>, Vec<u8>)> {
        let (mut key, mut val) = (Vec::new(), Vec::new());
        self.entry_into(i, &mut key, &mut val)?;
        Ok((key, val))
    }

    /// Entry `i` of the current leaf, into `key` and `val` (replacing what
    /// they held, their buffers reused).
    fn entry_into(&self, i: usize, key: &mut Vec<u8>, val: &mut Vec<u8>) -> Result<()> {
        self.key_into(i, key)?;
        self.value_into(i, val)
    }

    /// Entry `i`'s key into `key`.
    fn key_into(&self, i: usize, key: &mut Vec<u8>) -> Result<()> {
        let r = self.leaf.as_ref().expect("a cursor is always in a leaf");
        let page = r.read();
        let corrupt = |reason| Error::Corrupt {
            page: r.id(),
            reason,
        };
        let v = View::new(&page).map_err(corrupt)?;
        key.clear();
        v.key(i).map_err(corrupt)?.append_to(key);
        Ok(())
    }

    /// Entry `i`'s value into `val`.
    fn value_into(&self, i: usize, val: &mut Vec<u8>) -> Result<()> {
        let r = self.leaf.as_ref().expect("a cursor is always in a leaf");
        let page = r.read();
        let corrupt = |reason| Error::Corrupt {
            page: r.id(),
            reason,
        };
        let v = View::new(&page).map_err(corrupt)?;
        val.clear();
        let id = r.id();
        match v.value(i).map_err(corrupt)? {
            ValRef::Overflow { first, len, raw } => {
                drop(page);
                self.tree
                    .read_ref(id, ValRef::Overflow { first, len, raw }, val)?;
            }
            other => self.tree.read_ref(id, other, val)?,
        }
        Ok(())
    }

    /// Child `i` of internal page `id`.
    fn child(&self, id: PageId, i: usize) -> Result<PageId> {
        let r = self.tree.pool.fetch(id)?;
        let page = r.read();
        let corrupt = |reason| Error::Corrupt { page: id, reason };
        View::new(&page).and_then(|v| v.child(i)).map_err(corrupt)
    }

    /// The entry after the cursor, moving past it.
    pub fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let (mut key, mut val) = (Vec::new(), Vec::new());
        Ok(self.next_into(&mut key, &mut val)?.then_some((key, val)))
    }

    /// [`next_entry`](Self::next_entry) into `key` and `val` (their buffers
    /// reused); false at the end.
    pub fn next_into(&mut self, key: &mut Vec<u8>, val: &mut Vec<u8>) -> Result<bool> {
        self.next_within(key, val, &|_| true)
    }

    /// [`next_into`](Self::next_into), the value read only when `keep` holds
    /// for the key: false at the end and at a key `keep` refuses (its value
    /// — perhaps long, compressed, in overflow pages — left unread).
    pub fn next_within(
        &mut self,
        key: &mut Vec<u8>,
        val: &mut Vec<u8>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> Result<bool> {
        loop {
            if self.pos < self.leaf_len {
                self.key_into(self.pos, key)?;
                if !keep(key) {
                    return Ok(false);
                }
                self.value_into(self.pos, val)?;
                self.pos += 1;
                return Ok(true);
            }
            // Up to the first ancestor with a child to the right, then down
            // its leftmost path.
            loop {
                let Some((id, i, n)) = self.path.pop() else {
                    return Ok(false);
                };
                if i + 1 < n {
                    let child = self.child(id, i + 1)?;
                    self.path.push((id, i + 1, n));
                    self.descend(child, |_| Ok(0), |_| Ok(0))?;
                    break;
                }
            }
        }
    }

    /// The entry before the cursor, moving before it.
    pub fn prev_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if self.pos > 0 {
                self.pos -= 1;
                return self.entry(self.pos).map(Some);
            }
            loop {
                let Some((id, i, n)) = self.path.pop() else {
                    return Ok(None);
                };
                if i > 0 {
                    let child = self.child(id, i - 1)?;
                    self.path.push((id, i - 1, n));
                    self.descend(child, |v| Ok(v.len()), |v| Ok(v.len()))?;
                    break;
                }
            }
        }
    }
}

/// An iterator over a key range (see [`BTree::range`]).
pub struct Range<'t> {
    cursor: Cursor<'t>,
    /// Ascending: the upper bound; descending: the lower bound.
    end: Bound<Vec<u8>>,
    reverse: bool,
    done: bool,
}

impl std::fmt::Debug for Range<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Range")
            .field("reverse", &self.reverse)
            .finish_non_exhaustive()
    }
}

impl Range<'_> {
    /// The next entry into `key` and `val` (their buffers reused): what
    /// [`Iterator::next`] gives without allocating; false at the end.
    pub fn next_into(&mut self, key: &mut Vec<u8>, val: &mut Vec<u8>) -> Result<bool> {
        if self.done {
            return Ok(false);
        }
        let more = if self.reverse {
            match self.cursor.prev_entry() {
                Ok(Some((k, v))) => {
                    (*key, *val) = (k, v);
                    Ok(true)
                }
                Ok(None) => Ok(false),
                Err(e) => Err(e),
            }
        } else {
            // The end checked on the key, before its value is read.
            let end = &self.end;
            self.cursor.next_within(key, val, &|k| match end {
                Bound::Unbounded => true,
                Bound::Included(e) => k <= &e[..],
                Bound::Excluded(e) => k < &e[..],
            })
        };
        match more {
            Ok(true) => {}
            Ok(false) => {
                self.done = true;
                return Ok(false);
            }
            Err(e) => {
                self.done = true;
                return Err(e);
            }
        }
        if self.inside(key) {
            Ok(true)
        } else {
            self.done = true;
            Ok(false)
        }
    }

    fn inside(&self, k: &[u8]) -> bool {
        match (&self.end, self.reverse) {
            (Bound::Unbounded, _) => true,
            (Bound::Included(e), false) => k <= &e[..],
            (Bound::Excluded(e), false) => k < &e[..],
            (Bound::Included(e), true) => k >= &e[..],
            (Bound::Excluded(e), true) => k > &e[..],
        }
    }
}

impl Iterator for Range<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = if self.reverse {
            self.cursor.prev_entry()
        } else {
            let (mut k, mut v) = (Vec::new(), Vec::new());
            let end = &self.end;
            self.cursor
                .next_within(&mut k, &mut v, &|key| match end {
                    Bound::Unbounded => true,
                    Bound::Included(e) => key <= &e[..],
                    Bound::Excluded(e) => key < &e[..],
                })
                .map(|more| more.then_some((k, v)))
        };
        let (k, v) = match item {
            Ok(Some(kv)) => kv,
            Ok(None) => {
                self.done = true;
                return None;
            }
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        if self.inside(&k) {
            Some(Ok((k, v)))
        } else {
            self.done = true;
            None
        }
    }
}
/// A node in pieces that each fit a page (even splits): each with the
/// separator before it (empty for the first).
fn fit(node: Node) -> Vec<(Vec<u8>, Node)> {
    let mut out = vec![(Vec::new(), node)];
    while let Some(i) = out.iter().position(|(_, n)| n.size() > PAYLOAD_SIZE) {
        let (sep, n) = out.remove(i);
        let (l, mid, r) = n.split();
        out.insert(i, (mid, r));
        out.insert(i, (sep, l));
    }
    out
}

/// The encoded size of a leaf of `cells`.
fn cells_size(cells: &[(&[u8], ValRef)]) -> usize {
    let bytes = cells.iter().map(|(k, v)| cell_size(k, *v)).sum();
    let prefix = match (cells.first(), cells.last()) {
        (Some((a, _)), Some((b, _))) => key_prefix(a, b, cells.len()),
        _ => 0,
    };
    leaf_size(bytes, cells.len(), prefix)
}

/// [`pack_leaf`] of cells by reference: the ranges of `cells` each piece
/// holds — the same pieces.
fn pack_cells(cells: &[(&[u8], ValRef)]) -> Vec<std::ops::Range<usize>> {
    if cells_size(cells) <= PAYLOAD_SIZE {
        return std::iter::once(0..cells.len()).collect();
    }
    let target = PAYLOAD_SIZE * 7 / 8;
    let mut pieces: Vec<std::ops::Range<usize>> = std::iter::once(0..0).collect();
    let mut bytes = 0;
    let mut size = leaf_size(0, 0, 0);
    for (i, (k, v)) in cells.iter().enumerate() {
        let cell = cell_size(k, *v);
        let piece = pieces.last().expect("one").clone();
        let grown = if piece.is_empty() {
            leaf_size(cell, 1, 0)
        } else {
            leaf_size(
                bytes + cell,
                piece.len() + 1,
                key_prefix(cells[piece.start].0, k, piece.len() + 1),
            )
        };
        if grown > target && !piece.is_empty() {
            pieces.push(i..i);
            bytes = 0;
            size = leaf_size(cell, 1, 0);
        } else {
            size = grown;
        }
        bytes += cell;
        pieces.last_mut().expect("one").end = i + 1;
    }
    // A small last piece: evened out with the one before (as `fit` splits
    // a leaf: by bytes, until each piece fits).
    if pieces.len() > 1 && size < MIN_FILL {
        let last = pieces.pop().expect("two");
        let prev = pieces.pop().expect("two");
        fit_cells(cells, prev.start..last.end, &mut pieces);
    }
    pieces
}

/// `range` of `cells` in pieces that each fit a page, split as
/// [`Node::split`] splits a leaf, appended to `out` in order.
fn fit_cells(
    cells: &[(&[u8], ValRef)],
    range: std::ops::Range<usize>,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    if cells_size(&cells[range.clone()]) <= PAYLOAD_SIZE {
        out.push(range);
        return;
    }
    let sizes: Vec<usize> = cells[range.clone()]
        .iter()
        .map(|(k, v)| cell_size(k, *v))
        .collect();
    let k = range.start + balance_point(&sizes, 1, sizes.len() - 1);
    fit_cells(cells, range.start..k, out);
    fit_cells(cells, k..range.end, out);
}

/// The keys of leaf `v` (page `id`) whole, one after another, and where
/// each ends.
fn old_keys(id: PageId, v: &View) -> Result<(Vec<u8>, Vec<usize>)> {
    let mut arena = Vec::new();
    let mut ends = Vec::with_capacity(v.len());
    for i in 0..v.len() {
        v.key(i)
            .map_err(|reason| Error::Corrupt { page: id, reason })?
            .append_to(&mut arena);
        ends.push(arena.len());
    }
    Ok((arena, ends))
}

/// Sorted `entries` by the child of `node` they go to: each child's index
/// and its run of them.
#[allow(clippy::type_complexity)]
fn child_groups<'e>(
    node: &Internal,
    entries: &'e [(Vec<u8>, Option<Val>)],
) -> Vec<(usize, &'e [(Vec<u8>, Option<Val>)])> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        // Child `i` holds the keys below separator `i`: its run ends at the
        // first entry not below it (a search per child, not per entry).
        let i = node.child_index(&entries[start].0);
        let end = match node.keys.get(i) {
            Some(sep) => {
                start + entries[start..].partition_point(|(k, _)| k.as_slice() < sep.as_slice())
            }
            None => entries.len(),
        };
        groups.push((i, &entries[start..end]));
        start = end;
    }
    groups
}

/// A leaf merged on a worker ([`BTree::prepare_leaf`]), to install.
struct Prepared {
    freed: Vec<PageId>,
    pieces: Vec<(Vec<u8>, Page, usize)>,
}

/// Leaves a sorted batch reaches, prepared ahead on workers a window at a
/// time (so the pages waiting stay few), taken as the batch reaches them.
struct Prep<'b> {
    /// The batch.
    batch: &'b [(Vec<u8>, Option<Val>)],
    /// Every leaf the batch reaches, in order, with its run of the batch.
    jobs: Vec<(PageId, std::ops::Range<usize>)>,
    next: usize,
    /// Prepared leaves, each with the run it was prepared for.
    ready: std::collections::HashMap<PageId, (std::ops::Range<usize>, Prepared)>,
}

/// Windows of leaves prepared on workers so far ([`parallel_windows`]).
static PARALLEL_WINDOWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many windows of a sorted batch's leaves were prepared on workers
/// (in this process) — for tests that the parallel path ran.
pub fn parallel_windows() -> u64 {
    PARALLEL_WINDOWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Leaves at least this many, or the batch is applied on one thread.
const PARALLEL_LEAVES: usize = 64;
/// Leaves prepared a window at a time, per worker.
const WINDOW_PER_WORKER: usize = 128;

impl<'b> Prep<'b> {
    /// The leaves `batch` reaches — none when the tree has one worker or
    /// the batch reaches few leaves.
    fn new(tree: &BTree, batch: &'b [(Vec<u8>, Option<Val>)]) -> Result<Self> {
        let mut prep = Prep {
            batch,
            jobs: Vec::new(),
            next: 0,
            ready: std::collections::HashMap::new(),
        };
        if tree.workers > 1 && batch.len() >= PARALLEL_LEAVES {
            tree.leaf_jobs(tree.root, batch, 0, &mut prep.jobs)?;
            if prep.jobs.len() < PARALLEL_LEAVES {
                prep.jobs.clear();
            }
        }
        Ok(prep)
    }

    /// Leaf `id`'s preparation for the batch's run `run`, if it was
    /// prepared for that (the next window prepared first when `id` begins
    /// it); else the leaf is merged where it is.
    fn take(
        &mut self,
        tree: &BTree,
        id: PageId,
        run: std::ops::Range<usize>,
    ) -> Result<Option<Prepared>> {
        if self.jobs.is_empty() {
            return Ok(None);
        }
        if !self.ready.contains_key(&id) && self.jobs.get(self.next).is_some_and(|(j, _)| *j == id)
        {
            let end = (self.next + tree.workers * WINDOW_PER_WORKER).min(self.jobs.len());
            self.ready = tree.prepare_window(&self.jobs[self.next..end], self.batch)?;
            self.next = end;
        }
        Ok(self
            .ready
            .remove(&id)
            .filter(|(r, _)| *r == run)
            .map(|(_, p)| p))
    }
}

/// A leaf's entries in pieces of about 7/8 of a page each (the last at
/// least [`MIN_FILL`]): each with its first key as the separator before it
/// (empty for the first).  Decoded entries: the reference [`pack_cells`]
/// is checked against.
#[cfg(test)]
fn pack_leaf(entries: Vec<(Vec<u8>, Val)>) -> Vec<(Vec<u8>, Node)> {
    let leaf = Leaf { entries };
    if leaf.size() <= PAYLOAD_SIZE {
        return vec![(Vec::new(), Node::Leaf(leaf))];
    }
    let target = PAYLOAD_SIZE * 7 / 8;
    let mut pieces: Vec<Vec<(Vec<u8>, Val)>> = vec![Vec::new()];
    // The piece being filled: its cells' bytes, and its size as stored —
    // its keys' shared prefix stored once.
    let mut cells = 0;
    let mut size = leaf_size(0, 0, 0);
    for (k, v) in leaf.entries {
        let cell = leaf_cell_size(&k, &v);
        let piece = pieces.last().expect("one");
        let grown = match piece.first() {
            None => leaf_size(cell, 1, 0),
            Some((first, _)) => leaf_size(
                cells + cell,
                piece.len() + 1,
                key_prefix(first, &k, piece.len() + 1),
            ),
        };
        if grown > target && !piece.is_empty() {
            pieces.push(Vec::new());
            cells = 0;
            size = leaf_size(cell, 1, 0);
        } else {
            size = grown;
        }
        cells += cell;
        pieces.last_mut().expect("one").push((k, v));
    }
    // A small last piece: evened out with the one before.
    if pieces.len() > 1 && size < MIN_FILL {
        let last = pieces.pop().expect("two");
        let mut prev = pieces.pop().expect("two");
        prev.extend(last);
        for (_, n) in fit(Node::Leaf(Leaf { entries: prev })) {
            if let Node::Leaf(l) = n {
                pieces.push(l.entries);
            }
        }
    }
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, entries)| {
            let sep = if i == 0 {
                Vec::new()
            } else {
                entries[0].0.clone()
            };
            (sep, Node::Leaf(Leaf { entries }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pack_cells` cuts a leaf where `pack_leaf` (decoded entries) does,
    /// over leaves of every shape: short and long keys sharing prefixes or
    /// not, inline, compressed and overflow values, pieces left small.
    #[test]
    fn pack_cells_cuts_as_pack_leaf() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for round in 0..2000 {
            let count = 1 + next(400) as usize;
            let shared = next(40) as usize;
            let mut keys: Vec<Vec<u8>> = (0..count)
                .map(|_| {
                    let mut k = vec![b'k'; shared];
                    let len = next(if round % 7 == 0 { 300 } else { 12 }) as usize;
                    k.extend((0..len).map(|_| next(256) as u8));
                    k
                })
                .collect();
            keys.sort();
            keys.dedup();
            let entries: Vec<(Vec<u8>, Val)> = keys
                .into_iter()
                .map(|k| {
                    let v = match next(10) {
                        0 => Val::Packed {
                            raw: 999,
                            block: vec![3; next(200) as usize],
                        },
                        1 => Val::Overflow {
                            first: PageId(7),
                            len: 50_000,
                            raw: (next(2) == 0).then_some(80_000),
                        },
                        _ => Val::Inline(vec![
                            1;
                            next(if round % 5 == 0 { 1500 } else { 60 }) as usize
                        ]),
                    };
                    (k, v)
                })
                .collect();
            let refs: Vec<(&[u8], ValRef)> = entries
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_ref()))
                .collect();
            let got: Vec<(Vec<u8>, Vec<(Vec<u8>, Val)>)> = pack_cells(&refs)
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    let sep = if i == 0 {
                        Vec::new()
                    } else {
                        entries[r.start].0.clone()
                    };
                    (sep, entries[r].to_vec())
                })
                .collect();
            let want: Vec<(Vec<u8>, Vec<(Vec<u8>, Val)>)> = pack_leaf(entries.clone())
                .into_iter()
                .map(|(sep, n)| match n {
                    Node::Leaf(l) => (sep, l.entries),
                    Node::Internal(_) => unreachable!("leaves"),
                })
                .collect();
            assert_eq!(got, want, "round {round}");
            for (_, piece) in &got {
                let refs: Vec<(&[u8], ValRef)> = piece
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_ref()))
                    .collect();
                assert!(cells_size(&refs) <= PAYLOAD_SIZE, "round {round}");
                assert_eq!(
                    cells_size(&refs),
                    Node::Leaf(Leaf {
                        entries: piece.clone()
                    })
                    .size()
                );
            }
        }
    }
}
