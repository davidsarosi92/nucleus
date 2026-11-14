//! Path text to [`Path`]: PostgreSQL's `jsonpath` grammar, one token of
//! look-ahead, recursive descent.  What PostgreSQL refuses while it reads
//! a path — `@` outside a filter, `last` outside a subscript, an unknown
//! `like_regex` flag, a regular expression that does not compile — is
//! refused here, with its SQLSTATE.

use super::ast::*;
use crate::decimal::Decimal;
use crate::error::{Error, Result};

/// Parses a path.
pub fn parse(text: &str) -> Result<Path> {
    let toks = Lexer {
        text,
        b: text.as_bytes(),
        at: 0,
    }
    .tokens()?;
    let mut p = Parser {
        text,
        toks,
        i: 0,
        filters: 0,
        subscripts: 0,
    };
    let strict = if p.keyword("strict") {
        p.i += 1;
        true
    } else {
        if p.keyword("lax") {
            p.i += 1;
        }
        false
    };
    let expr = p.or()?;
    p.expect(Tok::End)?;
    Ok(Path { strict, expr })
}

fn syntax(message: impl Into<String>) -> Error {
    Error::syntax(message)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Dollar,
    Var(String),
    At,
    Str(String),
    /// A number, and whether it was written as an integer.
    Num(Decimal, bool),
    Ident(String),
    Dot,
    Star,
    StarStar,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Question,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    End,
}

struct Token {
    tok: Tok,
    at: usize,
    end: usize,
}

/// Bytes that end an identifier.
fn special(c: u8) -> bool {
    matches!(
        c,
        b'?' | b'%'
            | b'$'
            | b'.'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'('
            | b')'
            | b'|'
            | b'&'
            | b'!'
            | b'='
            | b'<'
            | b'>'
            | b'@'
            | b'#'
            | b','
            | b'*'
            | b':'
            | b'-'
            | b'+'
            | b'/'
            | b'\\'
            | b'"'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b'\x0c'
    )
}

struct Lexer<'a> {
    text: &'a str,
    b: &'a [u8],
    at: usize,
}

