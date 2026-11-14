//! A path's text as PostgreSQL normalizes it: keys and variables quoted,
//! `lax` left out, operators parenthesized by priority (`($."a" + 1)`),
//! numbers as `numeric` prints them.  One difference: an operator or a
//! number followed by accessors is parenthesized wherever it stands
//! (PostgreSQL drops the parentheses inside a filter, so that its text
//! reads back as another path).

use std::fmt::{self, Write};

use super::ast::*;

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        if self.strict {
            s.push_str("strict ");
        }
        item(&mut s, &self.expr, true);
        f.write_str(&s)
    }
}

fn priority(e: &Expr) -> u8 {
    match e {
        Expr::Or(..) => 0,
        Expr::And(..) => 1,
        Expr::Cmp(..) | Expr::StartsWith(..) => 2,
        Expr::Arith(Arith::Add | Arith::Sub, ..) => 3,
        Expr::Arith(..) => 4,
        Expr::Neg(..) => 5,
        Expr::Chain(h, _) => priority(h),
        _ => 6,
    }
}

fn string(out: &mut String, s: &str) {
    crate::json::text::write_str(s, out);
}

fn binary(out: &mut String, op: &str, e: &Expr, l: &Expr, r: &Expr, brackets: bool) {
    if brackets {
        out.push('(');
    }
    item(out, l, priority(l) <= priority(e));
    out.push(' ');
    out.push_str(op);
    out.push(' ');
    item(out, r, priority(r) <= priority(e));
    if brackets {
        out.push(')');
    }
}

fn item(out: &mut String, e: &Expr, brackets: bool) {
    match e {
        Expr::Root => out.push('$'),
        Expr::Current => out.push('@'),
        Expr::Last => out.push_str("last"),
        Expr::Var(v) => {
            out.push('$');
            string(out, v);
        }
        Expr::Null => out.push_str("null"),
        Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Num(d, _) => {
            let _ = write!(out, "{d}");
        }
        Expr::Str(s, _) => string(out, s),
        Expr::Chain(h, steps) => {
            match **h {
                Expr::Num(..) | Expr::Not(_) | Expr::IsUnknown(_) | Expr::Exists(_) => {
                    out.push('(');
                    item(out, h, false);
                    out.push(')');
                }
                _ => item(out, h, true),
            }
            for s in steps {
                step(out, s);
            }
        }
        Expr::Neg(minus, a) => {
            if brackets {
                out.push('(');
            }
            out.push(if *minus { '-' } else { '+' });
            item(out, a, priority(a) <= priority(e));
            if brackets {
                out.push(')');
            }
        }
        Expr::Arith(op, l, r) => {
            let op = match op {
                Arith::Add => "+",
                Arith::Sub => "-",
                Arith::Mul => "*",
                Arith::Div => "/",
                Arith::Mod => "%",
            };
            binary(out, op, e, l, r, brackets);
        }
        Expr::Cmp(op, l, r) => {
            let op = match op {
                Cmp::Eq => "==",
                Cmp::Ne => "!=",
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Gt => ">",
                Cmp::Ge => ">=",
            };
            binary(out, op, e, l, r, brackets);
        }
        Expr::And(l, r) => binary(out, "&&", e, l, r, brackets),
        Expr::Or(l, r) => binary(out, "||", e, l, r, brackets),
        Expr::StartsWith(l, r) => binary(out, "starts with", e, l, r, brackets),
        Expr::Not(a) => {
            out.push_str("!(");
            item(out, a, false);
            out.push(')');
        }
        Expr::IsUnknown(a) => {
            out.push('(');
            item(out, a, false);
            out.push_str(") is unknown");
        }
        Expr::Exists(a) => {
            out.push_str("exists (");
            item(out, a, false);
            out.push(')');
        }
        Expr::LikeRegex(a, re) => {
            if brackets {
                out.push('(');
            }
            item(out, a, priority(a) <= priority(e));
            out.push_str(" like_regex ");
            string(out, &re.pattern);
            if re.flags != 0 {
                out.push_str(" flag \"");
                for (bit, c) in [(FLAG_I, 'i'), (FLAG_S, 's'), (FLAG_M, 'm'), (FLAG_Q, 'q')] {
                    if re.flags & bit != 0 {
                        out.push(c);
                    }
                }
                out.push('"');
            }
            if brackets {
                out.push(')');
            }
        }
    }
}

fn step(out: &mut String, s: &Step) {
    match s {
        Step::Key(k) => {
            out.push('.');
            string(out, k);
        }
        Step::AnyKey => out.push_str(".*"),
        Step::AnyArray => out.push_str("[*]"),
        Step::Index(items) => {
            out.push('[');
            for (i, (from, to)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                item(out, from, false);
                if let Some(to) = to {
                    out.push_str(" to ");
                    item(out, to, false);
                }
            }
            out.push(']');
        }
        Step::Any(first, last) => {
            out.push('.');
            let _ = match (*first, *last) {
                (0, LAST) => write!(out, "**"),
                (LAST, LAST) => write!(out, "**{{last}}"),
                (a, b) if a == b => write!(out, "**{{{a}}}"),
                (LAST, b) => write!(out, "**{{last to {b}}}"),
                (a, LAST) => write!(out, "**{{{a} to last}}"),
                (a, b) => write!(out, "**{{{a} to {b}}}"),
            };
        }
        Step::Filter(p) => {
            out.push_str("?(");
            item(out, p, false);
            out.push(')');
        }
        Step::Method(m) => {
            out.push('.');
            out.push_str(m.name());
            out.push('(');
            match m {
                Method::Decimal(p, s) => {
                    if let Some(p) = p {
                        let _ = write!(out, "{p}");
                    }
                    if let Some(s) = s {
                        let _ = write!(out, ",{s}");
                    }
                }
                Method::Datetime(Some(t)) => string(out, t),
                Method::Time(Some(p))
                | Method::TimeTz(Some(p))
                | Method::Timestamp(Some(p))
                | Method::TimestampTz(Some(p)) => {
                    let _ = write!(out, "{p}");
                }
                _ => {}
            }
            out.push(')');
        }
    }
}
