//! Rows kept a column at a time: what an operator holds when it gathers
//! many rows from batches (a hash join's table) — one array a column, no
//! allocation per row, and for numbers a fraction of a row's memory.

use crate::types::{Row, Value};
use crate::vector::{Batch, Sel, TimeKind, Vector};

/// Rows, a column at a time.
pub(crate) struct Columns {
    cols: Vec<Col>,
    len: usize,
    /// The memory of the rows pushed one at a time, roughly.
    pushed: usize,
}

/// One column: typed while every value is of one kind (or `NULL`), any
/// values once they are not.
#[derive(Clone)]
enum Col {
    /// Only `NULL`s so far: how many.
    Nulls(usize),
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    /// Text (`char(n)` when the flag is set): row `i`'s bytes
    /// `bytes[offsets[i]..offsets[i + 1]]`.
    Text(bool, Vec<u32>, Vec<u8>, Vec<bool>),
    Time(TimeKind, Vec<i64>, Vec<bool>),
    Values(Vec<Value>),
}

impl Col {
    /// An empty column of `v`'s kind, after `nulls` `NULL`s.
    fn start(v: &Value, nulls: usize) -> Col {
        let n = vec![true; nulls];
        match v {
            Value::Int(_) => Col::Int(vec![0; nulls], n),
            Value::Float(_) => Col::Float(vec![0.0; nulls], n),
            Value::Bool(_) => Col::Bool(vec![false; nulls], n),
            Value::Text(_) | Value::Char(_) => Col::Text(
                matches!(v, Value::Char(_)),
                vec![0; nulls + 1],
                Vec::new(),
                n,
            ),
            v => match TimeKind::of(v) {
                Some((k, _)) => Col::Time(k, vec![0; nulls], n),
                None => Col::Values(vec![Value::Null; nulls]),
            },
        }
    }

    fn len(&self) -> usize {
        match self {
            Col::Nulls(n) => *n,
            Col::Int(d, _) => d.len(),
            Col::Float(d, _) => d.len(),
            Col::Bool(d, _) => d.len(),
            Col::Text(_, o, _, _) => o.len() - 1,
            Col::Time(_, d, _) => d.len(),
            Col::Values(v) => v.len(),
        }
    }

    /// Row `i`'s value.
    fn get(&self, i: usize) -> Value {
        match self {
            Col::Nulls(_) => Value::Null,
            Col::Int(_, n) | Col::Float(_, n) | Col::Bool(_, n) | Col::Time(_, _, n) if n[i] => {
                Value::Null
            }
            Col::Text(_, _, _, n) if n[i] => Value::Null,
            Col::Int(d, _) => Value::Int(d[i]),
            Col::Float(d, _) => Value::Float(d[i]),
            Col::Bool(d, _) => Value::Bool(d[i]),
            Col::Time(k, d, _) => k.value(d[i]),
            Col::Text(char, o, b, _) => {
                let s = String::from_utf8_lossy(&b[o[i] as usize..o[i + 1] as usize]).into_owned();
                if *char {
                    Value::Char(s)
                } else {
                    Value::Text(s)
                }
            }
            Col::Values(v) => v[i].clone(),
        }
    }

    /// Any values from here on (a value of another kind came).
    fn untyped(&mut self) -> &mut Vec<Value> {
        if !matches!(self, Col::Values(_)) {
            let values = (0..self.len()).map(|i| self.get(i)).collect();
            *self = Col::Values(values);
        }
        match self {
            Col::Values(v) => v,
            _ => unreachable!("made above"),
        }
    }

    fn push(&mut self, v: Value) {
        if let Col::Nulls(n) = self {
            if v.is_null() {
                *n += 1;
                return;
            }
            *self = Col::start(&v, *n);
        }
        match (&mut *self, v) {
            (Col::Int(d, n), Value::Int(x)) => (d.push(x), n.push(false)).1,
            (Col::Float(d, n), Value::Float(x)) => (d.push(x), n.push(false)).1,
            (Col::Bool(d, n), Value::Bool(x)) => (d.push(x), n.push(false)).1,
            (Col::Text(false, o, b, n), Value::Text(s))
            | (Col::Text(true, o, b, n), Value::Char(s)) => {
                b.extend_from_slice(s.as_bytes());
                o.push(b.len() as u32);
                n.push(false);
            }
            (Col::Int(d, n) | Col::Time(_, d, n), Value::Null) => (d.push(0), n.push(true)).1,
            (Col::Float(d, n), Value::Null) => (d.push(0.0), n.push(true)).1,
            (Col::Bool(d, n), Value::Null) => (d.push(false), n.push(true)).1,
            (Col::Text(_, o, _, n), Value::Null) => {
                o.push(*o.last().expect("an offset"));
                n.push(true);
            }
            (Col::Time(k, d, n), v) if TimeKind::of(&v).is_some_and(|(kv, _)| kv == *k) => {
                d.push(TimeKind::of(&v).expect("a time").1);
                n.push(false);
            }
            (Col::Values(vs), v) => vs.push(v),
            (col, v) => col.untyped().push(v),
        }
    }

