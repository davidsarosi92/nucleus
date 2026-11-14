use super::*;

fn round(s: &str) -> String {
    let b = parse::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
    to_text(&b).unwrap_or_else(|e| panic!("{s} {b:x?}: {e}"))
}

fn fails(s: &str) -> String {
    match parse::parse(s) {
        Ok(b) => panic!("{s} parsed: {}", to_text(&b).unwrap_or_default()),
        Err(e) => format!("{} {}", e.code, e.message),
    }
}

#[test]
fn text_round_trips() {
    for (input, want) in [
        ("null", "null"),
        (" true ", "true"),
        ("false", "false"),
        ("0", "0"),
        ("-0", "0"),
        ("-0.0", "0"),
        ("42", "42"),
        ("-42", "-42"),
        ("1.50", "1.5"),
        ("1e2", "100"),
        ("1E+2", "100"),
        ("12.5e-1", "1.25"),
        ("0.0001", "0.0001"),
        ("1e-7", "0.0000001"),
        (
            "123456789012345678901234567890",
            "123456789012345678901234567890",
        ),
        ("9223372036854775807", "9223372036854775807"),
        ("-9223372036854775808", "-9223372036854775808"),
        ("9223372036854775808", "9223372036854775808"),
        ("1e20", "100000000000000000000"),
        ("\"\"", "\"\""),
        ("\"a\\\"b\\\\c\\/d\"", "\"a\\\"b\\\\c/d\""),
        ("\"\\b\\f\\n\\r\\t\\u0001\"", "\"\\b\\f\\n\\r\\t\\u0001\""),
        ("\"\\u00e9\\u20ac\"", "\"é€\""),
        ("\"\\ud83d\\ude00\"", "\"😀\""),
        ("\"árvíztűrő tükörfúrógép\"", "\"árvíztűrő tükörfúrógép\""),
        ("[]", "[]"),
        ("[ 1 , [2, [ ] ], {} ]", "[1, [2, []], {}]"),
        ("{}", "{}"),
        ("{\"b\": 1, \"a\": 2}", "{\"a\": 2, \"b\": 1}"),
        ("{\"a\": 1, \"a\": 2}", "{\"a\": 2}"),
        (
            "{\"aa\": 1, \"b\": 2, \"a\": 3}",
            // Shorter keys first, as `jsonb` keeps them.
            "{\"a\": 3, \"b\": 2, \"aa\": 1}",
        ),
        (
            "{\"x\": {\"z\": [1, {\"q\": null}], \"y\": \"\"}, \"w\": false}",
            "{\"w\": false, \"x\": {\"y\": \"\", \"z\": [1, {\"q\": null}]}}",
        ),
    ] {
        assert_eq!(round(input), want, "{input}");
        // Canonical: the output parses to the same bytes.
        assert_eq!(
            parse::parse(want).expect("output parses"),
            parse::parse(input).expect("parses"),
            "{input}"
        );
    }
}

#[test]
fn equal_documents_equal_bytes() {
    let a = parse::parse("{\"b\": [1, 2.0], \"a\": {\"y\": 1, \"x\": 1e0}}").expect("a");
    let b = parse::parse("{\"a\":{\"x\":1,\"y\":1.000},\"b\":[1,2]}").expect("b");
    assert_eq!(a, b);
}

#[test]
fn malformed_text_is_refused() {
    for s in [
        "",
        " ",
        "{",
        "[1,]",
        "[1 2]",
        "{\"a\"}",
        "{\"a\":}",
        "{a: 1}",
        "{'a': 1}",
        "{\"a\": 1,}",
        "01",
        "1.",
        ".5",
        "1e",
        "+1",
        "-",
        "NaN",
        "Infinity",
        "tru",
        "nul",
        "\"abc",
        "\"a\nb\"",
        "\"\\x\"",
        "\"\\u12\"",
        "\"\\u12g4\"",
        "[1] 2",
        "// c\n1",
        "\"\\ud800\"",
        "\"\\udc00\"",
        "\"\\ud800\\u0041\"",
    ] {
        let e = fails(s);
        assert!(
            e.starts_with("22P02 invalid input syntax for type json"),
            "{s:?}: {e}"
        );
    }
    assert!(fails("\"\\u0000\"").starts_with("22P05"));
    assert!(fails("1e131072").starts_with("22003"));
    assert!(fails("1e-16384").starts_with("22003"));
    assert!(fails("1e99999999999999999999999").starts_with("22003"));
    assert_eq!(round("1e131071").len(), 131_072);
}

