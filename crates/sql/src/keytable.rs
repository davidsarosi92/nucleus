//! Groups' keys found by their hash (PLAN.md §M14.20 V13; DuckDB's
//! aggregate hash table): an array of slots, each a group's number and a
//! salt — the upper half of its key's hash — beside the groups' keys kept
//! apart.  A row's key is hashed and compared from the batch's vectors as
//! they are, so looking a group up builds no key; only a new group's key
//! is made.  A slot whose salt differs is passed over without comparing
//! keys.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::rc::Rc;

use nucleus_kernels::{MixHasher, MixState};

use crate::types::{Value, hash_number, unpadded};
use crate::vector::Vector;

/// The upper half of a hash: a slot's salt.
const SALT: u64 = 0xFFFF_FFFF_0000_0000;

/// A group's key: values, or a row of a batch's vectors.
#[derive(Clone, Copy)]
pub(crate) enum KeyRef<'k> {
    Values(&'k [Value]),
    Batch(&'k [Rc<Vector>], usize),
}

impl KeyRef<'_> {
    /// Its hash: the same for equal keys either way ([`Value`]'s hash, fed
    /// alike).
    pub(crate) fn hash(self, state: &MixState) -> u64 {
        let mut h = state.build_hasher();
        match self {
            KeyRef::Values(vs) => {
                for v in vs {
                    v.hash(&mut h);
                }
            }
            KeyRef::Batch(cols, i) => {
                for c in cols {
                    hash_at(c, i, &mut h);
                }
            }
        }
        h.finish()
    }

    /// Whether it equals `key` (as values compare for grouping).
    pub(crate) fn eq(self, key: &[Value]) -> bool {
        match self {
            KeyRef::Values(vs) => vs == key,
            KeyRef::Batch(cols, i) => cols.iter().zip(key).all(|(c, v)| eq_at(c, i, v)),
        }
    }

    /// Its value `j` as an integer (`None`: `NULL`, or not one).
    #[inline]
    pub(crate) fn int(self, j: usize) -> Option<i64> {
        match self {
            KeyRef::Values(vs) => match vs.get(j) {
                Some(Value::Int(x)) => Some(*x),
                _ => None,
            },
            KeyRef::Batch(cols, i) => match &*cols[j] {
                Vector::Int(d, n) => (!n.as_ref().is_some_and(|n| n[i])).then(|| d[i]),
                v => match v.value(i) {
                    Value::Int(x) => Some(x),
                    _ => None,
                },
            },
        }
    }

    /// Whether a value of it is `NULL`.
    pub(crate) fn has_null(self) -> bool {
        match self {
            KeyRef::Values(vs) => vs.iter().any(Value::is_null),
            KeyRef::Batch(cols, i) => cols.iter().any(|c| c.is_null(i)),
        }
    }

    /// The key as values.
    pub(crate) fn to_vec(self) -> Vec<Value> {
        match self {
            KeyRef::Values(vs) => vs.to_vec(),
            KeyRef::Batch(cols, i) => cols.iter().map(|c| c.value(i)).collect(),
        }
    }
}

/// Feeds row `i` of `v` to `h` as [`Value`]'s `Hash` feeds `v.value(i)`.
#[inline]
fn hash_at(v: &Vector, i: usize, h: &mut MixHasher) {
    if v.is_null(i) {
        return Value::Null.hash(h);
    }
    match v {
        Vector::Int(d, _) => hash_number(d[i] as f64, h),
        Vector::Float(d, _) => hash_number(d[i], h),
        Vector::Bool(d, _) => d[i].hash(h),
        // As a `str` hashes: its bytes, then 0xff.
        Vector::Text(o, b, _) => {
            h.write(&b[o[i] as usize..o[i + 1] as usize]);
            h.write_u8(0xff);
        }
        Vector::Char(o, b, _) => {
            h.write(trimmed(&b[o[i] as usize..o[i + 1] as usize]));
            h.write_u8(0xff);
        }
        _ => v.value(i).hash(h),
    }
}

/// Whether row `i` of `v` equals `key` (as [`Value`]'s `==`).
#[inline]
fn eq_at(v: &Vector, i: usize, key: &Value) -> bool {
    if v.is_null(i) {
        return key.is_null();
    }
    match (v, key) {
        (_, Value::Null) => false,
        (Vector::Int(d, _), Value::Int(x)) => d[i] == *x,
        (Vector::Bool(d, _), Value::Bool(x)) => d[i] == *x,
        (Vector::Text(o, b, _), Value::Text(s)) => {
            &b[o[i] as usize..o[i + 1] as usize] == s.as_bytes()
        }
        (Vector::Char(o, b, _), Value::Char(s)) => {
            trimmed(&b[o[i] as usize..o[i + 1] as usize]) == unpadded(s).as_bytes()
        }
        _ => v.value(i) == *key,
    }
}

