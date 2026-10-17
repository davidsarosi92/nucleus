//! SQL tokens.

use crate::error::{Error, Result};

/// A token.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// An unquoted identifier or keyword, lowercased.
    Word(String),
    /// A `"quoted"` identifier, case kept.
    Quoted(String),
    /// A `'string'` literal.
    Str(String),
    /// A number literal, as written.
    Number(String),
    /// `$n`.
    Param(usize),
    /// Punctuation or an operator: `( ) [ ] , ; . * + - / % = <> != < <= >
    /// >= || :: ~ ~* !~ !~*`.
    Op(&'static str),
    /// Any other operator (`<->`, `@>`): one a module or `ALTER TYPE …
    /// ADD OPERATOR` brings; the grammar has one place for them.
    UserOp(String),
}

impl Token {
    /// Whether this is the keyword `kw` (lowercase).
    pub fn is(&self, kw: &str) -> bool {
        matches!(self, Token::Word(w) if w == kw)
    }
}

/// The characters operators are made of (PostgreSQL's).
const OP_CHARS: &[u8] = b"+-*/<>=~!@#%^&|`?";

/// Longest first, so `<=` is not read as `<` `=`.
const OPS: &[&str] = &[
    "!~*", "<>", "!~", "!=", "~*", "<=", ">=", "||", "::", "(", ")", "[", "]", ",", ";", ".", "*",
    "+", "-", "/", "%", "=", "<", ">", "~",
];

/// The body of an `E'…'` string from just after its opening quote: C-style
/// backslash escapes.  Returns the string and the index after the closing
/// quote.
fn escape_string(sql: &str, mut i: usize) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut s = String::new();
    loop {
        let Some(c) = sql[i..].chars().next() else {
            return Err(Error::syntax("unterminated string literal"));
        };
        match c {
            '\'' if b.get(i + 1) == Some(&b'\'') => {
                s.push('\'');
                i += 2;
            }
            '\'' => return Ok((s, i + 1)),
            '\\' => {
                let e = sql[i + 1..]
                    .chars()
                    .next()
                    .ok_or_else(|| Error::syntax("unterminated string literal"))?;
                i += 1 + e.len_utf8();
                s.push(match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    other => other,
                });
            }
            c => {
                s.push(c);
                i += c.len_utf8();
            }
        }
    }
}

/// Splits `sql` into tokens, each with its byte offset.
pub fn tokenize(sql: &str) -> Result<Vec<(Token, usize)>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if sql[i..].starts_with("--") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if sql[i..].starts_with("/*") {
            let end = sql[i + 2..]
                .find("*/")
                .ok_or_else(|| Error::syntax("unterminated /* comment"))?;
            i += end + 4;
        } else if (c == b'e' || c == b'E') && b.get(i + 1) == Some(&b'\'') {
            let (s, end) = escape_string(sql, i + 2)?;
            out.push((Token::Str(s), i));
            i = end;
        } else if c == b'\'' {
            let start = i;
            let mut s = String::new();
            i += 1;
            loop {
                match sql[i..].find('\'') {
                    None => return Err(Error::syntax("unterminated string literal")),
                    Some(j) => {
                        s.push_str(&sql[i..i + j]);
                        i += j + 1;
                        if b.get(i) == Some(&b'\'') {
                            s.push('\'');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            out.push((Token::Str(s), start));
        } else if c == b'"' {
            let start = i;
            let mut s = String::new();
            i += 1;
            loop {
                match sql[i..].find('"') {
                    None => return Err(Error::syntax("unterminated quoted identifier")),
                    Some(j) => {
                        s.push_str(&sql[i..i + j]);
                        i += j + 1;
                        if b.get(i) == Some(&b'"') {
                            s.push('"');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            out.push((Token::Quoted(s), start));
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    i = j;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            out.push((Token::Number(sql[start..i].to_string()), start));
        } else if c == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let start = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n = sql[start + 1..i]
                .parse()
                .map_err(|_| Error::syntax("bad parameter number"))?;
            out.push((Token::Param(n), start));
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            out.push((Token::Word(sql[start..i].to_lowercase()), start));
        } else if OP_CHARS.contains(&c) {
            // PostgreSQL's rule: the longest run of operator characters —
            // not into a comment — and a run of more than one does not end
            // in + or - unless it holds one of ~ ! @ # % ^ & | ` ?.
            let mut end = i;
            while end < b.len() && OP_CHARS.contains(&b[end]) {
                if end > i && (sql[end..].starts_with("--") || sql[end..].starts_with("/*")) {
                    break;
                }
                end += 1;
            }
            let special = |s: &str| s.bytes().any(|x| b"~!@#%^&|`?".contains(&x));
            while end - i > 1 && matches!(b[end - 1], b'+' | b'-') && !special(&sql[i..end]) {
                end -= 1;
            }
            let run = &sql[i..end];
            match OPS.iter().find(|op| **op == run) {
                Some(op) => out.push((Token::Op(if *op == "!=" { "<>" } else { op }), i)),
                None => out.push((Token::UserOp(run.to_string()), i)),
            }
            i = end;
        } else if let Some(op) = OPS.iter().find(|op| sql[i..].starts_with(**op)) {
            out.push((Token::Op(op), i));
            i += op.len();
        } else {
            let ch = sql[i..].chars().next().unwrap_or('?');
            return Err(Error::syntax(format!(
                "unexpected character {ch:?} at position {}",
                i + 1
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Token> {
        tokenize(s).unwrap().into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn basics() {
        assert_eq!(
            toks("SELECT a, \"B c\" FROM t WHERE x <= 1.5e3 AND y != 'it''s' -- comment\n;"),
            vec![
                Token::Word("select".into()),
                Token::Word("a".into()),
                Token::Op(","),
                Token::Quoted("B c".into()),
                Token::Word("from".into()),
                Token::Word("t".into()),
                Token::Word("where".into()),
                Token::Word("x".into()),
                Token::Op("<="),
                Token::Number("1.5e3".into()),
                Token::Word("and".into()),
                Token::Word("y".into()),
                Token::Op("<>"),
                Token::Str("it's".into()),
                Token::Op(";"),
            ]
        );
        assert_eq!(
            toks("$1::text || .5 /* c */ >"),
            vec![
                Token::Param(1),
                Token::Op("::"),
                Token::Word("text".into()),
                Token::Op("||"),
                Token::Number(".5".into()),
                Token::Op(">"),
            ]
        );
        assert!(tokenize("'open").is_err());
        assert!(tokenize("a { b").is_err());
    }
}

#[cfg(test)]
mod operator_tests {
    use super::*;

    fn ops(sql: &str) -> Vec<String> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .filter_map(|(t, _)| match t {
                Token::Op(o) => Some(o.to_string()),
                Token::UserOp(o) => Some(format!("user {o}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn operators_as_postgresql_reads_them() {
        assert_eq!(ops("a=-1"), ["=", "-"]);
        assert_eq!(ops("a<>-1"), ["<>", "-"]);
        assert_eq!(ops("a*-1"), ["*", "-"]);
        assert_eq!(ops("a<-1"), ["<", "-"]);
        assert_eq!(ops("a!=b"), ["<>"]);
        assert_eq!(ops("a<->b"), ["user <->"]);
        assert_eq!(ops("a@>b"), ["user @>"]);
        assert_eq!(ops("a@-b"), ["user @-"]);
        assert_eq!(ops("a<=>b"), ["user <=>"]);
        assert_eq!(ops("a !~* b"), ["!~*"]);
        assert_eq!(ops("a--comment"), Vec::<String>::new());
        assert_eq!(ops("a+/*c*/b"), ["+"]);
    }
}
