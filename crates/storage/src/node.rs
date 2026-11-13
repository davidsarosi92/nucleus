//! B+tree node format.
//!
//! A node is one page.  Payload layout (little-endian):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0 | 1 | kind: 1 leaf, 2 internal, 3 leaf with a key prefix |
//! | 1 | 1 | reserved |
//! | 2 | 2 | cell count `n` |
//! | 4 | 8 | internal: the leftmost child; leaf: 0; a leaf with a key prefix: its length `u16` and offset `u16` |
//! | 12 | 2 × n | cell offsets (from the payload start), in key order |
//! | … | | free space, then the cells, packed at the end |
//!
//! Leaf cell: `klen u16, flags u8, vlen u32, key, value` — with flag 1
//! (overflow) the value is `first page u64, total length u64` and the bytes
//! live in a chain of overflow pages; with flag 2 (compressed, format
//! version 2) it is `length u32` uncompressed and an LZ block ([`crate::lz`]);
//! with flag 3 (compressed, in overflow pages) `first page u64, block
//! length u64, length u64`.  Internal cell: `klen u16, child u64,
//! key`; the child holds keys `≥ key` (and `<` the next cell's key).
//!
//! A leaf with a key prefix (format version 2, §M14.20 V5) stores the
//! prefix all its keys share once, among the cells, and only the rest of
//! each key in its cell: the keys of a table's rows share their table and
//! index ids and the high bytes of their primary keys.
//!
//! Overflow page payload: `next u64 (0 = last), len u32, data`.
//!
//! Reads go through a [`View`] of the page in place.  Nodes are decoded
//! into [`Leaf`] / [`Internal`] to be changed and encoded back — simple and
//! hard to get wrong, at a copy per node — but for a batch of sorted
//! changes to a leaf (every bulk write): its cells and the new ones are
//! merged by reference and written by [`encode_leaf`], nothing decoded.

use crate::page::{PAYLOAD_SIZE, Page, PageId, PageType, read_u32, read_u64, write_u32, write_u64};

/// Page type of a leaf node.
pub const LEAF: PageType = PageType(16);
/// Page type of an internal node.
pub const INTERNAL: PageType = PageType(17);
/// Page type of an overflow page.
pub const OVERFLOW: PageType = PageType(18);

/// The longest key the tree accepts.
pub const MAX_KEY: usize = 1024;
/// A leaf cell larger than this moves its value to overflow pages.  With
/// [`MAX_KEY`] it keeps every cell under a quarter page, so a split always
/// yields two nodes that fit.
pub const MAX_INLINE_CELL: usize = 2048;
/// A non-root node smaller than this (encoded bytes) is merged with or
/// refilled from a sibling.
pub const MIN_FILL: usize = PAYLOAD_SIZE / 4;
/// Data bytes per overflow page.
pub const OVERFLOW_CAPACITY: usize = PAYLOAD_SIZE - 12;

const HEADER: usize = 12;

type Decode<T> = std::result::Result<T, &'static str>;

/// A leaf value: in the cell, or in a chain of overflow pages; compressed
/// or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    /// Stored in the cell.
    Inline(Vec<u8>),
    /// Compressed, in the cell.
    Packed {
        /// The value's length.
        raw: u32,
        /// Its LZ block.
        block: Vec<u8>,
    },
    /// Stored in overflow pages starting at `first`.
    Overflow {
        /// The first overflow page.
        first: PageId,
        /// The bytes stored in the chain.
        len: u64,
        /// Compressed: the value's length (the chain holds its block).
        raw: Option<u64>,
    },
}

/// A leaf value as a page holds it (see [`View::value`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValRef<'a> {
    /// Stored in the cell.
    Inline(&'a [u8]),
    /// Compressed, in the cell.
    Packed {
        /// The value's length.
        raw: u32,
        /// Its LZ block.
        block: &'a [u8],
    },
    /// Stored in overflow pages starting at `first`.
    Overflow {
        /// The first overflow page.
        first: PageId,
        /// The bytes stored in the chain.
        len: u64,
        /// Compressed: the value's length.
        raw: Option<u64>,
    },
}