/// `char(n)` bytes without their trailing blanks.
fn trimmed(b: &[u8]) -> &[u8] {
    let n = b.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
    &b[..n]
}

/// The slots of groups `0..n` by their keys' hashes.
pub(crate) struct KeyTable {
    /// 0: empty; else the salt, and the group's number plus one.
    slots: Vec<u64>,
    /// Each group's hash (for growing without hashing keys again).
    hashes: Vec<u64>,
    pub(crate) state: MixState,
}

impl Default for KeyTable {
    fn default() -> Self {
        KeyTable {
            slots: vec![0; 16],
            hashes: Vec::new(),
            state: MixState::default(),
        }
    }
}

impl KeyTable {
    /// The group whose key has hash `h` and is `eq`.
    #[inline]
    pub(crate) fn find(&self, h: u64, eq: impl Fn(u32) -> bool) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let salt = h & SALT;
        let mut s = (h as usize) & mask;
        loop {
            let e = self.slots[s];
            if e == 0 {
                return None;
            }
            if e & SALT == salt {
                let g = (e as u32).wrapping_sub(1);
                if eq(g) {
                    return Some(g);
                }
            }
            s = (s + 1) & mask;
        }
    }

    /// Adds the next group (`0, 1, …`), its key's hash `h`.
    pub(crate) fn insert(&mut self, h: u64) {
        let g = self.hashes.len() as u32;
        self.hashes.push(h);
        if self.hashes.len() * 2 > self.slots.len() {
            // At most half full: grown, every group placed again.
            self.slots = vec![0; self.slots.len() * 2];
            for (g, &h) in self.hashes.iter().enumerate() {
                place(&mut self.slots, h, g as u32);
            }
        } else {
            place(&mut self.slots, h, g);
        }
    }
}

fn place(slots: &mut [u64], h: u64, g: u32) {
    let mask = slots.len() - 1;
    let mut s = (h as usize) & mask;
    while slots[s] != 0 {
        s = (s + 1) & mask;
    }
    slots[s] = (h & SALT) | (u64::from(g) + 1);
}

/// Integer keys of up to this wide a range are a direct index.
const DIRECT: i128 = 1 << 16;

/// Single integer keys' groups: by direct index while the keys lie in a
/// range of up to [`DIRECT`] values (DuckDB's perfect hash aggregate, the
/// range found as the keys come), by hash beyond.
pub(crate) enum IntKeys {
    /// Key `base + i`'s group plus one at `i` (0: none).
    Direct {
        base: i64,
        slots: Vec<u32>,
    },
    Hashed(HashMap<i64, u32, MixState>),
}

impl Default for IntKeys {
    fn default() -> Self {
        IntKeys::Direct {
            base: 0,
            slots: Vec::new(),
        }
    }
}

impl IntKeys {
    /// Key `x`'s group.
    #[inline]
    pub(crate) fn get(&self, x: i64) -> Option<u32> {
        match self {
            IntKeys::Direct { base, slots } => {
                let at = x.wrapping_sub(*base) as u64;
                match slots.get(at as usize) {
                    Some(&g) if at < slots.len() as u64 && g != 0 => Some(g - 1),
                    _ => None,
                }
            }
            IntKeys::Hashed(m) => m.get(&x).copied(),
        }
    }

