//! The binary form of a JSON value (PLAN.md §M14.15): canonical — one
//! value, one sequence of bytes — and read in place, without a parse.
//!
//! Every node starts with a header byte, its kind in the high four bits:
//!
//! - `null`, `false`, `true`: the header alone.
//! - an integer that fits 64 bits: the low four bits are a byte count
//!   (0–8, the fewest that hold it), then those bytes, little-endian, two's
//!   complement (sign-extended when read).  Zero is the header alone.
//! - any other number: the low bit says negative; then the exponent
//!   (zig-zag LEB128) and the count of digits (LEB128), then the digits
//!   two to a byte, high nibble first.  The value is `digits × 10^exp`,
//!   its digits without leading or trailing zeros.
//! - a string: the low four bits its length if under 15, else 15 and the
//!   length (LEB128); then its UTF-8 bytes.
//! - an array: the low bits the width of its offsets (0: one byte, 1:
//!   two, 2: four); its count (LEB128); the end of each element, relative
//!   to where they start; then the elements.  `a[i]` is found at once.
//! - an object: the same width and count; the end of each key, then the
//!   end of each value; the keys (UTF-8, sorted shorter first, then by
//!   their bytes — `jsonb`'s order —, each once, the last of duplicates
//!   kept), then the values in the keys' order.  A
//!   key is found by binary search, its value without reading the others.
//!
//! The widths are the smallest that hold a container's offsets, so equal
//! values have equal bytes — a comparison, a hash, a `DISTINCT`, a unique
//! index cost no more than for `bytea`.  Every node says how long it is, so
//! a value inside another is its bytes: `doc -> 'a'` is a slice copied.

use crate::error::{Error, Result};

pub(crate) const NULL: u8 = 0;
pub(crate) const FALSE: u8 = 1;
pub(crate) const TRUE: u8 = 2;
pub(crate) const INT: u8 = 3;
pub(crate) const DEC: u8 = 4;
pub(crate) const STR: u8 = 5;
pub(crate) const ARR: u8 = 6;
pub(crate) const OBJ: u8 = 7;

/// The most digits before a number's point, and after it — `numeric`'s
/// bounds in PostgreSQL — so a short text cannot ask for a huge one
/// (`1e999999999`).
pub const MAX_INT_DIGITS: i64 = 131_072;
/// See [`MAX_INT_DIGITS`].
pub const MAX_FRACTION_DIGITS: i64 = 16_383;

pub(crate) fn malformed() -> Error {
    Error::internal("malformed json value")
}

// ------------------------------------------------------------- reading

/// A node, read in place.
#[derive(Clone, Copy, Debug)]
pub enum Node<'a> {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer that fits 64 bits.
    Int(i64),
    /// Any other number.
    Dec(Dec<'a>),
    /// A string: its UTF-8 bytes (checked when the value was made; read
    /// them as `str` with [`text_of`], which checks again).
    Str(&'a [u8]),
    /// An array.
    Array(Array<'a>),
    /// An object.
    Object(Object<'a>),
}

/// A number that is not a 64-bit integer: `digits × 10^exp`.
#[derive(Clone, Copy, Debug)]
pub struct Dec<'a> {
    /// Negative.
    pub negative: bool,
    /// The power of ten of the last digit.
    pub exp: i64,
    /// How many digits.
    pub len: usize,
    packed: &'a [u8],
}

impl Dec<'_> {
    /// Digit `i` (0–9), from the first.
    pub fn digit(&self, i: usize) -> u8 {
        let b = self.packed[i / 2];
        if i.is_multiple_of(2) {
            b >> 4
        } else {
            b & 0x0f
        }
    }

    /// The digits, 0–9 each.
    pub fn digits(&self) -> Vec<u8> {
        (0..self.len).map(|i| self.digit(i)).collect()
    }

    /// Appends digits `from..to` as ASCII, two a byte where they align.
    pub fn write_digits(&self, mut from: usize, to: usize, out: &mut Vec<u8>) {
        out.reserve(to - from);
        if from % 2 == 1 && from < to {
            out.push(b'0' + self.digit(from));
            from += 1;
        }
        // Through a buffer on the stack, sixteen bytes at a time.
        let pairs = (to - from) / 2;
        for chunk in self.packed[from / 2..from / 2 + pairs].chunks(16) {
            let mut buf = [0u8; 32];
            for (o, b) in buf.as_chunks_mut::<2>().0.iter_mut().zip(chunk) {
                o[0] = b'0' + (b >> 4);
                o[1] = b'0' + (b & 0x0f);
            }
            out.extend_from_slice(&buf[..2 * chunk.len()]);
        }
        from += 2 * pairs;
        if from < to {
            out.push(b'0' + self.digit(from));
        }
    }
}

/// An array, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Array<'a> {
    len: usize,
    width: usize,
    ends: &'a [u8],
    data: &'a [u8],
}

