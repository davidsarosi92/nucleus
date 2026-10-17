//! The laws every type's abilities must keep (PLAN.md §M14.19, "Laws,
//! checked for every type"), checked on given values: equality is an
//! equivalence; hashing agrees with equality; the order is total and
//! agrees with equality; the key encoding's byte order is the type's
//! order, it decodes back and is self-delimiting; text output parses back
//! to an equal value; the row encoding reads back what it wrote; and the
//! engine's own comparisons and hashing (`Value`'s) agree with the type's.
//!
//! [`crate::ext::Registry::add_type`] runs them on a type's
//! [`examples`](crate::ext::Type::examples) and refuses a type that breaks
//! one; the test kit (`nucleus_testkit::laws`) runs them property-based.

use std::cmp::Ordering;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::encoding::{Reader, decode_key_value, decode_value, encode_key_value, encode_value};
use crate::ext::Type;
use crate::types::Value;

/// Collects broken laws, at most 20.
pub struct Report {
    name: String,
    /// The broken laws, described with the values that broke them.
    pub problems: Vec<String>,
}

impl Report {
    /// A report for type `t`.
    pub fn new(t: &dyn Type) -> Self {
        Self {
            name: t.name().to_string(),
            problems: Vec::new(),
        }
    }

    fn fail(&mut self, law: &str, vals: &[&Value]) {
        if self.problems.len() < 20 {
            let shown: Vec<String> = vals.iter().map(|v| format!("{v:?}")).collect();
            self.problems
                .push(format!("{}: {law}: {}", self.name, shown.join(", ")));
        }
    }
}

/// Every law on every value, pair and triple of `values`.
pub fn check_values(t: &dyn Type, values: &[Value]) -> Vec<String> {
    let mut r = Report::new(t);
    for a in values {
        check_one(t, a, &mut r);
        for b in values {
            check_pair(t, a, b, &mut r);
            for c in values.iter().take(16) {
                check_triple(t, a, b, c, &mut r);
            }
        }
    }
    r.problems
}

fn eq(t: &dyn Type, a: &Value, b: &Value) -> Option<bool> {
    t.equality().map(|e| e.equal(a, b).unwrap_or(false))
}

fn cmp(t: &dyn Type, a: &Value, b: &Value) -> Option<Ordering> {
    t.ordering()
        .map(|o| o.compare(a, b).unwrap_or(Ordering::Equal))
}

fn type_hash(t: &dyn Type, v: &Value) -> Option<u64> {
    t.hashing().map(|h| {
        let mut s = DefaultHasher::new();
        h.hash(v, &mut s);
        s.finish()
    })
}

fn value_hash(v: &Value) -> u64 {
    let mut s = DefaultHasher::new();
    v.hash(&mut s);
    s.finish()
}

fn key(t: &dyn Type, v: &Value) -> Option<Vec<u8>> {
    t.key_encoding().map(|k| {
        let mut out = Vec::new();
        k.encode(v, &mut out);
        out
    })
}

fn engine_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key_value(&mut out, v);
    out
}

