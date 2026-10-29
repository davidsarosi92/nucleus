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
//! Nodes are decoded into [`Leaf`] / [`Internal`] to be read or changed and
//! encoded back.  Simple and hard to get wrong; the cost is a copy per node
//! visited.

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
    fn stored_len(&self) -> usize {
        match self {
            Val::Inline(v) => v.len(),
            Val::Packed { block, .. } => 4 + block.len(),
            Val::Overflow { raw: None, .. } => 16,
            Val::Overflow { raw: Some(_), .. } => 24,
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
    2 + 7 + key.len() + val.stored_len()
}

fn internal_cell_size(key: &[u8]) -> usize {
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
        page.set_page_type(self.page_type());
        let p = page.payload_mut();
        p.fill(0);
        let mut end = PAYLOAD_SIZE;
        let mut slot = HEADER;
        // A leaf's key prefix: first, at the end, its length and offset in
        // the header.
        if let Node::Leaf(leaf) = self {
            let plen = leaf.prefix();
            if plen > 0 {
                end -= plen;
                p[end..end + plen].copy_from_slice(&leaf.entries[0].0[..plen]);
                p[4..6].copy_from_slice(&(plen as u16).to_le_bytes());
                p[6..8].copy_from_slice(&(end as u16).to_le_bytes());
            }
        }
        let mut put_cell = |p: &mut [u8], cell: &[u8]| {
            end -= cell.len();
            p[end..end + cell.len()].copy_from_slice(cell);
            p[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
            slot += 2;
        };
        let mut cell = Vec::with_capacity(MAX_INLINE_CELL + 32);
        match self {
            Node::Leaf(leaf) => {
                let plen = leaf.prefix();
                p[0] = 1;
                p[2..4].copy_from_slice(&(leaf.entries.len() as u16).to_le_bytes());
                if plen > 0 {
                    p[0] = 3;
                }
                for (key, val) in &leaf.entries {
                    let key = &key[plen..];
                    cell.clear();
                    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
                    match val {
                        Val::Inline(v) => {
                            cell.push(0);
                            cell.extend_from_slice(&(v.len() as u32).to_le_bytes());
                            cell.extend_from_slice(key);
                            cell.extend_from_slice(v);
                        }
                        Val::Packed { raw, block } => {
                            cell.push(2);
                            cell.extend_from_slice(&(4 + block.len() as u32).to_le_bytes());
                            cell.extend_from_slice(key);
                            cell.extend_from_slice(&raw.to_le_bytes());
                            cell.extend_from_slice(block);
                        }
                        Val::Overflow { first, len, raw } => {
                            cell.push(if raw.is_some() { 3 } else { 1 });
                            let vlen: u32 = if raw.is_some() { 24 } else { 16 };
                            cell.extend_from_slice(&vlen.to_le_bytes());
                            cell.extend_from_slice(key);
                            cell.extend_from_slice(&first.0.to_le_bytes());
                            cell.extend_from_slice(&len.to_le_bytes());
                            if let Some(raw) = raw {
                                cell.extend_from_slice(&raw.to_le_bytes());
                            }
                        }
                    }
                    put_cell(p, &cell);
                }
            }
            Node::Internal(node) => {
                p[0] = 2;
                p[2..4].copy_from_slice(&(node.keys.len() as u16).to_le_bytes());
                write_u64(p, 4, node.children[0].0);
                for (key, child) in node.keys.iter().zip(&node.children[1..]) {
                    cell.clear();
                    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
                    cell.extend_from_slice(&child.0.to_le_bytes());
                    cell.extend_from_slice(key);
                    put_cell(p, &cell);
                }
            }
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
fn balance_point(sizes: &[usize], lo: usize, hi: usize) -> usize {
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
                (b"a".to_vec(), Val::Inline(b"1".to_vec())),
                (b"".to_vec(), Val::Inline(vec![])),
                (
                    vec![0xFF; 1024],
                    Val::Overflow {
                        first: PageId(9),
                        len: 100_000,
                        raw: None,
                    },
                ),
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
            ],
        }));
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