    /// Rows `sel` of `v` appended — a typed vector of this column's kind
    /// copied as it is.
    fn append(&mut self, v: &Vector, sel: &Sel) {
        if let Col::Nulls(n) = self {
            // Typed from the vector's kind (its first value tells).
            let start = match v {
                Vector::Int(..) => Some(Col::Int(vec![0; *n], vec![true; *n])),
                Vector::Float(..) => Some(Col::Float(vec![0.0; *n], vec![true; *n])),
                Vector::Bool(..) => Some(Col::Bool(vec![false; *n], vec![true; *n])),
                Vector::Text(..) | Vector::Char(..) => Some(Col::Text(
                    matches!(v, Vector::Char(..)),
                    vec![0; *n + 1],
                    Vec::new(),
                    vec![true; *n],
                )),
                Vector::Time(k, ..) => Some(Col::Time(*k, vec![0; *n], vec![true; *n])),
                _ => None,
            };
            if let Some(c) = start {
                *self = c;
            }
        }
        let null = |vn: &Option<Vec<bool>>, i: usize| vn.as_ref().is_some_and(|n| n[i]);
        match (&mut *self, v) {
            (Col::Int(d, n), Vector::Int(s, sn)) => {
                for i in sel.iter() {
                    d.push(s[i]);
                    n.push(null(sn, i));
                }
            }
            (Col::Float(d, n), Vector::Float(s, sn)) => {
                for i in sel.iter() {
                    d.push(s[i]);
                    n.push(null(sn, i));
                }
            }
            (Col::Bool(d, n), Vector::Bool(s, sn)) => {
                for i in sel.iter() {
                    d.push(s[i]);
                    n.push(null(sn, i));
                }
            }
            (Col::Time(k, d, n), Vector::Time(sk, s, sn)) if k == sk => {
                for i in sel.iter() {
                    d.push(s[i]);
                    n.push(null(sn, i));
                }
            }
            (Col::Text(false, o, b, n), Vector::Text(so, sb, sn))
            | (Col::Text(true, o, b, n), Vector::Char(so, sb, sn)) => {
                for i in sel.iter() {
                    b.extend_from_slice(&sb[so[i] as usize..so[i + 1] as usize]);
                    o.push(b.len() as u32);
                    n.push(null(sn, i));
                }
            }
            (col, v) => {
                for i in sel.iter() {
                    col.push(v.value(i));
                }
            }
        }
    }

    /// Its memory, roughly.
    fn bytes(&self) -> usize {
        match self {
            Col::Nulls(_) => 0,
            Col::Int(d, _) | Col::Time(_, d, _) => d.len() * 9,
            Col::Float(d, _) => d.len() * 9,
            Col::Bool(d, _) => d.len() * 2,
            Col::Text(_, o, b, _) => o.len() * 5 + b.len(),
            Col::Values(v) => v.iter().map(crate::spill::value_bytes).sum(),
        }
    }
}

impl Columns {
    /// No rows, of `width` columns.
    pub(crate) fn new(width: usize) -> Self {
        Columns {
            cols: (0..width).map(|_| Col::Nulls(0)).collect(),
            len: 0,
            pushed: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// A batch's rows in play appended.
    pub(crate) fn push_batch(&mut self, b: &Batch) {
        self.push_vectors(&b.cols, &b.sel);
    }

    /// Rows `sel` of `vectors` (a column each) appended.
    pub(crate) fn push_vectors(&mut self, vectors: &[std::rc::Rc<Vector>], sel: &Sel) {
        for (col, v) in self.cols.iter_mut().zip(vectors) {
            col.append(v, sel);
        }
        self.len += sel.len();
    }

    /// A single integer column's values, `None` for its `NULL`s — when
    /// it is one (or only `NULL`s).
    pub(crate) fn ints(&self) -> Option<impl ExactSizeIterator<Item = Option<i64>> + '_> {
        let (d, n): (&[i64], &[bool]) = match &self.cols[..] {
            [Col::Int(d, n)] => (d, n),
            [Col::Nulls(_)] => (&[], &[]),
            _ => return None,
        };
        Some((0..self.len).map(move |i| n.get(i).is_some_and(|&n| !n).then(|| d[i])))
    }

    /// Two integer columns' values, `None` where either is `NULL` — when
    /// they are (or only `NULL`s).
    pub(crate) fn int_pairs(
        &self,
    ) -> Option<impl ExactSizeIterator<Item = Option<(i64, i64)>> + '_> {
        fn col(c: &Col) -> Option<(&[i64], &[bool])> {
            match c {
                Col::Int(d, n) => Some((d, n)),
                Col::Nulls(_) => Some((&[], &[])),
                _ => None,
            }
        }
        let [a, b] = &self.cols[..] else {
            return None;
        };
        let ((da, na), (db, nb)) = (col(a)?, col(b)?);
        let live = |d: &[i64], n: &[bool], i: usize| n.get(i).is_some_and(|&n| !n).then(|| d[i]);
        Some((0..self.len).map(move |i| Some((live(da, na, i)?, live(db, nb, i)?))))
    }