impl Val {
    /// It, borrowed: as a page would hold it.
    pub fn as_ref(&self) -> ValRef<'_> {
        match self {
            Val::Inline(v) => ValRef::Inline(v),
            Val::Packed { raw, block } => ValRef::Packed { raw: *raw, block },
            &Val::Overflow { first, len, raw } => ValRef::Overflow { first, len, raw },
        }
    }
}

impl ValRef<'_> {
    /// The bytes it takes in a cell.
    fn stored_len(&self) -> usize {
        match self {
            ValRef::Inline(v) => v.len(),
            ValRef::Packed { block, .. } => 4 + block.len(),
            ValRef::Overflow { raw: None, .. } => 16,
            ValRef::Overflow { raw: Some(_), .. } => 24,
        }
    }
}

/// A decoded leaf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Leaf {
    /// Entries in key order.
    pub entries: Vec<(Vec<u8>, Val)>,
}

/// A decoded internal node: `children.len() == keys.len() + 1`, and child
/// `i` holds the keys in `[keys[i-1], keys[i])`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Internal {
    /// Separator keys, ascending.
    pub keys: Vec<Vec<u8>>,
    /// Child pages.
    pub children: Vec<PageId>,
}

/// A decoded node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A leaf.
    Leaf(Leaf),
    /// An internal node.
    Internal(Internal),
}

/// The encoded size of a leaf cell (offset slot included).
pub fn leaf_cell_size(key: &[u8], val: &Val) -> usize {
    cell_size(key, val.as_ref())
}

/// [`leaf_cell_size`] of a value as a page holds it.
pub fn cell_size(key: &[u8], val: ValRef) -> usize {
    2 + 7 + key.len() + val.stored_len()
}

