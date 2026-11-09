//! Text analyzers on any text (PLAN.md §M14.21 T1): every registered
//! analyzer takes every string without a panic, and its tokens point into
//! the text at character boundaries, in order.

use nucleus_sql::text::{named, names, register_builtins, tokens};
use proptest::prelude::*;

/// Strings heavy in what analyzers look at: apostrophes, multi-byte
/// letters, combining marks, digits and separators, emoji, CJK.
fn texty() -> impl Strategy<Value = String> {
    let pieces = prop::sample::select(vec![
        "a", "s", "S", "'", "\u{2019}", "\u{FF07}", "ñ", "é", "e\u{301}", "ß", "İ", "Σ", "ς", " ",
        "\n", "\r\n", ".", ",", ":", "_", "3", "٣", "日", "テ", "\u{200D}", "👍", "🇭🇺", "\u{0308}",
        "\"", "-", "ing", "ies", "sses", "ational", "logi", "bli",
    ]);
    prop::collection::vec(pieces, 0..40).prop_map(|v| v.concat())
}

fn check(text: &str) {
    for name in names() {
        let analyzer = named(&name).unwrap();
        let mut position = 0;
        for t in tokens(analyzer.as_ref(), text) {
            assert!(
                t.start <= t.end && t.end <= text.len(),
                "{name}: {t:?} in {text:?}"
            );
            assert!(
                text.is_char_boundary(t.start) && text.is_char_boundary(t.end),
                "{name}: {t:?} in {text:?}"
            );
            assert!(t.position >= position, "{name}: {t:?} in {text:?}");
            position = t.position;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn analyzers_take_any_text(text in any::<String>()) {
        register_builtins();
        check(&text);
    }

    #[test]
    fn analyzers_take_texty_text(text in texty()) {
        register_builtins();
        check(&text);
    }
}
