//! JSON Schema checks measured (PLAN.md §M14.15 J5): nucleus's
//! `json::schema` against the `jsonschema` crate, on the same documents —
//! made-up tool calls, one in ten not fitting — each already parsed
//! (nucleus's binary form, serde_json's `Value`), the schema compiled
//! once.  Whether each fits, and every problem's place (the errors).
//! Each figure is the median of `ROUNDS` timed runs.
//!
//!     cargo run --release --bin schema [-- <documents>]

use std::time::Instant;

use nucleus_sql::json::{parse, schema::Schema};

const ROUNDS: usize = 15;

fn median(mut f: impl FnMut()) -> f64 {
    f();
    let mut t: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let s = Instant::now();
            f();
            s.elapsed().as_secs_f64()
        })
        .collect();
    t.sort_by(f64::total_cmp);
    t[ROUNDS / 2]
}

const SCHEMA: &str = r##"{
  "$defs": {"place": {"type": "object", "properties": {
      "city": {"type": "string", "minLength": 1, "maxLength": 80},
      "country": {"type": "string", "pattern": "^[A-Z]{2}$"}},
    "required": ["city"], "additionalProperties": false}},
  "type": "object",
  "properties": {
    "place": {"$ref": "#/$defs/place"},
    "unit": {"enum": ["celsius", "fahrenheit"]},
    "days": {"type": "integer", "minimum": 1, "maximum": 14},
    "hours": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 23},
              "uniqueItems": true, "maxItems": 24},
    "detail": {"anyOf": [{"const": "brief"}, {"type": "object", "properties": {
        "wind": {"type": "boolean"}, "rain": {"type": "boolean"}}}]}
  },
  "required": ["place", "unit"],
  "additionalProperties": false
}"##;

fn document(i: usize) -> String {
    let unit = if i % 10 == 7 { "kelvin" } else if i % 2 == 0 { "celsius" } else { "fahrenheit" };
    let hours: Vec<String> = (0..i % 6).map(|h| ((h * 5 + i) % 24).to_string()).collect();
    let detail = if i % 3 == 0 {
        r#""brief""#.to_string()
    } else {
        format!(r#"{{"wind": {}, "rain": true}}"#, i % 2 == 0)
    };
    format!(
        r#"{{"place": {{"city": "city{}", "country": "HU"}}, "unit": "{unit}", "days": {}, "hours": [{}], "detail": {detail}}}"#,
        i % 5000,
        i % 14 + 1,
        hours.join(", ")
    )
}

fn main() {
    let n: usize = std::env::args().nth(1).map_or(100_000, |a| a.parse().expect("a count"));
    let texts: Vec<String> = (0..n).map(document).collect();
    let ours: Vec<Vec<u8>> = texts.iter().map(|t| parse::parse(t).unwrap()).collect();
    let theirs: Vec<serde_json::Value> = texts.iter().map(|t| serde_json::from_str(t).unwrap()).collect();
    let s_ours = Schema::compile(&parse::parse(SCHEMA).unwrap()).unwrap();
    let s_theirs = jsonschema::validator_for(&serde_json::from_str(SCHEMA).unwrap()).unwrap();

    // The same answers.
    let fit_ours = ours.iter().filter(|d| s_ours.matches(d).unwrap()).count();
    let fit_theirs = theirs.iter().filter(|d| s_theirs.is_valid(d)).count();
    assert_eq!(fit_ours, fit_theirs);
    let errs_ours: usize = ours.iter().map(|d| s_ours.errors(d).unwrap().len()).sum();
    let errs_theirs: usize = theirs.iter().map(|d| s_theirs.iter_errors(d).count()).sum();

    let m_ours = median(|| {
        std::hint::black_box(ours.iter().filter(|d| s_ours.matches(d).unwrap()).count());
    });
    let m_theirs = median(|| {
        std::hint::black_box(theirs.iter().filter(|d| s_theirs.is_valid(d)).count());
    });
    let e_ours = median(|| {
        std::hint::black_box(ours.iter().map(|d| s_ours.errors(d).unwrap().len()).sum::<usize>());
    });
    let e_theirs = median(|| {
        std::hint::black_box(
            theirs.iter().map(|d| s_theirs.iter_errors(d).map(|e| e.to_string().len()).sum::<usize>()).sum::<usize>(),
        );
    });
    let c_ours = median(|| {
        std::hint::black_box(Schema::compile(&parse::parse(SCHEMA).unwrap()).unwrap());
    });
    let c_theirs = median(|| {
        std::hint::black_box(jsonschema::validator_for(&serde_json::from_str(SCHEMA).unwrap()).unwrap());
    });
    let schema_bytes = parse::parse(SCHEMA).unwrap();
    let cached = median(|| {
        for _ in 0..1000 {
            std::hint::black_box(nucleus_sql::json::schema::compiled(std::hint::black_box(&schema_bytes)).unwrap());
        }
    }) / 1000.0;
    println!("{n} documents, {fit_ours} fit; errors: nucleus {errs_ours}, jsonschema {errs_theirs}");
    println!("{:<22} {:>10} {:>12}", "", "nucleus", "jsonschema");
    let per = |t: f64| t / n as f64 * 1e9;
    println!("{:<22} {:>8.0}ns {:>10.0}ns", "fits (a document)", per(m_ours), per(m_theirs));
    println!("{:<22} {:>8.0}ns {:>10.0}ns", "errors (a document)", per(e_ours), per(e_theirs));
    println!("{:<22} {:>8.1}µs {:>10.1}µs", "compile (the schema)", c_ours * 1e6, c_theirs * 1e6);
    println!("{:<22} {:>8.0}ns", "compiled, kept", cached * 1e9);
}