#[test]
fn depth_is_bounded() {
    let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
    assert!(parse::parse(&ok).is_ok());
    let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
    assert!(fails(&deep).starts_with("54001"));
    let deep = "{\"a\":".repeat(MAX_DEPTH + 1) + "1" + &"}".repeat(MAX_DEPTH + 1);
    assert!(fails(&deep).starts_with("54001"));
}

#[test]
fn read_in_place() {
    let b = parse::parse(
        "{\"name\": \"Ann\", \"tags\": [\"a\", \"b\", \"c\"], \"age\": 41, \"pi\": 3.14}",
    )
    .expect("parses");
    let Node::Object(o) = form::root(&b).expect("root") else {
        panic!("an object")
    };
    assert_eq!(o.len(), 4);
    assert!(matches!(
        form::root(o.get_bytes("name").expect("ok").expect("name")),
        Ok(Node::Str(b"Ann"))
    ));
    assert!(matches!(
        form::root(o.get_bytes("age").expect("ok").expect("age")),
        Ok(Node::Int(41))
    ));
    assert!(o.get_bytes("nope").expect("ok").is_none());
    let tags = o.get_bytes("tags").expect("ok").expect("tags");
    let Node::Array(a) = form::root(tags).expect("tags") else {
        panic!("an array")
    };
    assert!(matches!(a.get(2), Ok(Node::Str(b"c"))));
    // A value inside is a value of its own.
    assert_eq!(to_text(tags).expect("text"), "[\"a\", \"b\", \"c\"]");
}