/// An object, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Object<'a> {
    len: usize,
    width: usize,
    key_ends: &'a [u8],
    value_ends: &'a [u8],
    keys: &'a [u8],
    values: &'a [u8],
}

fn width_of(code: u8) -> Result<usize> {
    match code {
        0 => Ok(1),
        1 => Ok(2),
        2 => Ok(4),
        _ => Err(malformed()),
    }
}

#[inline]
fn read_end(table: &[u8], width: usize, i: usize) -> usize {
    let b = &table[i * width..(i + 1) * width];
    match width {
        1 => usize::from(b[0]),
        2 => usize::from(u16::from_le_bytes([b[0], b[1]])),
        _ => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize,
    }
}

/// Reads a LEB128 number at `*at`.
#[inline]
pub(crate) fn read_varint(b: &[u8], at: &mut usize) -> Result<u64> {
    // Mostly one byte.
    if let Some(&x) = b.get(*at)
        && x < 0x80
    {
        *at += 1;
        return Ok(u64::from(x));
    }
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let x = *b.get(*at).ok_or_else(malformed)?;
        *at += 1;
        if shift == 63 && x > 1 {
            return Err(malformed());
        }
        v |= u64::from(x & 0x7f) << shift;
        if x & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(malformed());
        }
    }
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(i: i64) -> u64 {
    ((i << 1) ^ (i >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

#[inline]
fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(n).ok_or_else(malformed)?;
    let s = b.get(*at..end).ok_or_else(malformed)?;
    *at = end;
    Ok(s)
}

fn count(b: &[u8], at: &mut usize) -> Result<usize> {
    let n = read_varint(b, at)?;
    // Each element takes a byte at least: a count past what is left is
    // malformed (and must not size an allocation).
    usize::try_from(n)
        .ok()
        .filter(|n| *n <= b.len())
        .ok_or_else(malformed)
}

/// The node at the start of `b` and how many bytes it takes.
pub fn node(b: &[u8]) -> Result<(Node<'_>, usize)> {
    let h = *b.first().ok_or_else(malformed)?;
    let (kind, low) = (h >> 4, h & 0x0f);
    let mut at = 1;
    let n = match kind {
        NULL | FALSE | TRUE if low == 0 => match kind {
            NULL => Node::Null,
            FALSE => Node::Bool(false),
            _ => Node::Bool(true),
        },
        INT if low <= 8 => {
            let bytes = take(b, &mut at, usize::from(low))?;
            let mut v: i64 = 0;
            for (i, x) in bytes.iter().enumerate() {
                v |= i64::from(*x) << (8 * i);
            }
            if (1..8).contains(&low) {
                let shift = 64 - 8 * u32::from(low);
                v = (v << shift) >> shift;
            }
            Node::Int(v)
        }
        DEC if low <= 1 => {
            let exp = unzigzag(read_varint(b, &mut at)?);
            // Two digits a byte: the bytes left bound half the count.
            let len = usize::try_from(read_varint(b, &mut at)?)
                .ok()
                .filter(|n| n / 2 <= b.len())
                .ok_or_else(malformed)?;
            let packed = take(b, &mut at, len.div_ceil(2))?;
            // Within the bounds a parsed number keeps, so no reader is
            // asked for more (a corrupted exponent would make a text of
            // billions of zeros).
            let before = (len as i64).checked_add(exp).ok_or_else(malformed)?;
            if len == 0 || before > MAX_INT_DIGITS || exp < -MAX_FRACTION_DIGITS {
                return Err(malformed());
            }
            Node::Dec(Dec {
                negative: low == 1,
                exp,
                len,
                packed,
            })
        }
        STR => {
            let len = if low < 15 {
                usize::from(low)
            } else {
                count(b, &mut at)?
            };
            let s = take(b, &mut at, len)?;
            Node::Str(s)
        }
        ARR => {
            let width = width_of(low)?;
            let len = count(b, &mut at)?;
            let ends = take(b, &mut at, len.checked_mul(width).ok_or_else(malformed)?)?;
            let size = if len == 0 {
                0
            } else {
                read_end(ends, width, len - 1)
            };
            let data = take(b, &mut at, size)?;
            Node::Array(Array {
                len,
                width,
                ends,
                data,
            })
        }
        OBJ => {
            let width = width_of(low)?;
            let len = count(b, &mut at)?;
            let table = len.checked_mul(width).ok_or_else(malformed)?;
            let key_ends = take(b, &mut at, table)?;
            let value_ends = take(b, &mut at, table)?;
            let (ks, vs) = if len == 0 {
                (0, 0)
            } else {
                (
                    read_end(key_ends, width, len - 1),
                    read_end(value_ends, width, len - 1),
                )
            };
            let keys = take(b, &mut at, ks)?;
            let values = take(b, &mut at, vs)?;
            Node::Object(Object {
                len,
                width,
                key_ends,
                value_ends,
                keys,
                values,
            })
        }
        _ => return Err(malformed()),
    };
    Ok((n, at))
}

/// A string's or a key's bytes as `str`.
pub fn text_of(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|_| malformed())
}

/// The node `b` holds (its bytes exactly).
pub fn root(b: &[u8]) -> Result<Node<'_>> {
    let (n, len) = node(b)?;
    if len != b.len() {
        return Err(malformed());
    }
    Ok(n)
}

