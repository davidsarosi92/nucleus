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

use crate::buffer::BufferPool;
use crate::error::{Error, Result};
use crate::node::{
    Internal, Leaf, MAX_INLINE_CELL, MAX_KEY, MIN_FILL, Node, OVERFLOW, OVERFLOW_CAPACITY, Val,
    decode_overflow, encode_overflow, leaf_cell_size,
};
use crate::page::{PAYLOAD_SIZE, PageId};

/// A B+tree rooted at a page.
#[derive(Debug)]
pub struct BTree {
    pool: Arc<BufferPool>,
    root: PageId,
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
        Ok(Self { pool, root })
    }

    /// Opens the tree rooted at `root`.
    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Self {
        Self { pool, root }
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
        let inline = Val::Inline(value.to_vec());
        if leaf_cell_size(key, &inline) - 2 <= MAX_INLINE_CELL {
            return Ok(inline);
        }
        let mut next = PageId(0);
        for chunk in value.chunks(OVERFLOW_CAPACITY).rev() {
            let r = self.pool.create(OVERFLOW)?;
            encode_overflow(&mut *r.write()?, next, chunk);
            next = r.id();
        }
        Ok(Val::Overflow {
            first: next,
            len: value.len() as u64,
        })
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

    fn read_val(&self, val: &Val) -> Result<Vec<u8>> {
        match val {
            Val::Inline(v) => Ok(v.clone()),
            Val::Overflow { first, len } => {
                let mut out = Vec::with_capacity(*len as usize);
                for (_, data) in self.overflow_pages(*first, *len)? {
                    out.extend_from_slice(&data);
                }
                Ok(out)
            }
        }
    }

    fn free_val(&self, val: &Val) -> Result<()> {
        if let Val::Overflow { first, len } = val {
            for (id, _) in self.overflow_pages(*first, *len)? {
                self.pool.free(id)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ read

    /// The value of `key`, if present.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut id = self.root;
        loop {
            match self.load(id)? {
                Node::Internal(n) => id = n.children[n.child_index(key)],
                Node::Leaf(leaf) => {
                    return match leaf.search(key) {
                        Ok(i) => Ok(Some(self.read_val(&leaf.entries[i].1)?)),
                        Err(_) => Ok(None),
                    };
                }
            }
        }
    }

    /// A cursor, positioned before the first entry.
    pub fn cursor(&self) -> Result<Cursor<'_>> {
        let mut c = Cursor {
            tree: self,
            path: Vec::new(),
            leaf: Leaf::default(),
            pos: 0,
        };
        c.seek(&[])?;
        Ok(c)
    }

    /// The entries with keys in `(start, end)`, in ascending order.
    pub fn range(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Range<'_>> {
        let mut cursor = self.cursor()?;
        match start {
            Bound::Unbounded => {}
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
        let mut cursor = self.cursor()?;
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
                    if let Val::Overflow { first, len } = v {
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
    /// The internal nodes above the current leaf, and which child we are in.
    path: Vec<(Internal, usize)>,
    leaf: Leaf,
    /// The cursor sits before `leaf.entries[pos]`.
    pos: usize,
}

impl std::fmt::Debug for Cursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cursor")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

impl Cursor<'_> {
    /// Descends from `id` to a leaf, taking at each internal node the child
    /// `pick` chooses.
    fn descend(&mut self, mut id: PageId, pick: impl Fn(&Internal) -> usize) -> Result<()> {
        loop {
            match self.tree.load(id)? {
                Node::Internal(n) => {
                    let i = pick(&n);
                    id = n.children[i];
                    self.path.push((n, i));
                }
                Node::Leaf(leaf) => {
                    self.leaf = leaf;
                    return Ok(());
                }
            }
        }
    }

    /// Moves before the first entry `>= key`.
    pub fn seek(&mut self, key: &[u8]) -> Result<()> {
        self.path.clear();
        self.descend(self.tree.root, |n| n.child_index(key))?;
        self.pos = self
            .leaf
            .entries
            .partition_point(|(k, _)| k.as_slice() < key);
        Ok(())
    }

    /// Moves before the first entry `> key`.
    pub fn seek_after(&mut self, key: &[u8]) -> Result<()> {
        self.path.clear();
        self.descend(self.tree.root, |n| n.child_index(key))?;
        self.pos = self
            .leaf
            .entries
            .partition_point(|(k, _)| k.as_slice() <= key);
        Ok(())
    }

    /// Moves after the last entry.
    pub fn seek_end(&mut self) -> Result<()> {
        self.path.clear();
        self.descend(self.tree.root, |n| n.children.len() - 1)?;
        self.pos = self.leaf.entries.len();
        Ok(())
    }

    /// The entry after the cursor, moving past it.
    pub fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some((k, v)) = self.leaf.entries.get(self.pos) {
                self.pos += 1;
                return Ok(Some((k.clone(), self.tree.read_val(v)?)));
            }
            // Up to the first ancestor with a child to the right, then down
            // its leftmost path.
            loop {
                let Some((node, i)) = self.path.pop() else {
                    return Ok(None);
                };
                if i + 1 < node.children.len() {
                    let child = node.children[i + 1];
                    self.path.push((node, i + 1));
                    self.descend(child, |_| 0)?;
                    self.pos = 0;
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
                let (k, v) = &self.leaf.entries[self.pos];
                return Ok(Some((k.clone(), self.tree.read_val(v)?)));
            }
            loop {
                let Some((node, i)) = self.path.pop() else {
                    return Ok(None);
                };
                if i > 0 {
                    let child = node.children[i - 1];
                    self.path.push((node, i - 1));
                    self.descend(child, |n| n.children.len() - 1)?;
                    self.pos = self.leaf.entries.len();
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

impl Iterator for Range<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = if self.reverse {
            self.cursor.prev_entry()
        } else {
            self.cursor.next_entry()
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
        let inside = match (&self.end, self.reverse) {
            (Bound::Unbounded, _) => true,
            (Bound::Included(e), false) => k <= *e,
            (Bound::Excluded(e), false) => k < *e,
            (Bound::Included(e), true) => k >= *e,
            (Bound::Excluded(e), true) => k > *e,
        };
        if inside {
            Some(Ok((k, v)))
        } else {
            self.done = true;
            None
        }
    }
}