#[test]
fn wide_containers() {
    // Offsets of two and four bytes.
    for n in [10usize, 300, 70_000] {
        let text = format!(
            "{{{}}}",
            (0..n)
                .map(|i| format!("\"k{i:06}\": \"{}\"", "v".repeat(i % 7)))
                .collect::<Vec<_>>()
                .join(",")
        );
        let b = parse::parse(&text).expect("parses");
        let Node::Object(o) = form::root(&b).expect("root") else {
            panic!()
        };
        assert_eq!(o.len(), n);
        let k = format!("k{:06}", n - 1);
        assert!(o.get_bytes(&k).expect("ok").is_some());
        form::check(&b).expect("canonical");
        let arr = format!(
            "[{}]",
            (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        );
        let b = parse::parse(&arr).expect("parses");
        let Node::Array(a) = form::root(&b).expect("root") else {
            panic!()
        };
        assert!(matches!(a.get(n - 1), Ok(Node::Int(x)) if x == n as i64 - 1));
    }
}

#[test]
fn integers_take_the_fewest_bytes() {
    for (i, len) in [
        (0i64, 1),
        (1, 2),
        (-1, 2),
        (127, 2),
        (128, 3),
        (-128, 2),
        (-129, 3),
        (i64::MAX, 9),
        (i64::MIN, 9),
    ] {
        let b = parse::parse(&i.to_string()).expect("parses");
        assert_eq!(b.len(), len, "{i}");
        assert!(matches!(form::root(&b), Ok(Node::Int(x)) if x == i));
    }
}

#[test]
fn order_is_postgresql_s() {
    let ordered = [
        "null",
        "\"\"",
        "\"a\"",
        "\"ab\"",
        "\"b\"",
        "-1e300",
        "-100",
        "-1.5",
        "-1",
        "-0.001",
        "0",
        "0.001",
        "1",
        "1.5",
        "2",
        "10",
        "9223372036854775807",
        "9223372036854775808",
        "1e300",
        "false",
        "true",
        "[]",
        "[3]",
        "[1, 2]",
        "[1, 3]",
        "[2, 1]",
        "{}",
        "{\"b\": 1}",
        "{\"a\": 1, \"b\": 1}",
        "{\"a\": 2, \"b\": 1}",
    ];
    let vals: Vec<Vec<u8>> = ordered
        .iter()
        .map(|s| parse::parse(s).expect("parses"))
        .collect();
    for i in 0..vals.len() {
        for j in 0..vals.len() {
            assert_eq!(
                key::compare(&vals[i], &vals[j]).expect("compare"),
                i.cmp(&j),
                "{} vs {}",
                ordered[i],
                ordered[j]
            );
        }
        let k = key::of(&vals[i]).expect("key");
        let (back, used) = key::decode(&k).expect("decodes");
        assert_eq!(used, k.len());
        assert_eq!(back, vals[i], "{}", ordered[i]);
    }
}

#[test]
fn pretty_as_postgresql() {
    let b = parse::parse("{\"a\": 1, \"b\": [1, {}, []], \"c\": {\"d\": null}}").expect("parses");
    assert_eq!(
        to_pretty(&b).expect("pretty"),
        // Empty containers too, as PostgreSQL 18 prints them.
        "{\n    \"a\": 1,\n    \"b\": [\n        1,\n        {\n        },\n        [\n        ]\n    ],\n    \"c\": {\n        \"d\": null\n    }\n}"
    );
}

#[test]
fn malformed_bytes_are_caught() {
    for b in [
        &[][..],
        &[0x31],
        &[0x09],
        &[0x50 | 3, b'a'],
        &[0x60, 5],
        &[0x70, 1, 1, 1, b'a'],
        &[0x31, 0x00],
        &[0x32, 0x01, 0x00],
        // A number with no digits and an exponent far out of bounds.
        &[0x40, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00],
    ] {
        assert!(form::check(b).is_err(), "{b:?}");
    }
    // Not even read: its text would be billions of zeros (found by fuzzing).
    assert!(form::root(&[0x40, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00]).is_err());
    // A key whose number's exponent is i64::MIN (found by fuzzing).
    let mut k = vec![0x03, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x00];
    assert!(key::decode(&k).is_err());
    k[2] = 0xff;
    assert!(key::decode(&k).is_err());
    // Keys out of order.
    let mut b = parse::parse("{\"a\": 1, \"b\": 2}").expect("parses");
    let at = b.iter().position(|x| *x == b'a').expect("a");
    b.swap(at, at + 1);
    assert!(form::check(&b).is_err());
}

#[test]
fn casts_to_scalars() {
    crate::ext::Registry::shared_builtin();
    let j = |s: &str| from_text(s).expect("parses");
    assert_eq!(
        cast_to(&j("true"), DataType::Bool)
            .expect("cast")
            .expect("ok")
            .render(),
        "t"
    );
    assert_eq!(
        cast_to(&j("42"), DataType::Integer)
            .expect("cast")
            .expect("ok")
            .render(),
        "42"
    );
    assert_eq!(
        cast_to(&j("1.25"), DataType::Numeric)
            .expect("cast")
            .expect("ok")
            .render(),
        "1.25"
    );
    assert_eq!(
        cast_to(&j("1.5"), DataType::Integer)
            .expect("cast")
            .expect("ok")
            .render(),
        "2"
    );
    assert_eq!(
        cast_to(&j("1.5"), DataType::Float)
            .expect("cast")
            .expect("ok")
            .render(),
        "1.5"
    );
    let e = cast_to(&j("\"x\""), DataType::Integer)
        .expect("cast")
        .expect_err("refused");
    assert_eq!(e.code, "22023");
    assert_eq!(e.message, "cannot cast json string to type integer");
    assert!(
        cast_to(&j("1e300"), DataType::Numeric)
            .expect("cast")
            .is_err()
    );
    assert!(cast_to(&j("1"), DataType::Date).is_none());
}

#[test]
fn numbers_packed_from_text_are_canonical() {
    // From the text's digits, as the general writer makes them from a
    // digit list: zeros stripped, integers as integers.
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut cases: Vec<String> = [
        "0",
        "0.0",
        "00.000e5",
        "100",
        "100e-2",
        "1.000",
        "0.0001",
        "120.5e-1",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "-9223372036854775809",
        "99999999999999999999",
        "1e18",
        "1e19",
        "123456789012345678e1",
        "0.5e1",
        "5e-1",
        "1e-16383",
        "1e131071",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for _ in 0..20_000 {
        let r = next();
        let int: String = (0..(r % 22))
            .map(|i| char::from(b'0' + ((r >> (i * 2)) % 10) as u8))
            .collect();
        let r2 = next();
        let frac: String = (0..(r2 % 12))
            .map(|i| char::from(b'0' + ((r2 >> (i * 3)) % 10) as u8))
            .collect();
        let exp = (next() % 41) as i64 - 20;
        cases.push(format!(
            "{}{}{}e{exp}",
            if r & 1 == 0 { "" } else { "-" },
            if int.is_empty() { "0" } else { &int },
            if frac.is_empty() {
                String::new()
            } else {
                format!(".{frac}")
            }
        ));
    }
    for s in &cases {
        let (neg, body) = s
            .strip_prefix('-')
            .map_or((false, s.as_str()), |b| (true, b));
        let (mant, exp) = body
            .split_once('e')
            .map_or((body, 0), |(m, e)| (m, e.parse().unwrap()));
        let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
        let mut a = Vec::new();
        let ra = form::put_ascii_number(&mut a, neg, int.as_bytes(), frac.as_bytes(), exp);
        let digits: Vec<u8> = int.bytes().chain(frac.bytes()).map(|d| d - b'0').collect();
        let mut b = Vec::new();
        let rb = form::put_number(&mut b, neg, &digits, exp - frac.len() as i64);
        assert_eq!(ra.is_ok(), rb.is_ok(), "{s}");
        assert_eq!(a, b, "{s}");
    }
}
