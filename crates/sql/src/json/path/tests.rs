use super::*;

fn norm(s: &str) -> String {
    match parse(s) {
        Ok(p) => p.to_string(),
        Err(e) => format!("error {}", e.code),
    }
}

/// Paths as PostgreSQL 18 prints them back (`'…'::jsonpath`).
#[test]
fn normalized_as_postgresql() {
    for (input, want) in [
        ("$.a", "$.\"a\""),
        ("$a", "$\"a\""),
        ("lax $", "$"),
        ("strict $.a", "strict $.\"a\""),
        ("$.a ? (@ > 1)", "$.\"a\"?(@ > 1)"),
        ("$ + 1", "($ + 1)"),
        ("$.a + $.b * 2", "($.\"a\" + $.\"b\" * 2)"),
        ("($.a + $.b) * 2", "(($.\"a\" + $.\"b\") * 2)"),
        ("$ - 2 - 1", "(($ - 2) - 1)"),
        ("$ - (2 - 1)", "($ - (2 - 1))"),
        ("-$.a", "(-$.\"a\")"),
        ("-(1 + 2)", "(-(1 + 2))"),
        (
            "$ ? (@.a > 1 && @.b < 2 || !(@.c == 3))",
            "$?(@.\"a\" > 1 && @.\"b\" < 2 || !(@.\"c\" == 3))",
        ),
        (
            "$ ? ((@.a == 1 || @.b == 2) && @.c == 3)",
            "$?((@.\"a\" == 1 || @.\"b\" == 2) && @.\"c\" == 3)",
        ),
        ("$ ? (exists (@.a))", "$?(exists (@.\"a\"))"),
        ("$ ? (!exists(@.a))", "$?(!(exists (@.\"a\")))"),
        ("$ ? ((@ > 1) is unknown)", "$?((@ > 1) is unknown)"),
        (
            "$ ? (@ like_regex \"a\" flag \"ism\")",
            "$?(@ like_regex \"a\" flag \"ism\")",
        ),
        (
            "$ ? (@ like_regex \"a\" flag \"mi\")",
            "$?(@ like_regex \"a\" flag \"im\")",
        ),
        ("$ ? (@ starts with $v)", "$?(@ starts with $\"v\")"),
        ("$ ? (@ <> 1)", "$?(@ != 1)"),
        ("$.decimal(4,2)", "$.decimal(4,2)"),
        ("$.datetime(\"yyyy\")", "$.datetime(\"yyyy\")"),
        ("1e3", "1000"),
        (".5", "0.5"),
        ("1.0", "1.0"),
        ("0.10", "0.10"),
        ("- 1.5e-2", "-0.015"),
        ("-1", "-1"),
        ("$ ? (@ == -1)", "$?(@ == -1)"),
        ("\"\\u00e9\\x41\\/\\v\"", "\"éA/\\u000b\""),
        ("1.2.a", "(1.2).\"a\""),
        ("\"a\".\"b\"", "\"a\".\"b\""),
        ("($.a).b", "$.\"a\".\"b\""),
        ("$[1.5]", "$[1.5]"),
        ("$.a[$.b]", "$.\"a\"[$.\"b\"]"),
        ("$ ? (@ == 1 ? (@ > 0))", "$?(@ == (1)?(@ > 0))"),
        ("$.size", "$.\"size\""),
        ("$.SIZE()", "$.size()"),
        ("$.A", "$.\"A\""),
        ("$.last", "$.\"last\""),
        ("$.type.type()", "$.\"type\".type()"),
        ("\"\\q\"", "\"q\""),
        ("0x1F", "31"),
        ("0o17", "15"),
        ("0b101", "5"),
        ("1_000", "1000"),
        ("1.", "1"),
        (".5e1", "5"),
        ("$.a /* c */ .b", "$.\"a\".\"b\""),
        ("$.decimal(-1)", "$.decimal(-1)"),
        ("$.time(3)", "$.time(3)"),
        ("$[last - 1 to last]", "$[last - 1 to last]"),
        ("$.a[1,2 to 3]", "$.\"a\"[1,2 to 3]"),
        ("$.**{2 to last}", "$.**{2 to last}"),
        ("$.**{last}", "$.**{last}"),
        ("$.**{last to 2}", "$.**{last to 2}"),
        (
            "$ ? (@.a like_regex \"a\" && 1 + 2 like_regex \"b\")",
            "$?(@.\"a\" like_regex \"a\" && (1 + 2) like_regex \"b\")",
        ),
        ("-$", "(-$)"),
        ("+$.a", "(+$.\"a\")"),
        ("+1", "1"),
        ("- -1", "1"),
        ("$ ? (@ == $\"x y\")", "$?(@ == $\"x y\")"),
        ("$.\"\\u{1F600}\"", "$.\"😀\""),
        ("\"\\ud83d\\ude00\"", "\"😀\""),
        (
            "$ ? (exists(@.a ? (@ > 1)))",
            "$?(exists (@.\"a\"?(@ > 1)))",
        ),
        ("$.a ? (@ > 1).b", "$.\"a\"?(@ > 1).\"b\""),
        ("strict $.*", "strict $.*"),
        ("$.a.**.b", "$.\"a\".**.\"b\""),
        ("$.**.*", "$.**.*"),
        ("$.a[*][*]", "$.\"a\"[*][*]"),
        ("$ ? (!(@ > 1))", "$?(!(@ > 1))"),
        ("(1 > 2)", "(1 > 2)"),
        ("$.e3", "$.\"e3\""),
        ("1.e3", "1000"),
        ("$[1.5e1]", "$[15]"),
        ("$ ? (@ == 1.0e1)", "$?(@ == 10)"),
        ("\"a\" starts with \"b\"", "(\"a\" starts with \"b\")"),
        ("$.keyvalue().key", "$.keyvalue().\"key\""),
        ("$.datetime(  \"x\"  )", "$.datetime(\"x\")"),
        // PostgreSQL prints `@."a" + 1.abs()` here, which reads back as
        // another path.
        ("$ ? ((@.a + 1).abs() > 0)", "$?((@.\"a\" + 1).abs() > 0)"),
    ] {
        assert_eq!(norm(input), want, "{input}");
        // The normalized text reads back as itself.
        assert_eq!(norm(want), want, "{want}");
    }
}

