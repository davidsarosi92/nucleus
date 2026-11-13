//! The SQL lexer and parser: any text an application sends.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(sql) = std::str::from_utf8(data) else {
        return;
    };
    let _ = nucleus_sql::lexer::tokenize(sql);
    if let Ok(statements) = nucleus_sql::parser::parse_with_text(sql) {
        // What was parsed parses again the same from its own text.
        for (stmt, text) in statements {
            let again = nucleus_sql::parser::parse_one(&text)
                .unwrap_or_else(|e| panic!("{text:?} parsed once, not again: {e:?}"));
            assert_eq!(again, stmt, "{text:?}");
        }
    }
    let _ = nucleus_sql::parser::parse_expr(sql);
});