impl Lexer<'_> {
    fn near(&self, from: usize, to: usize) -> Error {
        let to = to.min(self.b.len());
        let mut to = to.max(from + 1).min(self.b.len());
        while !self.text.is_char_boundary(to) {
            to += 1;
        }
        syntax(format!(
            "syntax error at or near \"{}\" of jsonpath input",
            &self.text[from..to]
        ))
    }

    fn tokens(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            self.space()?;
            let at = self.at;
            let tok = self.token()?;
            let end = tok == Tok::End;
            out.push(Token {
                tok,
                at,
                end: self.at,
            });
            if end {
                return Ok(out);
            }
        }
    }

    fn space(&mut self) -> Result<()> {
        loop {
            match self.b.get(self.at) {
                Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') => self.at += 1,
                Some(b'/') if self.b.get(self.at + 1) == Some(&b'*') => {
                    match self.text[self.at + 2..].find("*/") {
                        Some(i) => self.at += 2 + i + 2,
                        None => return Err(syntax("unexpected end of comment")),
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn ident_end(&self, from: usize) -> usize {
        let mut i = from;
        while i < self.b.len() && !special(self.b[i]) {
            i += 1;
        }
        i
    }

    fn token(&mut self) -> Result<Tok> {
        let b = self.b;
        let at = self.at;
        let Some(&c) = b.get(at) else {
            return Ok(Tok::End);
        };
        let next = b.get(at + 1).copied();
        let two = |t: Tok, s: &mut Self| {
            s.at += 2;
            Ok(t)
        };
        let one = |t: Tok, s: &mut Self| {
            s.at += 1;
            Ok(t)
        };
        match c {
            b'$' => {
                self.at += 1;
                match next {
                    Some(b'"') => Ok(Tok::Var(self.string()?)),
                    Some(n) if !special(n) => {
                        let end = self.ident_end(self.at);
                        let name = self.text[self.at..end].to_string();
                        self.at = end;
                        Ok(Tok::Var(name))
                    }
                    _ => Ok(Tok::Dollar),
                }
            }
            b'@' => one(Tok::At, self),
            b'"' => Ok(Tok::Str(self.string()?)),
            b'0'..=b'9' => self.number(),
            b'.' if matches!(next, Some(b'0'..=b'9')) => self.number(),
            b'.' => one(Tok::Dot, self),
            b'*' if next == Some(b'*') => two(Tok::StarStar, self),
            b'*' => one(Tok::Star, self),
            b'[' => one(Tok::LBracket, self),
            b']' => one(Tok::RBracket, self),
            b'{' => one(Tok::LBrace, self),
            b'}' => one(Tok::RBrace, self),
            b'(' => one(Tok::LParen, self),
            b')' => one(Tok::RParen, self),
            b',' => one(Tok::Comma, self),
            b'?' => one(Tok::Question, self),
            b'+' => one(Tok::Plus, self),
            b'-' => one(Tok::Minus, self),
            b'/' => one(Tok::Slash, self),
            b'%' => one(Tok::Percent, self),
            b'=' if next == Some(b'=') => two(Tok::Eq, self),
            b'!' if next == Some(b'=') => two(Tok::Ne, self),
            b'!' => one(Tok::Not, self),
            b'<' if next == Some(b'>') => two(Tok::Ne, self),
            b'<' if next == Some(b'=') => two(Tok::Le, self),
            b'<' => one(Tok::Lt, self),
            b'>' if next == Some(b'=') => two(Tok::Ge, self),
            b'>' => one(Tok::Gt, self),
            b'&' if next == Some(b'&') => two(Tok::And, self),
            b'|' if next == Some(b'|') => two(Tok::Or, self),
            c if !special(c) => {
                let end = self.ident_end(at);
                self.at = end;
                Ok(Tok::Ident(self.text[at..end].to_string()))
            }
            _ => Err(self.near(at, at + 1)),
        }
    }

    /// A quoted string, from its `"`.
    fn string(&mut self) -> Result<String> {
        let b = self.b;
        self.at += 1;
        let mut s = String::new();
        loop {
            let run = self.at;
            while self.at < b.len() && b[self.at] != b'"' && b[self.at] != b'\\' {
                self.at += 1;
            }
            s.push_str(&self.text[run..self.at]);
            match b.get(self.at) {
                None => return Err(syntax("unexpected end of quoted string")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(s);
                }
                _ => {}
            }
            // A backslash.
            self.at += 1;
            let Some(&e) = b.get(self.at) else {
                return Err(syntax("unexpected end of quoted string"));
            };
            self.at += 1;
            match e {
                b'b' => s.push('\u{8}'),
                b'f' => s.push('\u{c}'),
                b'n' => s.push('\n'),
                b'r' => s.push('\r'),
                b't' => s.push('\t'),
                b'v' => s.push('\u{b}'),
                b'x' => {
                    let start = self.at;
                    while self.at < b.len() && self.at - start < 2 && b[self.at].is_ascii_hexdigit()
                    {
                        self.at += 1;
                    }
                    if self.at - start != 2 {
                        return Err(syntax("invalid hexadecimal character sequence"));
                    }
                    let v = u32::from_str_radix(&self.text[start..self.at], 16).unwrap_or(0);
                    s.push(self.unicode_char(v)?);
                }
                b'u' => {
                    let c = self.unicode()?;
                    s.push(c);
                }
                _ => {
                    // Any other character stands for itself.
                    self.at -= 1;
                    let c = self.text[self.at..].chars().next().expect("a character");
                    self.at += c.len_utf8();
                    s.push(c);
                }
            }
        }
    }

    fn hex(&mut self, min: usize, max: usize) -> Result<u32> {
        let start = self.at;
        while self.at < self.b.len() && self.at - start < max && self.b[self.at].is_ascii_hexdigit()
        {
            self.at += 1;
        }
        if self.at - start < min {
            return Err(syntax("invalid Unicode escape sequence"));
        }
        Ok(u32::from_str_radix(&self.text[start..self.at], 16).unwrap_or(u32::MAX))
    }

    fn unicode_char(&self, v: u32) -> Result<char> {
        if v == 0 {
            return Err(Error::untranslatable(
                "unsupported Unicode escape sequence: \\u0000 cannot be converted to text",
            ));
        }
        char::from_u32(v).ok_or_else(|| syntax("invalid Unicode escape value"))
    }

    /// `\uXXXX` (a surrogate pair joined) or `\u{X…}`, after the `u`.
    fn unicode(&mut self) -> Result<char> {
        let v = if self.b.get(self.at) == Some(&b'{') {
            self.at += 1;
            let v = self.hex(1, 6)?;
            if self.b.get(self.at) != Some(&b'}') {
                return Err(syntax("invalid Unicode escape sequence"));
            }
            self.at += 1;
            v
        } else {
            self.hex(4, 4)?
        };
        match v {
            0xd800..=0xdbff => {
                if !self.b[self.at..].starts_with(b"\\u") {
                    return Err(syntax("invalid Unicode surrogate pair"));
                }
                self.at += 2;
                let lo = self.hex(4, 4)?;
                if !(0xdc00..=0xdfff).contains(&lo) {
                    return Err(syntax("invalid Unicode surrogate pair"));
                }
                self.unicode_char(0x10000 + ((v - 0xd800) << 10) + (lo - 0xdc00))
            }
            0xdc00..=0xdfff => Err(syntax("invalid Unicode surrogate pair")),
            v => self.unicode_char(v),
        }
    }

    /// Digits (`_` allowed between two), from `self.at`.
    fn digits(&mut self, ok: impl Fn(u8) -> bool) -> usize {
        let b = self.b;
        let start = self.at;
        while self.at < b.len() {
            let separator = b[self.at] == b'_'
                && self.at > start
                && self.at + 1 < b.len()
                && ok(b[self.at + 1]);
            if ok(b[self.at]) || separator {
                self.at += 1;
            } else {
                break;
            }
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Tok> {
        let b = self.b;
        let start = self.at;
        let junk = |s: &Self, end: usize| {
            let mut end = (end + 1).min(b.len());
            while !s.text.is_char_boundary(end) {
                end += 1;
            }
            syntax(format!(
                "trailing junk after numeric literal at or near \"{}\" of jsonpath input",
                &s.text[start..end]
            ))
        };
        // 0x…, 0o…, 0b…
        if b[start] == b'0'
            && let Some(radix) = match b.get(start + 1) {
                Some(b'x' | b'X') => Some(16),
                Some(b'o' | b'O') => Some(8),
                Some(b'b' | b'B') => Some(2),
                _ => None,
            }
        {
            self.at += 2;
            let n = self.digits(|c| (c as char).is_digit(radix));
            if n == 0 || self.at < b.len() && !special(b[self.at]) {
                return Err(junk(self, self.at));
            }
            let digits: String = self.text[start + 2..self.at]
                .chars()
                .filter(|&c| c != '_')
                .collect();
            let v = u128::from_str_radix(&digits, radix)
                .ok()
                .and_then(|v| i128::try_from(v).ok())
                .ok_or_else(|| Error::out_of_range("value overflows numeric format"))?;
            return Ok(Tok::Num(Decimal::new(v, 0)?, true));
        }
        let mut int = true;
        if b[start] == b'0' {
            self.at += 1;
        } else if b[start] != b'.' {
            self.digits(|c| c.is_ascii_digit());
        }
        if b.get(self.at) == Some(&b'.') {
            int = false;
            self.at += 1;
            let n = self.digits(|c| c.is_ascii_digit());
            if n == 0 && self.at == start + 1 {
                return Err(junk(self, self.at));
            }
        }
        if let Some(b'e' | b'E') = b.get(self.at) {
            int = false;
            self.at += 1;
            if let Some(b'+' | b'-') = b.get(self.at) {
                self.at += 1;
            }
            if self.digits(|c| c.is_ascii_digit()) == 0 {
                return Err(junk(self, self.at - 1));
            }
        }
        if self.at < b.len() && !special(b[self.at]) {
            return Err(junk(self, self.at));
        }
        let text: String = self.text[start..self.at]
            .chars()
            .filter(|&c| c != '_')
            .collect();
        Ok(Tok::Num(Decimal::parse(&text)?, int))
    }
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Token>,
    i: usize,
    /// Filters around the current point (`@` is allowed in one).
    filters: usize,
    /// Array subscripts around it (`last` is allowed in one).
    subscripts: usize,
}

const METHODS: &[&str] = &[
    "type",
    "size",
    "double",
    "ceiling",
    "floor",
    "abs",
    "keyvalue",
    "bigint",
    "boolean",
    "integer",
    "number",
    "string",
    "decimal",
    "datetime",
    "date",
    "time",
    "time_tz",
    "timestamp",
    "timestamp_tz",
];

impl Parser<'_> {
    fn tok(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek(&self, n: usize) -> &Tok {
        &self.toks[(self.i + n).min(self.toks.len() - 1)].tok
    }

    fn unexpected(&self) -> Error {
        let t = &self.toks[self.i];
        if t.tok == Tok::End {
            return syntax("syntax error at end of jsonpath input");
        }
        syntax(format!(
            "syntax error at or near \"{}\" of jsonpath input",
            &self.text[t.at..t.end]
        ))
    }

    fn expect(&mut self, t: Tok) -> Result<()> {
        if *self.tok() == t {
            self.i += 1;
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    /// Whether the token is the keyword `word` (in any case; `true`,
    /// `false` and `null` only in lower case).
    fn keyword_at(&self, n: usize, word: &str) -> bool {
        match self.peek(n) {
            Tok::Ident(s) => s.eq_ignore_ascii_case(word),
            _ => false,
        }
    }

    fn keyword(&self, word: &str) -> bool {
        self.keyword_at(0, word)
    }

    fn expect_keyword(&mut self, word: &str) -> Result<()> {
        if self.keyword(word) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    /// A predicate where one must be; `at` is the token to blame.
    fn predicate(&self, e: Expr, at: usize) -> Result<Expr> {
        if e.is_predicate() {
            Ok(e)
        } else {
            Err(self.blame(at))
        }
    }

    /// An expression (not a predicate) where one must be.
    fn expression(&self, e: Expr, at: usize) -> Result<Expr> {
        if e.is_predicate() {
            Err(self.blame(at))
        } else {
            Ok(e)
        }
    }

    fn blame(&self, at: usize) -> Error {
        let t = &self.toks[at.min(self.toks.len() - 1)];
        if t.tok == Tok::End {
            return syntax("syntax error at end of jsonpath input");
        }
        syntax(format!(
            "syntax error at or near \"{}\" of jsonpath input",
            &self.text[t.at..t.end]
        ))
    }

    fn or(&mut self) -> Result<Expr> {
        let mut l = self.and()?;
        while *self.tok() == Tok::Or {
            let at = self.i;
            l = self.predicate(l, at)?;
            self.i += 1;
            let r = self.and()?;
            let r = self.predicate(r, self.i)?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut l = self.not()?;
        while *self.tok() == Tok::And {
            let at = self.i;
            l = self.predicate(l, at)?;
            self.i += 1;
            let r = self.not()?;
            let r = self.predicate(r, self.i)?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not(&mut self) -> Result<Expr> {
        if *self.tok() != Tok::Not {
            return self.comparison();
        }
        self.i += 1;
        // `!` takes a parenthesized predicate or `exists(…)`.
        if self.keyword("exists") {
            return Ok(Expr::Not(Box::new(self.exists()?)));
        }
        let at = self.i;
        self.expect(Tok::LParen)?;
        let p = self.or()?;
        let p = self.predicate(p, at + 1)?;
        self.expect(Tok::RParen)?;
        Ok(Expr::Not(Box::new(p)))
    }

    fn exists(&mut self) -> Result<Expr> {
        self.i += 1;
        self.expect(Tok::LParen)?;
        let at = self.i;
        let e = self.or()?;
        let e = self.expression(e, at)?;
        self.expect(Tok::RParen)?;
        Ok(Expr::Exists(Box::new(e)))
    }

    fn comparison(&mut self) -> Result<Expr> {
        let l = self.additive()?;
        let op = match self.tok() {
            Tok::Eq => Cmp::Eq,
            Tok::Ne => Cmp::Ne,
            Tok::Lt => Cmp::Lt,
            Tok::Le => Cmp::Le,
            Tok::Gt => Cmp::Gt,
            Tok::Ge => Cmp::Ge,
            Tok::Ident(_) if self.keyword("starts") => {
                let l = self.expression(l, self.i)?;
                self.i += 1;
                self.expect_keyword("with")?;
                let r = match self.tok().clone() {
                    Tok::Str(s) => {
                        let b = super::super::ops::string(&s).into_boxed_slice();
                        Expr::Str(s, b)
                    }
                    Tok::Var(v) => Expr::Var(v),
                    _ => return Err(self.unexpected()),
                };
                self.i += 1;
                return Ok(Expr::StartsWith(Box::new(l), Box::new(r)));
            }
            Tok::Ident(_) if self.keyword("like_regex") => {
                let l = self.expression(l, self.i)?;
                self.i += 1;
                let Tok::Str(pattern) = self.tok().clone() else {
                    return Err(self.unexpected());
                };
                self.i += 1;
                let mut flags = String::new();
                if self.keyword("flag") {
                    self.i += 1;
                    let Tok::Str(f) = self.tok().clone() else {
                        return Err(self.unexpected());
                    };
                    self.i += 1;
                    flags = f;
                }
                return Ok(Expr::LikeRegex(
                    Box::new(l),
                    Box::new(regex(pattern, &flags)?),
                ));
            }
            _ => return Ok(l),
        };
        let l = self.expression(l, self.i)?;
        self.i += 1;
        let at = self.i;
        let r = self.additive()?;
        let r = self.expression(r, at)?;
        Ok(Expr::Cmp(op, Box::new(l), Box::new(r)))
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.tok() {
                Tok::Plus => Arith::Add,
                Tok::Minus => Arith::Sub,
                _ => return Ok(l),
            };
            l = self.expression(l, self.i)?;
            self.i += 1;
            let at = self.i;
            let r = self.multiplicative()?;
            let r = self.expression(r, at)?;
            l = Expr::Arith(op, Box::new(l), Box::new(r));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.tok() {
                Tok::Star => Arith::Mul,
                Tok::Slash => Arith::Div,
                Tok::Percent => Arith::Mod,
                _ => return Ok(l),
            };
            l = self.expression(l, self.i)?;
            self.i += 1;
            let at = self.i;
            let r = self.unary()?;
            let r = self.expression(r, at)?;
            l = Expr::Arith(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        let minus = match self.tok() {
            Tok::Plus => false,
            Tok::Minus => true,
            _ => return self.accessor(),
        };
        self.i += 1;
        let at = self.i;
        let e = self.unary()?;
        let e = self.expression(e, at)?;
        // A sign on a number is the number's (as PostgreSQL folds it).
        Ok(match e {
            Expr::Num(d, _) => {
                let d = if minus { d.negate() } else { d };
                num(d)
            }
            e => Expr::Neg(minus, Box::new(e)),
        })
    }

    fn accessor(&mut self) -> Result<Expr> {
        let (head, parenthesized) = self.primary()?;
        let mut steps = Vec::new();
        loop {
            let at = self.i;
            let step = match self.tok() {
                Tok::Dot => self.dot()?,
                Tok::LBracket => self.subscript()?,
                Tok::Question => self.filter()?,
                _ => break,
            };
            // Of the predicates only a parenthesized one takes accessors.
            if head.is_predicate() && !parenthesized {
                return Err(self.blame(at));
            }
            steps.push(step);
        }
        if steps.is_empty() {
            return Ok(head);
        }
        Ok(match head {
            Expr::Chain(h, mut first) => {
                first.extend(steps);
                Expr::Chain(h, first)
            }
            h => Expr::Chain(Box::new(h), steps),
        })
    }

    /// A primary, and whether it was parenthesized.
    fn primary(&mut self) -> Result<(Expr, bool)> {
        let e = match self.tok().clone() {
            Tok::Dollar => Expr::Root,
            Tok::At => {
                if self.filters == 0 {
                    return Err(syntax("@ is not allowed in root expressions"));
                }
                Expr::Current
            }
            Tok::Var(v) => Expr::Var(v),
            Tok::Str(s) => {
                let b = super::super::ops::string(&s).into_boxed_slice();
                Expr::Str(s, b)
            }
            Tok::Num(d, _) => num(d),
            Tok::Ident(w) => match w.as_str() {
                "null" => Expr::Null,
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                _ if w.eq_ignore_ascii_case("last") => {
                    if self.subscripts == 0 {
                        return Err(syntax("LAST is allowed only in array subscripts"));
                    }
                    Expr::Last
                }
                _ if w.eq_ignore_ascii_case("exists") => return Ok((self.exists()?, false)),
                _ => return Err(self.unexpected()),
            },
            Tok::LParen => {
                self.i += 1;
                let e = self.or()?;
                self.expect(Tok::RParen)?;
                if e.is_predicate() && self.keyword("is") {
                    self.i += 1;
                    self.expect_keyword("unknown")?;
                    return Ok((Expr::IsUnknown(Box::new(e)), false));
                }
                return Ok((e, true));
            }
            _ => return Err(self.unexpected()),
        };
        self.i += 1;
        Ok((e, false))
    }

    /// An accessor or a method after `.`.
    fn dot(&mut self) -> Result<Step> {
        self.i += 1;
        let step = match self.tok().clone() {
            Tok::Star => Step::AnyKey,
            Tok::StarStar => {
                self.i += 1;
                let (first, last) = if *self.tok() == Tok::LBrace {
                    self.i += 1;
                    let first = self.level()?;
                    let last = if self.keyword("to") {
                        self.i += 1;
                        self.level()?
                    } else {
                        first
                    };
                    self.expect(Tok::RBrace)?;
                    (first, last)
                } else {
                    (0, LAST)
                };
                return Ok(Step::Any(first, last));
            }
            Tok::Str(s) => Step::Key(s),
            Tok::Ident(w) => {
                let method = METHODS.iter().find(|m| w.eq_ignore_ascii_case(m));
                match method {
                    Some(m) if *self.peek(1) == Tok::LParen => {
                        self.i += 2;
                        let m = self.method(m)?;
                        self.expect(Tok::RParen)?;
                        return Ok(Step::Method(m));
                    }
                    _ => Step::Key(w),
                }
            }
            _ => return Err(self.unexpected()),
        };
        self.i += 1;
        Ok(step)
    }

    /// A `.**` level: an integer or `last`.
    fn level(&mut self) -> Result<u32> {
        let v = match self.tok() {
            Tok::Num(d, true) if d.signum() >= 0 => u32::try_from(d.mantissa())
                .ok()
                .filter(|&v| v != LAST)
                .ok_or_else(|| self.unexpected())?,
            _ if self.keyword("last") => LAST,
            _ => return Err(self.unexpected()),
        };
        self.i += 1;
        Ok(v)
    }

    /// A method's arguments (after its `(`).
    fn method(&mut self, name: &str) -> Result<Method> {
        Ok(match name {
            "type" => Method::Type,
            "size" => Method::Size,
            "double" => Method::Double,
            "ceiling" => Method::Ceiling,
            "floor" => Method::Floor,
            "abs" => Method::Abs,
            "keyvalue" => Method::KeyValue,
            "bigint" => Method::BigInt,
            "boolean" => Method::Boolean,
            "integer" => Method::Integer,
            "number" => Method::Number,
            "string" => Method::String,
            "date" => Method::Date,
            "datetime" => Method::Datetime(match self.tok().clone() {
                Tok::Str(s) => {
                    self.i += 1;
                    Some(s)
                }
                _ => None,
            }),
            "decimal" => {
                let p = self.int(true)?;
                let s = if p.is_some() && *self.tok() == Tok::Comma {
                    self.i += 1;
                    Some(self.int(true)?.ok_or_else(|| self.unexpected())?)
                } else {
                    None
                };
                Method::Decimal(p, s)
            }
            "time" => Method::Time(self.int(false)?),
            "time_tz" => Method::TimeTz(self.int(false)?),
            "timestamp" => Method::Timestamp(self.int(false)?),
            _ => Method::TimestampTz(self.int(false)?),
        })
    }

    /// An optional integer argument (signed, if `signed`).
    fn int(&mut self, signed: bool) -> Result<Option<i32>> {
        let negative = match self.tok() {
            Tok::Minus | Tok::Plus if signed => {
                let n = *self.tok() == Tok::Minus;
                self.i += 1;
                if !matches!(self.tok(), Tok::Num(_, true)) {
                    return Err(self.unexpected());
                }
                n
            }
            _ => false,
        };
        let Tok::Num(d, true) = self.tok() else {
            if matches!(self.tok(), Tok::Num(..)) {
                return Err(self.unexpected());
            }
            return Ok(None);
        };
        let v = i32::try_from(d.mantissa()).map_err(|_| self.unexpected())?;
        self.i += 1;
        Ok(Some(if negative { -v } else { v }))
    }

    fn subscript(&mut self) -> Result<Step> {
        self.i += 1;
        if *self.tok() == Tok::Star && *self.peek(1) == Tok::RBracket {
            self.i += 2;
            return Ok(Step::AnyArray);
        }
        self.subscripts += 1;
        let mut items = Vec::new();
        loop {
            let from = self.expr()?;
            let to = if self.keyword("to") {
                self.i += 1;
                Some(self.expr()?)
            } else {
                None
            };
            items.push((from, to));
            if *self.tok() != Tok::Comma {
                break;
            }
            self.i += 1;
        }
        self.subscripts -= 1;
        self.expect(Tok::RBracket)?;
        Ok(Step::Index(items))
    }

    /// An expression, not a predicate.
    fn expr(&mut self) -> Result<Expr> {
        let at = self.i;
        let e = self.additive()?;
        self.expression(e, at)
    }

    fn filter(&mut self) -> Result<Step> {
        self.i += 1;
        self.expect(Tok::LParen)?;
        self.filters += 1;
        let at = self.i;
        let p = self.or()?;
        let p = self.predicate(p, at)?;
        self.filters -= 1;
        self.expect(Tok::RParen)?;
        Ok(Step::Filter(Box::new(p)))
    }
}

/// A number literal.
pub(super) fn num(d: Decimal) -> Expr {
    let b = super::super::ops::from_value(&crate::types::Value::Numeric(d))
        .unwrap_or_default()
        .into_boxed_slice();
    Expr::Num(d, b)
}

/// `like_regex`'s pattern and flags, compiled.
fn regex(pattern: String, flags: &str) -> Result<Regex> {
    let mut f = 0;
    for c in flags.chars() {
        f |= match c {
            'i' => FLAG_I,
            's' => FLAG_S,
            'm' => FLAG_M,
            'q' => FLAG_Q,
            'x' => {
                return Err(Error::feature(
                    "XQuery \"x\" flag (expanded regular expressions) is not implemented",
                ));
            }
            _ => {
                return Err(syntax(format!(
                    "invalid input syntax for type jsonpath: unrecognized flag character \"{c}\" in LIKE_REGEX predicate"
                )));
            }
        };
    }
    let source = if f & FLAG_Q != 0 {
        regex::escape(&pattern)
    } else {
        pattern.clone()
    };
    let re = regex::RegexBuilder::new(&source)
        .case_insensitive(f & FLAG_I != 0)
        .dot_matches_new_line(f & FLAG_S != 0)
        .multi_line(f & FLAG_M != 0)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| {
            Error::invalid_regex(format!(
                "invalid regular expression: {}",
                e.to_string().lines().last().unwrap_or("")
            ))
        })?;
    Ok(Regex {
        pattern,
        flags: f,
        re,
    })
}