#[test]
fn refused_as_postgresql() {
    for (input, code) in [
        ("$ ? (@ == TRUE)", "42601"),
        ("1e", "42601"),
        ("1a", "42601"),
        ("00", "42601"),
        ("$.decimal(4.5,2)", "42601"),
        ("$.datetime(1)", "42601"),
        ("$ ? (@ like_regex \"a\" flag \"x\")", "0A000"),
        ("$ ? (@ like_regex \"a\" flag \"z\")", "42601"),
        ("$ ? (@ like_regex \"(\")", "2201B"),
        ("@", "42601"),
        ("last", "42601"),
        ("lax", "42601"),
        ("$ $", "42601"),
        ("$ ? (@ > 1 is unknown)", "42601"),
        ("1 > 2 > 3", "42601"),
        ("$.1e3", "42601"),
        ("$.0", "42601"),
        ("$.ab12_$", "42601"),
        ("$ ? (@.a)", "42601"),
        ("(1 > 2) == true", "42601"),
        ("$ ? (exists((@ > 1)))", "42601"),
        ("\"\\u0000\"", "22P05"),
        ("$.a /* c", "42601"),
    ] {
        assert_eq!(norm(input), format!("error {code}"), "{input}");
    }
}

fn query(doc: &str, path: &str) -> String {
    let p = parse(path).expect("a path");
    let d = crate::json::parse::parse(doc).expect("json");
    let mut out = Vec::new();
    match eval::run(&p, &d, None, false, Some(&mut out)) {
        Ok(_) => out
            .iter()
            .map(|i| crate::json::to_text(&i.to_json()).expect("text"))
            .collect::<Vec<_>>()
            .join(" "),
        Err(Fail::Soft(e)) => format!("soft {}", e.code),
        Err(Fail::Hard(e)) => format!("hard {}", e.code),
    }
}

#[test]
fn evaluates_as_postgresql() {
    let doc = r#"{"a": [1, 2, {"b": 3}], "c": {"d": "x"}, "e": null}"#;
    for (path, want) in [
        ("$.a[*]", r#"1 2 {"b": 3}"#),
        ("$.a.b", "3"),
        ("strict $.a.b", "soft 2203A"),
        ("$.z", ""),
        ("strict $.z", "soft 2203A"),
        ("$.a[1 to last]", r#"2 {"b": 3}"#),
        ("$.a[5]", ""),
        ("strict $.a[5]", "soft 22033"),
        ("$.c[0]", r#"{"d": "x"}"#),
        ("strict $.c[0]", "soft 22039"),
        ("$.a[*] ? (@ > 1)", "2"),
        ("$.a ? (@ > 1)", "2"),
        (
            "$.**",
            r#"{"a": [1, 2, {"b": 3}], "c": {"d": "x"}, "e": null} [1, 2, {"b": 3}] 1 2 {"b": 3} 3 {"d": "x"} "x" null"#,
        ),
        ("$.**{2}", r#"1 2 {"b": 3} "x""#),
        ("$.a.size()", "3"),
        ("$.c.size()", "1"),
        ("strict $.c.size()", "soft 22039"),
        ("$.a[0] + 1", "2"),
        ("$.a + 1", "soft 22038"),
        ("$.a[0] / 0", "soft 22012"),
        ("$.a[*].type()", r#""number" "number" "object""#),
        ("$.c.keyvalue().value", r#""x""#),
        (
            "$.keyvalue() ? (@.key == \"e\")",
            r#"{"id": 0, "key": "e", "value": null}"#,
        ),
        ("$.a[0] > 0", "true"),
        ("$.c > 0", "null"),
        ("$.e == null", "true"),
        ("$.e != 1", "true"),
        ("$.c.d starts with \"x\"", "true"),
        ("$.c.d like_regex \"X\" flag \"i\"", "true"),
        ("$.x.y == 1", "false"),
        ("$var", "hard 42704"),
        ("-$.a[*]", "soft 2203B"),
        ("-$.a[0 to 1]", "-1 -2"),
        ("$.a[last]", r#"{"b": 3}"#),
        ("$.a[1.9]", "2"),
        ("$.c.d.datetime()", "soft 22031"),
        ("\"2020-01-01\".datetime().type()", r#""date""#),
        ("\"12.345\".decimal(4,2)", "12.35"),
        ("12.345.decimal(2,2)", "soft 22036"),
        ("\"1.5\".bigint()", "soft 22036"),
        ("\"yes\".boolean()", "true"),
        ("1.5.boolean()", "soft 22036"),
        ("\"1.5\".double()", "1.5"),
        ("\"x\".double()", "soft 22036"),
        ("2.5.floor()", "2"),
        ("(-2.5).abs()", "2.5"),
        ("$.a[0].string()", r#""1""#),
    ] {
        assert_eq!(query(doc, path), want, "{path}");
    }
}