/// Writes a leaf of `entries` (in key order, borrowed: from a page being
/// rewritten, or new) into `page` — the leaf [`Node::encode`] writes for
/// the same entries, without a decoded [`Leaf`].
pub fn encode_leaf(page: &mut Page, entries: &[(&[u8], ValRef)]) {
    page.set_page_type(LEAF);
    let p = page.payload_mut();
    // Every byte is written: the header here, the free space between the
    // offsets and the cells at the end (zeroed, as a fresh page's).
    p[..HEADER].fill(0);
    let mut end = PAYLOAD_SIZE;
    let plen = match (entries.first(), entries.last()) {
        (Some((a, _)), Some((b, _))) => key_prefix(a, b, entries.len()),
        _ => 0,
    };
    // The key prefix first, at the end; its length and offset in the
    // header.
    p[0] = 1;
    if plen > 0 {
        end -= plen;
        p[end..end + plen].copy_from_slice(&entries[0].0[..plen]);
        p[4..6].copy_from_slice(&(plen as u16).to_le_bytes());
        p[6..8].copy_from_slice(&(end as u16).to_le_bytes());
        p[0] = 3;
    }
    p[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut slot = HEADER;
    for (key, val) in entries {
        let key = &key[plen..];
        let vlen = val.stored_len();
        end -= 7 + key.len() + vlen;
        let c = &mut p[end..end + 7 + key.len() + vlen];
        c[0..2].copy_from_slice(&(key.len() as u16).to_le_bytes());
        c[2] = match val {
            ValRef::Inline(_) => 0,
            ValRef::Packed { .. } => 2,
            ValRef::Overflow { raw: None, .. } => 1,
            ValRef::Overflow { raw: Some(_), .. } => 3,
        };
        c[3..7].copy_from_slice(&(vlen as u32).to_le_bytes());
        c[7..7 + key.len()].copy_from_slice(key);
        let v = &mut c[7 + key.len()..];
        match *val {
            ValRef::Inline(b) => v.copy_from_slice(b),
            ValRef::Packed { raw, block } => {
                v[0..4].copy_from_slice(&raw.to_le_bytes());
                v[4..].copy_from_slice(block);
            }
            ValRef::Overflow { first, len, raw } => {
                v[0..8].copy_from_slice(&first.0.to_le_bytes());
                v[8..16].copy_from_slice(&len.to_le_bytes());
                if let Some(raw) = raw {
                    v[16..24].copy_from_slice(&raw.to_le_bytes());
                }
            }
        }
        p[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
        slot += 2;
    }
    debug_assert!(slot <= end, "leaf overflows its page");
    p[slot..end].fill(0);
}

/// The encoded size of an internal node's cell of `key`.
pub fn internal_cell_size(key: &[u8]) -> usize {
    2 + 10 + key.len()
}

/// The length of the key prefix a leaf with these keys (ascending) is
/// stored with: what its first and last keys share (0 for fewer than two).
pub fn key_prefix(first: &[u8], last: &[u8], n: usize) -> usize {
    if n < 2 {
        return 0;
    }
    first.iter().zip(last).take_while(|(a, b)| a == b).count()
}

/// A leaf's encoded size: `cells` the sum of its [`leaf_cell_size`]s, `n`
/// its entries, `prefix` its key prefix's length.
pub fn leaf_size(cells: usize, n: usize, prefix: usize) -> usize {
    HEADER + cells + prefix - n * prefix
}

impl Leaf {
    /// The length of its key prefix.
    pub fn prefix(&self) -> usize {
        match (self.entries.first(), self.entries.last()) {
            (Some((a, _)), Some((b, _))) => key_prefix(a, b, self.entries.len()),
            _ => 0,
        }
    }

    /// Encoded size in bytes.
    pub fn size(&self) -> usize {
        let cells = self
            .entries
            .iter()
            .map(|(k, v)| leaf_cell_size(k, v))
            .sum::<usize>();
        leaf_size(cells, self.entries.len(), self.prefix())
    }

    /// `Ok(i)` if `key` is entry `i`, `Err(i)` where it would be inserted.
    pub fn search(&self, key: &[u8]) -> Result<usize, usize> {
        self.entries
            .binary_search_by(|(k, _)| k.as_slice().cmp(key))
    }
}

impl Internal {
    /// Encoded size in bytes.
    pub fn size(&self) -> usize {
        HEADER
            + self
                .keys
                .iter()
                .map(|k| internal_cell_size(k))
                .sum::<usize>()
    }

    /// The child that holds `key`.
    pub fn child_index(&self, key: &[u8]) -> usize {
        self.keys.partition_point(|k| k.as_slice() <= key)
    }
}

impl Node {
    /// Encoded size in bytes.
    pub fn size(&self) -> usize {
        match self {
            Node::Leaf(l) => l.size(),
            Node::Internal(n) => n.size(),
        }
    }

    /// The page type this node is stored as.
    pub fn page_type(&self) -> PageType {
        match self {
            Node::Leaf(_) => LEAF,
            Node::Internal(_) => INTERNAL,
        }
    }

    /// Splits an overfull node in two by bytes: `(left, separator, right)`.
    pub fn split(self) -> (Node, Vec<u8>, Node) {
        match self {
            Node::Leaf(leaf) => {
                let sizes: Vec<usize> = leaf
                    .entries
                    .iter()
                    .map(|(k, v)| leaf_cell_size(k, v))
                    .collect();
                let k = balance_point(&sizes, 1, sizes.len() - 1);
                let mut left = leaf.entries;
                let right = left.split_off(k);
                let sep = right[0].0.clone();
                (
                    Node::Leaf(Leaf { entries: left }),
                    sep,
                    Node::Leaf(Leaf { entries: right }),
                )
            }
            Node::Internal(node) => {
                // keys[k] moves up; left keeps keys[..k], right keys[k+1..].
                let sizes: Vec<usize> = node.keys.iter().map(|k| internal_cell_size(k)).collect();
                let k = balance_point(&sizes, 1, sizes.len() - 2);
                let mut keys = node.keys;
                let mut right_keys = keys.split_off(k);
                let sep = right_keys.remove(0);
                let mut children = node.children;
                let right_children = children.split_off(k + 1);
                (
                    Node::Internal(Internal { keys, children }),
                    sep,
                    Node::Internal(Internal {
                        keys: right_keys,
                        children: right_children,
                    }),
                )
            }
        }
    }

    /// Writes this node into `page` (type included).
    pub fn encode(&self, page: &mut Page) {
        let node = match self {
            Node::Leaf(leaf) => {
                let refs: Vec<(&[u8], ValRef)> = leaf
                    .entries
                    .iter()
                    .map(|(k, v)| (k.as_slice(), v.as_ref()))
                    .collect();
                return encode_leaf(page, &refs);
            }
            Node::Internal(node) => node,
        };
        page.set_page_type(INTERNAL);
        let p = page.payload_mut();
        p.fill(0);
        let mut end = PAYLOAD_SIZE;
        let mut slot = HEADER;
        p[0] = 2;
        p[2..4].copy_from_slice(&(node.keys.len() as u16).to_le_bytes());
        write_u64(p, 4, node.children[0].0);
        for (key, child) in node.keys.iter().zip(&node.children[1..]) {
            let len = 10 + key.len();
            end -= len;
            let c = &mut p[end..end + len];
            c[0..2].copy_from_slice(&(key.len() as u16).to_le_bytes());
            c[2..10].copy_from_slice(&child.0.to_le_bytes());
            c[10..].copy_from_slice(key);
            p[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
            slot += 2;
        }
        debug_assert!(slot <= end, "node overflows its page");
    }

    /// Reads a node from `page`.
    pub fn decode(page: &Page) -> Decode<Node> {
        let p = page.payload();
        let n = u16::from_le_bytes([p[2], p[3]]) as usize;
        if HEADER + 2 * n > PAYLOAD_SIZE {
            return Err("node cell count out of range");
        }
        let cell = |i: usize| -> Decode<&[u8]> {
            let off = u16::from_le_bytes([p[HEADER + 2 * i], p[HEADER + 2 * i + 1]]) as usize;
            if off < HEADER + 2 * n || off >= PAYLOAD_SIZE {
                return Err("node cell offset out of range");
            }
            Ok(&p[off..])
        };
        match (p[0], page.page_type()) {
            (1 | 3, LEAF) => {
                let prefix: &[u8] = if p[0] == 3 {
                    let len = u16::from_le_bytes([p[4], p[5]]) as usize;
                    let off = u16::from_le_bytes([p[6], p[7]]) as usize;
                    p.get(off..off + len)
                        .ok_or("leaf key prefix out of range")?
                } else {
                    &[]
                };
                let mut entries = Vec::with_capacity(n);
                for i in 0..n {
                    let c = cell(i)?;
                    if c.len() < 7 {
                        return Err("leaf cell truncated");
                    }
                    let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
                    let flags = c[2];
                    let vlen = read_u32(c, 3) as usize;
                    if c.len() < 7 + klen + vlen {
                        return Err("leaf cell runs past the page");
                    }
                    let mut key = Vec::with_capacity(prefix.len() + klen);
                    key.extend_from_slice(prefix);
                    key.extend_from_slice(&c[7..7 + klen]);
                    let v = &c[7 + klen..7 + klen + vlen];
                    let val = match (flags, vlen) {
                        (0, _) => Val::Inline(v.to_vec()),
                        (2, 4..) => Val::Packed {
                            raw: read_u32(v, 0),
                            block: v[4..].to_vec(),
                        },
                        (1, 16) | (3, 24) => Val::Overflow {
                            first: PageId(read_u64(v, 0)),
                            len: read_u64(v, 8),
                            raw: (flags == 3).then(|| read_u64(v, 16)),
                        },
                        _ => return Err("leaf cell has unknown flags"),
                    };
                    entries.push((key, val));
                }
                // Out of order, a leaf's keys would be searched wrongly and
                // cut wrongly when written again (their shared prefix is
                // the first's and the last's): damage a checksum cannot
                // see, refused (found fuzzing, PLAN.md §M14.29 S2).
                if entries.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Err("leaf keys out of order");
                }
                Ok(Node::Leaf(Leaf { entries }))
            }
            (2, INTERNAL) => {
                let mut keys = Vec::with_capacity(n);
                let mut children = Vec::with_capacity(n + 1);
                children.push(PageId(read_u64(p, 4)));
                for i in 0..n {
                    let c = cell(i)?;
                    if c.len() < 10 {
                        return Err("internal cell truncated");
                    }
                    let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
                    if c.len() < 10 + klen {
                        return Err("internal cell runs past the page");
                    }
                    children.push(PageId(read_u64(c, 2)));
                    keys.push(c[10..10 + klen].to_vec());
                }
                if keys.windows(2).any(|w| w[0] >= w[1]) {
                    return Err("internal keys out of order");
                }
                Ok(Node::Internal(Internal { keys, children }))
            }
            _ => Err("not a B+tree node"),
        }
    }
}

/// A node read in place, without decoding it: what a search needs (a
/// separator, a child, an entry), each checked as [`Node::decode`] checks
/// it.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    p: &'a [u8],
    /// Entries (a leaf) or keys (an internal node).
    n: usize,
    leaf: bool,
    /// What every key of a leaf starts with (stored once).
    prefix: &'a [u8],
}

/// A key as a page holds it: a prefix the page's keys share, and the rest.
#[derive(Clone, Copy, Debug)]
pub struct KeyRef<'a> {
    /// The page's key prefix.
    pub prefix: &'a [u8],
    /// The rest of the key.
    pub rest: &'a [u8],
}

