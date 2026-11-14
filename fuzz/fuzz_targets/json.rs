//! `json` (PLAN.md §M14.15): text parsed into the binary form and back,
//! and bytes read as the binary form — the laws that make the form
//! canonical hold, and nothing panics; and the operators (J2) on the
//! value make canonical values, with their own laws (a value contains
//! itself; a key deleted is gone, set it is there).
//!
//! The first byte picks the mode: even, the rest is JSON text; odd, the
//! rest is a value's bytes (as a corrupted page or a key would hand them).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nucleus_sql::json::{form, key, ops, parse, to_pretty, to_text};

fuzz_target!(|data: &[u8]| {
    let Some((mode, rest)) = data.split_first() else {
        return;
    };
    if mode % 2 == 0 {
        let Ok(text) = std::str::from_utf8(rest) else {
            return;
        };
        let Ok(b) = parse::parse(text) else {
            return;
        };
        laws(&b);
    } else if form::check(rest).is_ok() {
        laws(rest);
    } else {
        // Malformed bytes: read without a panic, whatever comes back.
        let _ = form::root(rest).map(|n| {
            let mut s = Vec::new();
            let _ = nucleus_sql::json::text::write_bytes(n, &mut s);
        });
        let _ = key::decode(rest);
    }
});

/// A canonical value's laws.
fn laws(b: &[u8]) {
    form::check(b).expect("the parser writes canonical bytes");
    let text = to_text(b).expect("a value has a text");
    assert_eq!(
        parse::parse(&text).expect("its text parses"),
        b,
        "text round trip"
    );
    let pretty = to_pretty(b).expect("and an indented one");
    assert_eq!(
        parse::parse(&pretty).expect("which parses"),
        b,
        "pretty round trip"
    );
    let k = key::of(b).expect("a key");
    let (back, used) = key::decode(&k).expect("the key decodes");
    assert_eq!((back.as_slice(), used), (b, k.len()), "key round trip");
    assert_eq!(
        key::compare(b, b).expect("compares"),
        std::cmp::Ordering::Equal
    );
    operators(b);
}

/// The operators' laws on a canonical value.
fn operators(b: &[u8]) {
    let canonical = |v: &[u8], what: &str| {
        form::check(v).unwrap_or_else(|e| panic!("{what} is canonical: {e}"));
    };
    assert!(
        ops::contains(b, b).expect("contains"),
        "a value contains itself"
    );
    canonical(&ops::concat(b, b).expect("concat"), "||");
    canonical(&ops::strip_nulls(b, true).expect("strip"), "strip_nulls");
    let Ok(form::Node::Object(o)) = form::root(b) else {
        if let Ok(form::Node::Array(a)) = form::root(b) {
            for i in [0i64, -1, a.len() as i64] {
                canonical(&ops::delete_index(b, i).expect("- i"), "- i");
            }
            let path = [Some("0".to_string())];
            let set = ops::set(b, &path, b"\x00", true).expect("set");
            canonical(&set, "json_set");
            canonical(
                &ops::insert(b, &path, b"\x00", true).expect("insert"),
                "json_insert",
            );
            canonical(&ops::delete_path(b, &path).expect("#-"), "#-");
        }
        return;
    };
    for i in 0..o.len().min(8) {
        let k = o.key(i).expect("a key").to_string();
        assert_eq!(
            ops::field(b, &k).expect("->"),
            Some(o.value_bytes(i).expect("its value")),
            "-> finds each key"
        );
        let gone = ops::delete_keys(b, &[k.as_str()]).expect("-");
        canonical(&gone, "-");
        assert!(!ops::exists(&gone, &k).expect("?"), "a deleted key is gone");
        assert!(
            ops::contains(b, &gone).expect("@>"),
            "a value contains itself less a key"
        );
        let path = [Some(k.clone())];
        let set = ops::set(&gone, &path, b"\x00", true).expect("set");
        canonical(&set, "json_set");
        assert!(ops::exists(&set, &k).expect("?"), "a key set is there");
    }
}