#[inline]
fn span(ends: &[u8], width: usize, i: usize) -> (usize, usize) {
    let start = if i == 0 {
        0
    } else {
        read_end(ends, width, i - 1)
    };
    (start, read_end(ends, width, i))
}

impl<'a> Array<'a> {
    /// How many elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Element `i`'s bytes.
    pub fn bytes(&self, i: usize) -> Result<&'a [u8]> {
        let (s, e) = span(self.ends, self.width, i);
        self.data.get(s..e).ok_or_else(malformed)
    }

    /// Element `i`.
    pub fn get(&self, i: usize) -> Result<Node<'a>> {
        root(self.bytes(i)?)
    }
}

impl<'a> Object<'a> {
    /// How many keys.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Key `i` (in the keys' order).
    pub fn key(&self, i: usize) -> Result<&'a str> {
        text_of(self.key_bytes(i)?)
    }

    /// Key `i`'s bytes (UTF-8, unchecked).
    pub fn key_bytes(&self, i: usize) -> Result<&'a [u8]> {
        let (s, e) = span(self.key_ends, self.width, i);
        self.keys.get(s..e).ok_or_else(malformed)
    }

    /// Value `i`'s bytes.
    pub fn value_bytes(&self, i: usize) -> Result<&'a [u8]> {
        let (s, e) = span(self.value_ends, self.width, i);
        self.values.get(s..e).ok_or_else(malformed)
    }

    /// Value `i`.
    pub fn value(&self, i: usize) -> Result<Node<'a>> {
        root(self.value_bytes(i)?)
    }

    /// The position of `key`, by binary search.
    pub fn find(&self, key: &str) -> Result<Option<usize>> {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (s, e) = span(self.key_ends, self.width, mid);
            let k = self.keys.get(s..e).ok_or_else(malformed)?;
            match key_order(k, key.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(mid)),
            }
        }
        Ok(None)
    }

    /// The bytes of `key`'s value.
    pub fn get_bytes(&self, key: &str) -> Result<Option<&'a [u8]>> {
        self.find(key)?.map(|i| self.value_bytes(i)).transpose()
    }
}

// ------------------------------------------------------------- writing

pub(crate) fn put_null(out: &mut Vec<u8>) {
    out.push(NULL << 4);
}

pub(crate) fn put_bool(out: &mut Vec<u8>, b: bool) {
    out.push(if b { TRUE << 4 } else { FALSE << 4 });
}

pub(crate) fn put_int(out: &mut Vec<u8>, v: i64) {
    let bytes = v.to_le_bytes();
    // The fewest bytes that sign-extend back to `v` (none for zero).
    let n = if v == 0 {
        0
    } else {
        (1..8)
            .find(|n| {
                let shift = 64 - 8 * n;
                (v << shift) >> shift == v
            })
            .unwrap_or(8)
    };
    out.push((INT << 4) | n as u8);
    out.extend_from_slice(&bytes[..n]);
}