impl KeyRef<'_> {
    /// The key's order against `other`.
    pub fn order(&self, other: &[u8]) -> std::cmp::Ordering {
        let p = self.prefix;
        if other.len() < p.len() {
            return p[..other.len()]
                .cmp(other)
                .then(std::cmp::Ordering::Greater);
        }
        p.cmp(&other[..p.len()])
            .then_with(|| self.rest.cmp(&other[p.len()..]))
    }

    /// Whether it is `other`.
    pub fn is(&self, other: &[u8]) -> bool {
        other.len() == self.len()
            && other.starts_with(self.prefix)
            && &other[self.prefix.len()..] == self.rest
    }

    /// Its length.
    pub fn len(&self) -> usize {
        self.prefix.len() + self.rest.len()
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends it to `out`.
    pub fn append_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.prefix);
        out.extend_from_slice(self.rest);
    }

    /// It, owned.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.len());
        self.append_to(&mut v);
        v
    }
}

impl<'a> View<'a> {
    /// The node in `page`.
    pub fn new(page: &'a Page) -> Decode<Self> {
        let p = page.payload();
        let n = u16::from_le_bytes([p[2], p[3]]) as usize;
        if HEADER + 2 * n > PAYLOAD_SIZE {
            return Err("node cell count out of range");
        }
        let leaf = match (p[0], page.page_type()) {
            (1 | 3, LEAF) => true,
            (2, INTERNAL) => false,
            _ => return Err("not a B+tree node"),
        };
        let prefix: &[u8] = if p[0] == 3 {
            let len = u16::from_le_bytes([p[4], p[5]]) as usize;
            let off = u16::from_le_bytes([p[6], p[7]]) as usize;
            p.get(off..off + len)
                .ok_or("leaf key prefix out of range")?
        } else {
            &[]
        };
        Ok(View { p, n, leaf, prefix })
    }

