//! `db.<collection>.<method>(…)` lines in ncsql (PLAN.md §M16 Q4): the
//! requests MongoDB's shell writes — `find`, `findOne`, `countDocuments`,
//! `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`,
//! `deleteOne`, `deleteMany`, with `.sort()`, `.skip()`, `.limit()` after
//! a `find` — compiled by `nucleus_sql::documents` to the statement a
//! driver's request is.  Arguments are JSON or the shell's relaxed form
//! (unquoted keys, single quotes, trailing commas), kept in their order
//! (a sort document's fields are).

use nucleus_sql::documents::{Op, Request};

/// Whether `text` is such a line (it may not be whole yet).
pub(crate) fn is_request(text: &str) -> bool {
    text.trim_start().starts_with("db.")
}

/// Whether `text` holds a whole request: its brackets closed, its strings
/// ended, and it ends with `)` (or `);`).
pub(crate) fn complete(text: &str) -> bool {
    let (mut depth, mut quote, mut escaped) = (0i32, None::<char>, false);
    for ch in text.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    let t = text.trim_end().trim_end_matches(';').trim_end();
    quote.is_none() && depth <= 0 && t.ends_with(')')
}

/// The request a line is.
pub(crate) fn parse(text: &str) -> Result<Request, String> {
    let t = text.trim().trim_end_matches(';').trim_end();
    let rest = t.strip_prefix("db.").ok_or("a request begins with db.")?;
    let calls = calls(rest)?;
    let Some(((collection, _), calls)) = calls.split_first() else {
        return Err("db.<collection>.<method>(…)".into());
    };
    let Some(((method, args), chained)) = calls.split_first() else {
        return Err(format!("db.{collection}.<method>(…): which method?"));
    };
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let options = |i: usize| -> Result<Vec<(String, String)>, String> {
        match args.get(i) {
            Some(o) => fields(o),
            None => Ok(Vec::new()),
        }
    };
    let mut r = Request {
        collection: collection.clone(),
        ..Request::default()
    };
    match method.as_str() {
        "find" | "findOne" => {
            r.op = Some(Op::Find);
            r.filter = arg(0);
            r.body = arg(1);
            if method == "findOne" {
                r.limit = 1;
            }
        }
        "countDocuments" | "count" => {
            r.op = Some(Op::Count);
            r.filter = arg(0);
        }
        "insertOne" | "insertMany" | "insert" => {
            r.op = Some(Op::Insert);
            r.body = arg(0);
        }
        "updateOne" | "updateMany" | "replaceOne" => {
            r.op = Some(if method == "replaceOne" {
                Op::Replace
            } else {
                Op::Update
            });
            r.filter = arg(0);
            r.body = arg(1);
            r.many = method == "updateMany";
            for (k, v) in options(2)? {
                if k == "upsert" {
                    r.upsert = v == "true";
                }
            }
        }
        "deleteOne" | "deleteMany" => {
            r.op = Some(Op::Delete);
            r.filter = arg(0);
            r.many = method == "deleteMany";
        }
        m => return Err(format!("db.{collection}.{m}: not a request ncsql knows")),
    }
    for (name, args) in chained {
        if r.op != Some(Op::Find) {
            return Err(format!(".{name}() follows a find"));
        }
        let n = || -> Result<u64, String> {
            args.first()
                .and_then(|a| a.trim().parse().ok())
                .ok_or_else(|| format!(".{name}() takes a number"))
        };
        match name.as_str() {
            "sort" => r.sort = args.first().cloned().unwrap_or_default(),
            "skip" => r.skip = n()?,
            "limit" => r.limit = n()?,
            "toArray" | "pretty" => {}
            m => return Err(format!(".{m}() is not known after a find")),
        }
    }
    Ok(r)
}

/// `name(args).name(args)…` after `db.`: the collection (no arguments)
/// first, each call's arguments as JSON texts.
fn calls(s: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    // The collection: up to the first `.`
    let start = i;
    while i < b.len() && b[i] != '.' {
        i += 1;
    }
    let collection: String = b[start..i].iter().collect();
    if collection.is_empty() {
        return Err("db.<collection>".into());
    }
    out.push((collection, Vec::new()));
    while i < b.len() {
        if b[i] != '.' {
            return Err(format!("unexpected '{}'", b[i]));
        }
        i += 1;
        let start = i;
        while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
            i += 1;
        }
        let name: String = b[start..i].iter().collect();
        if i >= b.len() || b[i] != '(' {
            return Err(format!(".{name}: '(' expected"));
        }
        // The arguments: to the matching ')'.
        let open = i;
        let (mut depth, mut quote, mut escaped) = (0i32, None::<char>, false);
        while i < b.len() {
            let ch = b[i];
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    quote = None;
                }
            } else {
                match ch {
                    '"' | '\'' => quote = Some(ch),
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if i >= b.len() {
            return Err(format!(".{name}(: ')' missing"));
        }
        let inner: String = b[open + 1..i].iter().collect();
        i += 1;
        out.push((name, arguments(&inner)?));
    }
    Ok(out)
}