/// A number from its sign, its digits (0–9 each) and the power of ten of
/// the last one, made canonical: zeros trimmed, a 64-bit integer as one.
pub(crate) fn put_number(out: &mut Vec<u8>, negative: bool, digits: &[u8], exp: i64) -> Result<()> {
    let first = digits.iter().position(|d| *d != 0);
    let Some(first) = first else {
        put_int(out, 0);
        return Ok(());
    };
    let last = digits
        .iter()
        .rposition(|d| *d != 0)
        .expect("a non-zero digit");
    let d = &digits[first..=last];
    let exp = exp
        .checked_add((digits.len() - 1 - last) as i64)
        .ok_or_else(overflow)?;
    // Within numeric's bounds: digits before the point, and after.
    let before = (d.len() as i64).checked_add(exp).ok_or_else(overflow)?;
    if before > MAX_INT_DIGITS || exp < -MAX_FRACTION_DIGITS {
        return Err(overflow());
    }
    if exp >= 0 && before <= 19 {
        let mut m: i128 = 0;
        for x in d {
            m = m * 10 + i128::from(*x);
        }
        m *= 10i128.pow(exp as u32);
        let m = if negative { -m } else { m };
        if let Ok(i) = i64::try_from(m) {
            put_int(out, i);
            return Ok(());
        }
    }
    out.push((DEC << 4) | u8::from(negative));
    put_varint(out, zigzag(exp));
    put_varint(out, d.len() as u64);
    for pair in d.chunks(2) {
        out.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
    }
    Ok(())
}

/// A number from its text's digits — `int` before the point, `frac`
/// after it, ASCII — and the exponent written after them, made canonical
/// as [`put_number`] makes one, packed straight from the text.
pub(crate) fn put_ascii_number(
    out: &mut Vec<u8>,
    negative: bool,
    int: &[u8],
    frac: &[u8],
    exp: i64,
) -> Result<()> {
    // The significant digits: a run of `int` then of `frac`, without
    // leading or trailing zeros, and the power of ten of the last.
    let tf = frac.iter().rev().take_while(|d| **d == b'0').count();
    let frac = &frac[..frac.len() - tf];
    let mut exp = exp.checked_sub(frac.len() as i64).ok_or_else(overflow)?;
    let int = if frac.is_empty() {
        let ti = int.iter().rev().take_while(|d| **d == b'0').count();
        exp = exp.checked_add(ti as i64).ok_or_else(overflow)?;
        &int[..int.len() - ti]
    } else {
        int
    };
    let lead = int.iter().take_while(|d| **d == b'0').count();
    let (int, frac) = if lead == int.len() {
        let f = frac.iter().take_while(|d| **d == b'0').count();
        (&int[..0], &frac[f..])
    } else {
        (&int[lead..], frac)
    };
    let len = int.len() + frac.len();
    if len == 0 {
        put_int(out, 0);
        return Ok(());
    }
    let before = (len as i64).checked_add(exp).ok_or_else(overflow)?;
    if before > MAX_INT_DIGITS || exp < -MAX_FRACTION_DIGITS {
        return Err(overflow());
    }
    let digits = int.iter().chain(frac).map(|d| d - b'0');
    if exp >= 0 && before <= 18 {
        let mut m: i64 = 0;
        for d in digits {
            m = m * 10 + i64::from(d);
        }
        m *= 10i64.pow(exp as u32);
        put_int(out, if negative { -m } else { m });
        return Ok(());
    }
    if exp >= 0 && before == 19 {
        // Perhaps a 64-bit integer still: the general path decides.
        let d: Vec<u8> = digits.collect();
        return put_number(out, negative, &d, exp);
    }
    out.push((DEC << 4) | u8::from(negative));
    put_varint(out, zigzag(exp));
    put_varint(out, len as u64);
    // Two digits a byte, the `int` run then the `frac` one: a digit
    // left over from the first pairs with the second's first.
    out.reserve(len.div_ceil(2));
    let pack = |out: &mut Vec<u8>, run: &[u8]| {
        let (pairs, rest) = run.as_chunks::<2>();
        out.extend(pairs.iter().map(|[a, b]| ((a - b'0') << 4) | (b - b'0')));
        rest.first().copied()
    };
    let rest = match pack(out, int) {
        None => pack(out, frac),
        Some(h) => match frac.split_first() {
            Some((f, more)) => {
                out.push(((h - b'0') << 4) | (f - b'0'));
                pack(out, more)
            }
            None => Some(h),
        },
    };
    if let Some(h) = rest {
        out.push((h - b'0') << 4);
    }
    Ok(())
}

pub(crate) fn overflow() -> Error {
    Error::out_of_range("value overflows numeric format")
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    if s.len() < 15 {
        out.push((STR << 4) | s.len() as u8);
    } else {
        out.push((STR << 4) | 15);
        put_varint(out, s.len() as u64);
    }
    out.extend_from_slice(s.as_bytes());
}

fn width_for(max: usize) -> (u8, usize) {
    if max <= 0xff {
        (0, 1)
    } else if max <= 0xffff {
        (1, 2)
    } else {
        (2, 4)
    }
}

