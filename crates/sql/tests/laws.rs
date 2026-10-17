//! The type law kit (`nucleus_testkit::laws`, PLAN.md §M14.19) on every
//! type the built-in module registers, and on arrays of the built-in
//! ones.

use std::sync::Arc;

use nucleus_sql::ext::*;
use nucleus_testkit::laws;

const CASES: usize = 3000;

#[test]
fn every_builtin_type_keeps_the_laws() {
    let registry = Registry::builtin();
    let mut problems = Vec::new();
    for (ty, _) in registry.types() {
        problems.extend(laws::check_builtin(*ty, CASES));
        if let Some(array) = ty.array_of() {
            problems.extend(laws::check_builtin(array, CASES));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// A type whose order is the reverse of its key encoding's byte order —
/// what would make an index range scan return the wrong rows.
struct Backwards;

struct Reversed;

impl TotalOrder for Reversed {
    fn compare(&self, a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
        Ok(b.ext_bytes().cmp(&a.ext_bytes()))
    }
}

/// The raw bytes as the key, for the type named `.0`.
struct Raw(&'static str);

impl KeyEncoding for Raw {
    fn encode(&self, v: &Value, out: &mut Vec<u8>) {
        out.extend_from_slice(v.ext_bytes().unwrap());
    }
    fn decode(&self, b: &[u8]) -> Result<(Value, usize)> {
        let t = TypeRef::named(self.0).unwrap();
        Ok((Value::ext(t, b[..2].to_vec()), 2))
    }
}

impl Type for Backwards {
    fn name(&self) -> &str {
        "backwards"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let n: u16 = text
            .parse()
            .map_err(|_| Error::invalid_text("not a number"))?;
        Ok(Value::ext(
            TypeRef::named("backwards").unwrap(),
            n.to_be_bytes().to_vec(),
        ))
    }
    fn output(&self, v: &Value) -> String {
        let b = v.ext_bytes().unwrap();
        u16::from_be_bytes([b[0], b[1]]).to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn hashing(&self) -> Option<&dyn Hashing> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&Reversed)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Raw("backwards"))
    }
}

#[test]
fn the_kit_catches_a_broken_type() {
    let mut r = Registry::empty();
    r.add_type(Arc::new(Backwards)).unwrap();
    let ty = r.type_named("backwards").unwrap();
    let t = TypeRef::named("backwards").unwrap();
    let mut generate =
        |g: &mut laws::Gen| Value::ext(t, (g.below(50) as u16).to_be_bytes().to_vec());
    let problems = laws::check(ty, &mut generate, 300);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("the key encoding's byte order is not the type's order")),
        "{problems:#?}"
    );
}

/// The same broken type, with examples: registration checks the laws on
/// them and refuses it.
struct BackwardsWithExamples;

impl Type for BackwardsWithExamples {
    fn name(&self) -> &str {
        "backwards_with_examples"
    }
    fn input(&self, text: &str) -> Result<Value> {
        let n: u16 = text
            .parse()
            .map_err(|_| Error::invalid_text("not a number"))?;
        let t = TypeRef::named("backwards_with_examples").unwrap();
        Ok(Value::ext(t, n.to_be_bytes().to_vec()))
    }
    fn output(&self, v: &Value) -> String {
        let b = v.ext_bytes().unwrap();
        u16::from_be_bytes([b[0], b[1]]).to_string()
    }
    fn equality(&self) -> Option<&dyn Equality> {
        Some(&CanonicalBytes)
    }
    fn ordering(&self) -> Option<&dyn TotalOrder> {
        Some(&Reversed)
    }
    fn key_encoding(&self) -> Option<&dyn KeyEncoding> {
        Some(&Raw("backwards_with_examples"))
    }
    fn examples(&self) -> Vec<Value> {
        let t = TypeRef::named("backwards_with_examples").unwrap();
        [1u16, 2, 300]
            .map(|n| Value::ext(t, n.to_be_bytes().to_vec()))
            .to_vec()
    }
}

#[test]
fn a_type_breaking_a_law_is_not_registered() {
    let mut r = Registry::empty();
    let e = r.add_type(Arc::new(BackwardsWithExamples)).unwrap_err();
    assert!(
        e.message
            .contains("the key encoding's byte order is not the type's order"),
        "{e}"
    );
    assert!(r.type_named("backwards_with_examples").is_none());
}