    /// Whether it is a leaf.
    pub fn is_leaf(&self) -> bool {
        self.leaf
    }

    /// Its entries (a leaf) or separator keys (an internal node).
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn cell(&self, i: usize) -> Decode<&'a [u8]> {
        let off = u16::from_le_bytes([self.p[HEADER + 2 * i], self.p[HEADER + 2 * i + 1]]) as usize;
        if off < HEADER + 2 * self.n || off >= PAYLOAD_SIZE {
            return Err("node cell offset out of range");
        }
        Ok(&self.p[off..])
    }

    /// Entry `i`'s key (a leaf), or separator `i` (an internal node).
    pub fn key(&self, i: usize) -> Decode<KeyRef<'a>> {
        let c = self.cell(i)?;
        let head = if self.leaf { 7 } else { 10 };
        if c.len() < head {
            return Err("node cell truncated");
        }
        let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
        let rest = c
            .get(head..head + klen)
            .ok_or("node cell runs past the page")?;
        Ok(KeyRef {
            prefix: self.prefix,
            rest,
        })
    }

    /// Child `i` of an internal node.
    pub fn child(&self, i: usize) -> Decode<PageId> {
        if i == 0 {
            return Ok(PageId(read_u64(self.p, 4)));
        }
        let c = self.cell(i - 1)?;
        if c.len() < 10 {
            return Err("internal cell truncated");
        }
        Ok(PageId(read_u64(c, 2)))
    }

    /// The child of an internal node that holds `key`.
    pub fn child_index(&self, key: &[u8]) -> Decode<usize> {
        self.partition(|k| k.order(key).is_le())
    }

    /// The first `i` whose key fails `pred` (keys for which it holds come
    /// first), by binary search.
    pub fn partition(&self, pred: impl Fn(KeyRef) -> bool) -> Decode<usize> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if pred(self.key(mid)?) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Entry `i`'s value (a leaf), as the page holds it.
    pub fn value(&self, i: usize) -> Decode<ValRef<'a>> {
        let c = self.cell(i)?;
        if c.len() < 7 {
            return Err("leaf cell truncated");
        }
        let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
        let flags = c[2];
        let vlen = read_u32(c, 3) as usize;
        let v = c
            .get(7 + klen..7 + klen + vlen)
            .ok_or("leaf cell runs past the page")?;
        match (flags, vlen) {
            (0, _) => Ok(ValRef::Inline(v)),
            (2, 4..) => Ok(ValRef::Packed {
                raw: read_u32(v, 0),
                block: &v[4..],
            }),
            (1, 16) | (3, 24) => Ok(ValRef::Overflow {
                first: PageId(read_u64(v, 0)),
                len: read_u64(v, 8),
                raw: (flags == 3).then(|| read_u64(v, 16)),
            }),
            _ => Err("leaf cell has unknown flags"),
        }
    }
}