fn put_end(out: &mut Vec<u8>, end: usize, width: usize) {
    out.extend_from_slice(&(end as u32).to_le_bytes()[..width]);
}

/// An array of the elements in `data`, element `i` ending at `ends[i]`.
pub(crate) fn put_array(out: &mut Vec<u8>, ends: &[usize], data: &[u8]) -> Result<()> {
    let total = ends.last().copied().unwrap_or(0);
    if u32::try_from(total).is_err() {
        return Err(too_large());
    }
    let (code, width) = width_for(total);
    out.push((ARR << 4) | code);
    put_varint(out, ends.len() as u64);
    out.reserve(ends.len() * width + data.len());
    for e in ends {
        put_end(out, *e, width);
    }
    out.extend_from_slice(data);
    Ok(())
}

pub(crate) fn too_large() -> Error {
    Error::program_limit("a json value is limited to 4 GB")
}

/// The order of an object's keys: shorter first, then by bytes (as
/// `jsonb` keeps them, so text and order come out as PostgreSQL's).
pub fn key_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// An object of `pairs` — each a key and the range of its value's bytes
/// in `values` — sorted by key, the last of equal keys kept.
pub(crate) fn put_object<K: AsRef<str>>(
    out: &mut Vec<u8>,
    pairs: &mut Vec<(K, std::ops::Range<usize>)>,
    values: &[u8],
) -> Result<()> {
    // A stable sort keeps duplicates in their order: the last one wins.
    pairs.sort_by(|a, b| key_order(a.0.as_ref().as_bytes(), b.0.as_ref().as_bytes()));
    let mut w = 0;
    for r in 0..pairs.len() {
        if r + 1 < pairs.len() && pairs[r + 1].0.as_ref() == pairs[r].0.as_ref() {
            continue;
        }
        pairs.swap(w, r);
        w += 1;
    }
    pairs.truncate(w);
    let keys: usize = pairs.iter().map(|p| p.0.as_ref().len()).sum();
    let vals: usize = pairs.iter().map(|p| p.1.len()).sum();
    if u32::try_from(keys.max(vals)).is_err() {
        return Err(too_large());
    }
    let (code, width) = width_for(keys.max(vals));
    out.push((OBJ << 4) | code);
    put_varint(out, pairs.len() as u64);
    out.reserve(2 * pairs.len() * width + keys + vals);
    let mut end = 0;
    for p in pairs.iter() {
        end += p.0.as_ref().len();
        put_end(out, end, width);
    }
    end = 0;
    for p in pairs.iter() {
        end += p.1.len();
        put_end(out, end, width);
    }
    for p in pairs.iter() {
        out.extend_from_slice(p.0.as_ref().as_bytes());
    }
    for p in pairs.iter() {
        out.extend_from_slice(&values[p.1.clone()]);
    }
    Ok(())
}

/// Checks that `b` is one canonical value, exactly — what bytes from
/// outside (a key, a fuzzer) must be before they are a value.
pub fn check(b: &[u8]) -> Result<()> {
    let mut re = Vec::with_capacity(b.len());
    rebuild(root(b)?, &mut re, 0)?;
    if re != b {
        return Err(malformed());
    }
    Ok(())
}

/// Writes `n` again, canonically.
pub(crate) fn rebuild(n: Node<'_>, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > super::MAX_DEPTH {
        return Err(super::too_deep());
    }
    match n {
        Node::Null => put_null(out),
        Node::Bool(b) => put_bool(out, b),
        Node::Int(i) => put_int(out, i),
        Node::Dec(d) => {
            let digits = d.digits();
            if digits.iter().any(|x| *x > 9) {
                return Err(malformed());
            }
            put_number(out, d.negative, &digits, d.exp)?;
        }
        Node::Str(s) if s.contains(&0) => return Err(malformed()),
        Node::Str(s) => put_str(out, text_of(s)?),
        Node::Array(a) => {
            let mut data = Vec::new();
            let mut ends = Vec::with_capacity(a.len());
            for i in 0..a.len() {
                rebuild(a.get(i)?, &mut data, depth + 1)?;
                ends.push(data.len());
            }
            put_array(out, &ends, &data)?;
        }
        Node::Object(o) => {
            let mut data = Vec::new();
            let mut pairs = Vec::with_capacity(o.len());
            for i in 0..o.len() {
                let start = data.len();
                rebuild(o.value(i)?, &mut data, depth + 1)?;
                let k = o.key(i)?;
                if k.contains('\0') {
                    return Err(malformed());
                }
                pairs.push((k, start..data.len()));
            }
            put_object(out, &mut pairs, &data)?;
        }
    }
    Ok(())
}
