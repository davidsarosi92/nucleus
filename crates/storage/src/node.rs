//! B+tree node format.
//!
//! A node is one page.  Payload layout (little-endian):
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0 | 1 | kind: 1 leaf, 2 internal |
//! | 1 | 1 | reserved |
//! | 2 | 2 | cell count `n` |
//! | 4 | 8 | internal: the leftmost child; leaf: 0 |
//! | 12 | 2 × n | cell offsets (from the payload start), in key order |
//! | … | | free space, then the cells, packed at the end |
//!
//! Leaf cell: `klen u16, flags u8, vlen u32, key, value` — with flag 1
//! (overflow) the value is `first page u64, total length u64` and the bytes
//! live in a chain of overflow pages.  Internal cell: `klen u16, child u64,
//! key`; the child holds keys `≥ key` (and `<` the next cell's key).
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

/// A leaf value: in the cell, or in a chain of overflow pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    /// Stored in the cell.
    Inline(Vec<u8>),
    /// Stored in overflow pages starting at `first`.
    Overflow {
        /// The first overflow page.
        first: PageId,
        /// The value's length in bytes.
        len: u64,
    },
}

impl Val {
    fn stored_len(&self) -> usize {
        match self {
            Val::Inline(v) => v.len(),
            Val::Overflow { .. } => 16,
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

impl Leaf {
    /// Encoded size in bytes.
    pub fn size(&self) -> usize {
        HEADER
            + self
                .entries
                .iter()
                .map(|(k, v)| leaf_cell_size(k, v))
                .sum::<usize>()
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
        let mut put_cell = |p: &mut [u8], cell: &[u8]| {
            end -= cell.len();
            p[end..end + cell.len()].copy_from_slice(cell);
            p[slot..slot + 2].copy_from_slice(&(end as u16).to_le_bytes());
            slot += 2;
        };
        let mut cell = Vec::with_capacity(MAX_INLINE_CELL + 32);
        match self {
            Node::Leaf(leaf) => {
                p[0] = 1;
                p[2..4].copy_from_slice(&(leaf.entries.len() as u16).to_le_bytes());
                for (key, val) in &leaf.entries {
                    cell.clear();
                    cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
                    match val {
                        Val::Inline(v) => {
                            cell.push(0);
                            cell.extend_from_slice(&(v.len() as u32).to_le_bytes());
                            cell.extend_from_slice(key);
                            cell.extend_from_slice(v);
                        }
                        Val::Overflow { first, len } => {
                            cell.push(1);
                            cell.extend_from_slice(&16u32.to_le_bytes());
                            cell.extend_from_slice(key);
                            cell.extend_from_slice(&first.0.to_le_bytes());
                            cell.extend_from_slice(&len.to_le_bytes());
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
            (1, LEAF) => {
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
                    let key = c[7..7 + klen].to_vec();
                    let v = &c[7 + klen..7 + klen + vlen];
                    let val = match (flags, vlen) {
                        (0, _) => Val::Inline(v.to_vec()),
                        (1, 16) => Val::Overflow {
                            first: PageId(read_u64(v, 0)),
                            len: read_u64(v, 8),
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
            (1, LEAF) => true,
            (2, INTERNAL) => false,
            _ => return Err("not a B+tree node"),
        };
        Ok(View { p, n, leaf })
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
    pub fn key(&self, i: usize) -> Decode<&'a [u8]> {
        let c = self.cell(i)?;
        let head = if self.leaf { 7 } else { 10 };
        if c.len() < head {
            return Err("node cell truncated");
        }
        let klen = u16::from_le_bytes([c[0], c[1]]) as usize;
        c.get(head..head + klen)
            .ok_or("node cell runs past the page")
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
        self.partition(|k| k <= key)
    }

    /// The first `i` whose key fails `pred` (keys for which it holds come
    /// first), by binary search.
    pub fn partition(&self, pred: impl Fn(&[u8]) -> bool) -> Decode<usize> {
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

    /// Entry `i`'s value (a leaf).
    pub fn value(&self, i: usize) -> Decode<Val> {
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
            (0, _) => Ok(Val::Inline(v.to_vec())),
            (1, 16) => Ok(Val::Overflow {
                first: PageId(read_u64(v, 0)),
                len: read_u64(v, 8),
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
}