    /// A row appended.
    pub(crate) fn push_row(&mut self, row: Row) {
        for (col, v) in self.cols.iter_mut().zip(row) {
            self.pushed += match &v {
                Value::Null | Value::Int(_) | Value::Float(_) | Value::Bool(_) => 9,
                Value::Text(s) | Value::Char(s) => 5 + s.len(),
                v if TimeKind::of(v).is_some() => 9,
                v => crate::spill::value_bytes(v),
            };
            col.push(v);
        }
        self.len += 1;
    }

    /// The memory of the rows [`push_row`](Self::push_row) added, roughly
    /// (counted as they came: no pass over them).
    pub(crate) fn pushed_bytes(&self) -> usize {
        self.pushed
    }

    /// Columns `cols` of these rows, copied.
    pub(crate) fn project(&self, cols: &[usize]) -> Columns {
        Columns {
            cols: cols.iter().map(|&c| self.cols[c].clone()).collect(),
            len: self.len,
            pushed: 0,
        }
    }

    /// `other`'s rows appended after these.
    pub(crate) fn append(&mut self, other: Columns) {
        if self.len == 0 {
            *self = other;
            return;
        }
        for (col, o) in self.cols.iter_mut().zip(other.cols) {
            match (&mut *col, o) {
                (Col::Nulls(a), Col::Nulls(b)) => *a += b,
                (Col::Int(d, n), Col::Int(od, on)) => {
                    d.extend(od);
                    n.extend(on);
                }
                (Col::Time(k, d, n), Col::Time(ok, od, on)) if *k == ok => {
                    d.extend(od);
                    n.extend(on);
                }
                (Col::Float(d, n), Col::Float(od, on)) => {
                    d.extend(od);
                    n.extend(on);
                }
                (Col::Text(c, o, b, n), Col::Text(oc, oo, ob, on)) if *c == oc => {
                    let base = b.len() as u32;
                    o.extend(oo[1..].iter().map(|&x| x + base));
                    b.extend(ob);
                    n.extend(on);
                }
                (Col::Values(v), Col::Values(ov)) => v.extend(ov),
                (col, o) => {
                    for i in 0..o.len() {
                        col.push(o.get(i));
                    }
                }
            }
        }
        self.len += other.len;
    }

    /// Column `c`'s rows `rows` (`None`: a `NULL`), as a vector.
    pub(crate) fn gather(&self, c: usize, rows: &[Option<u32>]) -> Vector {
        fn pick<T: Copy + Default>(d: &[T], rows: &[Option<u32>]) -> Vec<T> {
            rows.iter()
                .map(|r| r.map_or(T::default(), |i| d[i as usize]))
                .collect()
        }
        let nulls = |n: &[bool]| {
            crate::vector::nulls_from(
                rows.iter()
                    .map(|r| r.is_none_or(|i| n[i as usize]))
                    .collect(),
            )
        };
        match &self.cols[c] {
            Col::Nulls(_) => Vector::Const(Value::Null),
            Col::Int(d, n) => Vector::Int(pick(d, rows), nulls(n)),
            Col::Float(d, n) => Vector::Float(pick(d, rows), nulls(n)),
            Col::Bool(d, n) => Vector::Bool(pick(d, rows), nulls(n)),
            Col::Time(k, d, n) => Vector::Time(*k, pick(d, rows), nulls(n)),
            Col::Text(char, o, b, n) => {
                let mut offsets = Vec::with_capacity(rows.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0);
                // A NULL's place holds no bytes.
                for &i in rows.iter().flatten() {
                    let i = i as usize;
                    bytes.extend_from_slice(&b[o[i] as usize..o[i + 1] as usize]);
                }
                let mut at = 0;
                for r in rows {
                    if let &Some(i) = r {
                        at += o[i as usize + 1] - o[i as usize];
                    }
                    offsets.push(at);
                }
                if *char {
                    Vector::Char(offsets, bytes, nulls(n))
                } else {
                    Vector::Text(offsets, bytes, nulls(n))
                }
            }
            Col::Values(v) => Vector::typed(
                rows.iter()
                    .map(|r| r.map_or(Value::Null, |i| v[i as usize].clone()))
                    .collect(),
            ),
        }
    }

    /// Row `r`'s values appended to `out`.
    pub(crate) fn extend_row(&self, r: usize, out: &mut Row) {
        out.extend(self.cols.iter().map(|c| c.get(r)));
    }

    /// Their memory, roughly.
    pub(crate) fn bytes(&self) -> usize {
        self.cols.iter().map(Col::bytes).sum()
    }
}