    /// Key `x` (not there yet) is group `g`'s.
    pub(crate) fn insert(&mut self, x: i64, g: u32) {
        if let IntKeys::Direct { base, slots } = self {
            let (lo, hi) = if slots.is_empty() {
                (i128::from(x), i128::from(x))
            } else {
                let (b, top) = (
                    i128::from(*base),
                    i128::from(*base) + slots.len() as i128 - 1,
                );
                (b.min(i128::from(x)), top.max(i128::from(x)))
            };
            let needed = hi - lo + 1;
            if needed <= DIRECT {
                let len = slots.len() as i128;
                if needed > len {
                    // Grown at least twofold (within the limit), toward `x`.
                    let span = needed.max(len * 2).clamp(16, DIRECT);
                    let new_base = if i128::from(x) < i128::from(*base) && len > 0 {
                        i128::from(*base) + len - span
                    } else {
                        lo
                    };
                    let mut grown = vec![0u32; span as usize];
                    let shift = (i128::from(*base) - new_base) as usize;
                    if len > 0 {
                        grown[shift..shift + slots.len()].copy_from_slice(slots);
                    }
                    *slots = grown;
                    *base = new_base as i64;
                }
                slots[(i128::from(x) - i128::from(*base)) as usize] = g + 1;
                return;
            }
            // Too wide a range: hashed from here on.
            let m: HashMap<i64, u32, MixState> = slots
                .iter()
                .enumerate()
                .filter(|(_, g)| **g != 0)
                .map(|(i, g)| (*base + i as i64, g - 1))
                .collect();
            *self = IntKeys::Hashed(m);
        }
        if let IntKeys::Hashed(m) = self {
            m.insert(x, g);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_row_hashes_and_compares_as_its_values() {
        let cols: Vec<Rc<Vector>> = vec![
            Rc::new(Vector::Int(vec![1, 2, 3], Some(vec![false, true, false]))),
            Rc::new(Vector::Float(vec![-0.0, 0.5, f64::NAN], None)),
            Rc::new(Vector::Text(vec![0, 1, 1, 4], b"aabc".to_vec(), None)),
            Rc::new(Vector::Char(vec![0, 3, 4, 7], b"a  bcc ".to_vec(), None)),
            Rc::new(Vector::Bool(vec![true, false, true], None)),
            Rc::new(Vector::Const(Value::Numeric(
                crate::decimal::Decimal::from_i64(7),
            ))),
        ];
        let state = MixState::default();
        for i in 0..3 {
            let values: Vec<Value> = cols.iter().map(|c| c.value(i)).collect();
            let row = KeyRef::Batch(&cols, i);
            assert_eq!(
                row.hash(&state),
                KeyRef::Values(&values).hash(&state),
                "row {i}"
            );
            assert!(row.eq(&values), "row {i}");
            assert_eq!(row.to_vec(), values);
        }
        // Equal values of other spellings: 0 and -0, a char's blanks.
        let other = vec![
            Value::Int(1),
            Value::Float(0.0),
            Value::Text("a".into()),
            Value::Char("a".into()),
            Value::Bool(true),
            Value::Int(7),
        ];
        assert!(KeyRef::Batch(&cols, 0).eq(&other));
        assert_eq!(
            KeyRef::Batch(&cols, 0).hash(&state),
            KeyRef::Values(&other).hash(&state)
        );
        assert!(!KeyRef::Batch(&cols, 1).eq(&other));
    }

    #[test]
    fn integer_keys_by_index_then_by_hash() {
        // Keys coming down, up, apart: a direct index while the range is
        // narrow, a hash past it, every group found either way.
        let mut keys = IntKeys::default();
        let mut want = Vec::new();
        let xs: Vec<i64> = (0..300)
            .map(|i| 1000 - i * 3)
            .chain((0..300).map(|i| 2000 + i * 7))
            .chain([i64::MIN, i64::MAX, -1, 0])
            .chain((0..100).map(|i| 50_000 + i))
            .collect();
        for (g, &x) in xs.iter().enumerate() {
            assert_eq!(keys.get(x), None, "{x}");
            keys.insert(x, g as u32);
            want.push((x, g as u32));
            if x == 2000 + 299 * 7 {
                assert!(matches!(keys, IntKeys::Direct { .. }), "still direct");
            }
            if g % 97 == 0 {
                for &(x, g) in &want {
                    assert_eq!(keys.get(x), Some(g), "{x}");
                }
            }
        }
        assert!(matches!(keys, IntKeys::Hashed(_)));
        for &(x, g) in &want {
            assert_eq!(keys.get(x), Some(g), "{x}");
        }
        assert_eq!(keys.get(1), None);
    }

    #[test]
    fn groups_are_found_after_growing() {
        let state = MixState::default();
        let mut t = KeyTable::default();
        let keys: Vec<Vec<Value>> = (0..5000).map(|i| vec![Value::Int(i % 2500 * 7)]).collect();
        let mut groups: Vec<Vec<Value>> = Vec::new();
        for k in &keys {
            let h = KeyRef::Values(k).hash(&state);
            if t.find(h, |g| groups[g as usize] == *k).is_none() {
                t.insert(h);
                groups.push(k.clone());
            }
        }
        assert_eq!(groups.len(), 2500);
        for (g, k) in groups.iter().enumerate() {
            let h = KeyRef::Values(k).hash(&state);
            assert_eq!(t.find(h, |x| groups[x as usize] == *k), Some(g as u32));
        }
    }
}
