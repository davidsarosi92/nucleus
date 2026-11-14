//! `json` (PLAN.md §M14.15): text parsed into the binary form and back,
//! and bytes read as the binary form — the laws that make the form
//! canonical hold, and nothing panics.
//!
//! The first byte picks the mode: even, the rest is JSON text; odd, the
//! rest is a value's bytes (as a corrupted page or a key would hand them).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nucleus_sql::json::{form, key, parse, to_pretty, to_text};

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
}