/// The split index `k` in `lo..=hi` that best balances `sizes[..k]` against
/// `sizes[k..]`.
pub(crate) fn balance_point(sizes: &[usize], lo: usize, hi: usize) -> usize {
    let total: usize = sizes.iter().sum();
    let mut left = sizes[..lo].iter().sum::<usize>();
    let mut best = (usize::MAX, lo);
    for (k, size) in sizes.iter().enumerate().take(hi + 1).skip(lo) {
        let diff = left.abs_diff(total - left);
        if diff < best.0 {
            best = (diff, k);
        }
        left += size;
    }
    best.1
}

/// Writes an overflow page payload.
pub fn encode_overflow(page: &mut Page, next: PageId, data: &[u8]) {
    page.set_page_type(OVERFLOW);
    let p = page.payload_mut();
    write_u64(p, 0, next.0);
    write_u32(p, 8, data.len() as u32);
    p[12..12 + data.len()].copy_from_slice(data);
}

/// Reads an overflow page payload: `(next, data)`.
pub fn decode_overflow(page: &Page) -> Decode<(PageId, &[u8])> {
    if page.page_type() != OVERFLOW {
        return Err("not an overflow page");
    }
    let p = page.payload();
    let len = read_u32(p, 8) as usize;
    if len > OVERFLOW_CAPACITY {
        return Err("overflow page length out of range");
    }
    Ok((PageId(read_u64(p, 0)), &p[12..12 + len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(node: &Node) {
        let mut page = Page::new(PageType::UNUSED);
        node.encode(&mut page);
        assert_eq!(&Node::decode(&page).unwrap(), node);
    }

    #[test]
    fn leaf_roundtrip() {
        roundtrip(&Node::Leaf(Leaf::default()));
        roundtrip(&Node::Leaf(Leaf {
            entries: vec![
                (b"".to_vec(), Val::Inline(vec![])),
                (b"a".to_vec(), Val::Inline(b"1".to_vec())),
                (
                    b"c".to_vec(),
                    Val::Overflow {
                        first: PageId(10),
                        len: 9_000,
                        raw: Some(40_000),
                    },
                ),
                (
                    b"p".to_vec(),
                    Val::Packed {
                        raw: 300,
                        block: vec![1, 2, 3],
                    },
                ),
                (
                    vec![0xFF; 1024],
                    Val::Overflow {
                        first: PageId(9),
                        len: 100_000,
                        raw: None,
                    },
                ),
            ],
        }));
    }

    #[test]
    fn a_leaf_written_over_another_is_byte_for_byte_a_fresh_one() {
        let leaf = |n: u32, len: usize| {
            Node::Leaf(Leaf {
                entries: (0..n)
                    .map(|i| (i.to_be_bytes().to_vec(), Val::Inline(vec![0xAB; len])))
                    .collect(),
            })
        };
        let mut dirty = Page::new(PageType::UNUSED);
        leaf(150, 40).encode(&mut dirty);
        let mut fresh = Page::new(PageType::UNUSED);
        for small in [leaf(3, 5), leaf(0, 0), leaf(1, 3000)] {
            small.encode(&mut dirty);
            small.encode(&mut fresh);
            assert!(dirty == fresh);
            assert_eq!(Node::decode(&dirty).unwrap(), small);
        }
    }

    /// Keys out of order — damage a checksum cannot see — are refused
    /// when read, not cut wrongly when written again (found fuzzing,
    /// PLAN.md §M14.29 S2).
    #[test]
    fn keys_out_of_order_are_corrupt() {
        let leaf = Node::Leaf(Leaf {
            entries: vec![
                (b"key-1".to_vec(), Val::Inline(vec![1])),
                (b"key-2".to_vec(), Val::Inline(vec![2])),
            ],
        });
        let mut page = Page::new(PageType::UNUSED);
        leaf.encode(&mut page);
        // The second key's last byte (after the shared prefix `key-`)
        // made smaller than the first's.
        let p = page.payload_mut();
        let at = p.windows(1).rposition(|w| w == b"2").unwrap();
        p[at] = b'0';
        assert_eq!(Node::decode(&page), Err("leaf keys out of order"));
        let internal = Node::Internal(Internal {
            keys: vec![b"t".to_vec(), b"m".to_vec()],
            children: vec![PageId(3), PageId(4), PageId(5)],
        });
        let mut page = Page::new(PageType::UNUSED);
        internal.encode(&mut page);
        assert_eq!(Node::decode(&page), Err("internal keys out of order"));
    }

    #[test]
    fn internal_roundtrip() {
        roundtrip(&Node::Internal(Internal {
            keys: vec![b"m".to_vec(), b"t".to_vec()],
            children: vec![PageId(3), PageId(4), PageId(5)],
        }));
    }

    #[test]
    fn split_balances_and_fits() {
        let entries: Vec<_> = (0..200u32)
            .map(|i| {
                (
                    i.to_be_bytes().to_vec(),
                    Val::Inline(vec![7; (i as usize * 37) % 100]),
                )
            })
            .collect();
        let node = Node::Leaf(Leaf { entries });
        assert!(node.size() > PAYLOAD_SIZE);
        let (l, sep, r) = node.split();
        assert!(l.size() <= PAYLOAD_SIZE && r.size() <= PAYLOAD_SIZE);
        assert!(l.size() >= MIN_FILL && r.size() >= MIN_FILL);
        let Node::Leaf(r) = r else { unreachable!() };
        assert_eq!(sep, r.entries[0].0);
    }

    #[test]
    fn prefixed_keys_compare_as_whole_keys() {
        let words: [&[u8]; 8] = [b"", b"a", b"ab", b"abc", b"abd", b"b", b"aba", b"abcd"];
        for &whole in &words {
            for cut in 0..=whole.len() {
                let k = KeyRef {
                    prefix: &whole[..cut],
                    rest: &whole[cut..],
                };
                for &other in &words {
                    assert_eq!(
                        k.order(other),
                        whole.cmp(other),
                        "{whole:?}/{cut} {other:?}"
                    );
                    assert_eq!(k.is(other), whole == other);
                }
                assert_eq!(k.to_vec(), whole);
            }
        }
    }
}