/// The top-level arguments, each as JSON.
fn arguments(s: &str) -> Result<Vec<String>, String> {
    let toks = tokens(s)?;
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut depth = 0;
    for t in toks {
        match t.as_str() {
            "{" | "[" => depth += 1,
            "}" | "]" => depth -= 1,
            _ => {}
        }
        if t == "," && depth == 0 {
            out.push(json(&cur));
            cur.clear();
        } else {
            cur.push(t);
        }
    }
    if !cur.is_empty() {
        out.push(json(&cur));
    }
    Ok(out)
}

/// An object argument's fields, each value as JSON (an options object).
fn fields(s: &str) -> Result<Vec<(String, String)>, String> {
    let toks = tokens(s)?;
    let mut out = Vec::new();
    let mut i = 1;
    while i + 2 < toks.len() {
        let key = toks[i].trim_matches('"').to_string();
        let mut depth = 0;
        let mut j = i + 2;
        let start = j;
        while j < toks.len() - 1 {
            match toks[j].as_str() {
                "{" | "[" => depth += 1,
                "}" | "]" => depth -= 1,
                "," if depth == 0 => break,
                _ => {}
            }
            j += 1;
        }
        out.push((key, json(&toks[start..j])));
        i = j + 1;
    }
    Ok(out)
}

/// Tokens as JSON: strings double-quoted, an unquoted key quoted,
/// trailing commas dropped.
fn tokens(s: &str) -> Result<Vec<String>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let ch = b[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let mut text = String::new();
            i += 1;
            while i < b.len() && b[i] != ch {
                if b[i] == '\\' && i + 1 < b.len() {
                    // An escape: kept, but a quote of the other kind.
                    if b[i + 1] == '\'' {
                        text.push('\'');
                    } else {
                        text.push('\\');
                        text.push(b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                if b[i] == '"' {
                    text.push_str("\\\"");
                } else {
                    text.push(b[i]);
                }
                i += 1;
            }
            if i >= b.len() {
                return Err("a string is not ended".into());
            }
            i += 1;
            out.push(format!("\"{text}\""));
            continue;
        }
        if "{}[],:".contains(ch) {
            // A trailing comma: dropped.
            if (ch == '}' || ch == ']') && out.last().is_some_and(|t| t == ",") {
                out.pop();
            }
            out.push(ch.to_string());
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && !b[i].is_whitespace() && !"{}[],:\"'".contains(b[i]) {
            i += 1;
        }
        let word: String = b[start..i].iter().collect();
        // A key: followed by ':'.
        let mut j = i;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == ':' {
            out.push(format!("\"{word}\""));
        } else {
            out.push(word);
        }
    }
    Ok(out)
}

fn json(toks: &[String]) -> String {
    let mut s = String::new();
    for t in toks {
        s.push_str(t);
        if t == ":" || t == "," {
            s.push(' ');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shells_lines_are_requests() {
        let r = parse("db.people.find({age: {$gt: 30}, 'name': \"ann\"}, {name: 1}).sort({age: -1, name: 1}).skip(2).limit(5)").unwrap();
        assert_eq!(r.op, Some(Op::Find));
        assert_eq!(r.collection, "people");
        assert_eq!(r.filter, r#"{"age": {"$gt": 30}, "name": "ann"}"#);
        assert_eq!(r.body, r#"{"name": 1}"#);
        assert_eq!(r.sort, r#"{"age": -1, "name": 1}"#);
        assert_eq!((r.skip, r.limit), (2, 5));
        let r = parse("db.c.updateMany({}, {$inc: {n: 1},}, {upsert: true});").unwrap();
        assert_eq!((r.op, r.many, r.upsert), (Some(Op::Update), true, true));
        assert_eq!(r.body, r#"{"$inc": {"n": 1}}"#);
        assert!(parse("db.c.drop()").is_err());
        assert!(parse("db.c.count().limit(1)").is_err());
        assert!(complete("db.c.find({a: ')'})"));
        assert!(!complete("db.c.find({a: 1}"));
    }
}