/// Laws of one value.
pub fn check_one(t: &dyn Type, a: &Value, r: &mut Report) {
    // Text output parses back to an equal value.
    match t.input(&t.output(a)) {
        Ok(back) => {
            if eq(t, &back, a) == Some(false)
                || (t.equality().is_none() && back.ext_bytes() != a.ext_bytes())
            {
                r.fail(
                    "text output does not parse back to an equal value",
                    &[a, &back],
                );
            }
        }
        Err(e) => r.fail(&format!("text output does not parse back ({e})"), &[a]),
    }
    // The row encoding reads back what it wrote.
    let mut row = Vec::new();
    encode_value(&mut row, a);
    match decode_value(&mut Reader::new(&row)) {
        Ok(back) if back == *a && back.ext_bytes() == a.ext_bytes() => {}
        Ok(back) => r.fail("the row encoding reads back another value", &[a, &back]),
        Err(e) => r.fail(&format!("the row encoding does not read back ({e})"), &[a]),
    }
    if eq(t, a, a) == Some(false) {
        r.fail("equality is not reflexive", &[a]);
    }
    if cmp(t, a, a).is_some_and(|o| o != Ordering::Equal) {
        r.fail("the order is not reflexive", &[a]);
    }
    if let Some(k) = key(t, a) {
        let decoded = t.key_encoding().unwrap().decode(&k);
        match decoded {
            Ok((back, used)) => {
                if used != k.len() {
                    r.fail("the key encoding does not decode its own length", &[a]);
                }
                if eq(t, &back, a) == Some(false) {
                    r.fail(
                        "the key encoding does not decode back to an equal value",
                        &[a, &back],
                    );
                }
            }
            Err(e) => r.fail(&format!("the key encoding does not decode ({e})"), &[a]),
        }
        // Self-delimiting: trailing bytes are not consumed.
        let mut longer = k.clone();
        longer.extend_from_slice(&[0x00, 0xFF, 0x7F]);
        if t.key_encoding()
            .unwrap()
            .decode(&longer)
            .map(|(_, n)| n)
            .ok()
            != Some(k.len())
        {
            r.fail("the key encoding is not self-delimiting", &[a]);
        }
        // The engine's key encoding (tagged) round-trips too.
        let ek = engine_key(a);
        match decode_key_value(&ek) {
            Ok((back, used)) if used == ek.len() && back == *a => {}
            Ok((back, _)) => r.fail(
                "the engine's key encoding decodes another value",
                &[a, &back],
            ),
            Err(e) => r.fail(
                &format!("the engine's key encoding does not decode ({e})"),
                &[a],
            ),
        }
    }
}

/// Laws of an ordered pair.
pub fn check_pair(t: &dyn Type, a: &Value, b: &Value, r: &mut Report) {
    let e = eq(t, a, b);
    if let Some(e) = e {
        if eq(t, b, a) != Some(e) {
            r.fail("equality is not symmetric", &[a, b]);
        }
        // The engine's equality is the type's.
        if a.sql_eq(b).ok().flatten() != Some(e) {
            r.fail(
                "the engine's equality (sql_eq) disagrees with the type's",
                &[a, b],
            );
        }
        if (a == b) != e {
            r.fail("Value's equality disagrees with the type's", &[a, b]);
        }
        if e {
            if let (Some(x), Some(y)) = (type_hash(t, a), type_hash(t, b))
                && x != y
            {
                r.fail("hashing disagrees with equality", &[a, b]);
            }
            if value_hash(a) != value_hash(b) {
                r.fail("Value's hash disagrees with equality", &[a, b]);
            }
        }
    }
    if let Some(o) = cmp(t, a, b) {
        if cmp(t, b, a) != Some(o.reverse()) {
            r.fail("the order is not antisymmetric", &[a, b]);
        }
        if let Some(e) = e
            && (o == Ordering::Equal) != e
        {
            r.fail("the order disagrees with equality", &[a, b]);
        }
        if a.sql_cmp(b).ok().flatten() != Some(o) {
            r.fail(
                "the engine's order (sql_cmp) disagrees with the type's",
                &[a, b],
            );
        }
        if let (Some(ka), Some(kb)) = (key(t, a), key(t, b))
            && ka.cmp(&kb) != o
        {
            r.fail(
                "the key encoding's byte order is not the type's order",
                &[a, b],
            );
        }
        if engine_key(a).cmp(&engine_key(b)) != o && t.key_encoding().is_some() {
            r.fail(
                "the engine's key encoding's byte order is not the type's order",
                &[a, b],
            );
        }
    } else if let (Some(ka), Some(kb), Some(e)) = (key(t, a), key(t, b), e)
        && (ka == kb) != e
    {
        r.fail("equal keys and equal values differ", &[a, b]);
    }
}

/// Transitivity.
pub fn check_triple(t: &dyn Type, a: &Value, b: &Value, c: &Value, r: &mut Report) {
    if eq(t, a, b) == Some(true) && eq(t, b, c) == Some(true) && eq(t, a, c) == Some(false) {
        r.fail("equality is not transitive", &[a, b, c]);
    }
    if let (Some(ab), Some(bc), Some(ac)) = (cmp(t, a, b), cmp(t, b, c), cmp(t, a, c))
        && ab != Ordering::Greater
        && bc != Ordering::Greater
        && ac == Ordering::Greater
    {
        r.fail("the order is not transitive", &[a, b, c]);
    }
}
