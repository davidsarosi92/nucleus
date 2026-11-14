//! Datetime items: `.datetime()` and its kin read a string — with a
//! template, or by trying the ISO 8601 forms PostgreSQL tries — into a
//! date, a time (with or without a zone), or a timestamp (with or
//! without); they compare with each other, and they print as ISO 8601
//! (`2020-01-01T10:00:00+03:00`), as PostgreSQL's do.
//!
//! The template language is `to_timestamp`'s, as PostgreSQL's standard
//! mode reads it (separators must match; spaces before a field are
//! skipped): `YYYY` `YYY` `YY` `Y` `MM` `DD` `Mon` `Month` `Dy` `Day`
//! `HH24` `HH12` `HH` `MI` `SS` `MS` `US` `FF1`–`FF6` `AM` `PM` `A.M.`
//! `P.M.` `TZH` `TZM` `TZ`, and `"quoted text"`.  A zone is `Z`, `UTC`,
//! `GMT`, `UT` or an offset; the session's zone is UTC.

use std::cmp::Ordering;

use crate::datetime::{date_from_ymd, format_time, ymd};
use crate::error::{Error, Result};

const US_PER_SEC: i64 = 1_000_000;
const US_PER_DAY: i64 = 86_400 * US_PER_SEC;

/// A datetime item.  Zones are seconds east of UTC; a `TimestampTz`
/// holds its instant (UTC) and the zone it was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dt {
    Date(i32),
    Time(i64),
    TimeTz(i64, i32),
    Timestamp(i64),
    TimestampTz(i64, i32),
}

impl Dt {
    /// `.type()`'s name for it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Dt::Date(_) => "date",
            Dt::Time(_) => "time without time zone",
            Dt::TimeTz(..) => "time with time zone",
            Dt::Timestamp(_) => "timestamp without time zone",
            Dt::TimestampTz(..) => "timestamp with time zone",
        }
    }

    /// Its ISO 8601 text.
    pub fn text(&self) -> String {
        match *self {
            Dt::Date(d) => date_text(d),
            Dt::Time(t) => format_time(t),
            Dt::TimeTz(t, z) => format!("{}{}", format_time(t), zone_text(z)),
            Dt::Timestamp(ts) => ts_text(ts),
            Dt::TimestampTz(ts, z) => {
                format!(
                    "{}{}",
                    ts_text(ts + i64::from(z) * US_PER_SEC),
                    zone_text(z)
                )
            }
        }
    }
}

fn date_text(d: i32) -> String {
    let (y, m, d) = ymd(d);
    if y <= 0 {
        format!("{:04}-{m:02}-{d:02} BC", 1 - y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    }
}

fn ts_text(ts: i64) -> String {
    let day = ts.div_euclid(US_PER_DAY);
    let t = ts.rem_euclid(US_PER_DAY);
    let (y, m, d) = ymd(day as i32);
    let date = if y <= 0 {
        format!("{:04}-{m:02}-{d:02}", 1 - y)
    } else {
        format!("{y:04}-{m:02}-{d:02}")
    };
    let bc = if y <= 0 { " BC" } else { "" };
    format!("{date}T{}{bc}", format_time(t))
}

fn zone_text(z: i32) -> String {
    let sign = if z < 0 { '-' } else { '+' };
    let a = z.unsigned_abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

/// The error for a cast that needs the session's zone, outside the
/// `_tz` functions.
pub fn needs_zone(from: &str, to: &str) -> Error {
    Error::feature(format!(
        "cannot convert value from {from} to {to} without time zone usage"
    ))
    .with_hint("Use *_tz() function for time zone support.")
}

/// Compares two datetime items: `Ok(None)` when they do not compare (a
/// date and a time); an error when comparing needs a zone and `use_tz`
/// is off.
pub fn compare(a: Dt, b: Dt, use_tz: bool) -> Result<Option<Ordering>> {
    let check = |from: &str, to: &str| {
        if use_tz {
            Ok(())
        } else {
            Err(needs_zone(from, to))
        }
    };
    let date_ts = |d: i32| i64::from(d) * US_PER_DAY;
    // A time with the session's zone (UTC): its UTC time and zone west.
    let tz_key = |t: i64, z: i32| ((t - i64::from(z) * US_PER_SEC), -z);
    Ok(Some(match (a, b) {
        (Dt::Date(x), Dt::Date(y)) => x.cmp(&y),
        (Dt::Date(x), Dt::Timestamp(y)) => date_ts(x).cmp(&y),
        (Dt::Timestamp(x), Dt::Date(y)) => x.cmp(&date_ts(y)),
        (Dt::Date(x), Dt::TimestampTz(y, _)) => {
            check("date", "timestamptz")?;
            date_ts(x).cmp(&y)
        }
        (Dt::TimestampTz(x, _), Dt::Date(y)) => {
            check("date", "timestamptz")?;
            x.cmp(&date_ts(y))
        }
        (Dt::Timestamp(x), Dt::Timestamp(y)) => x.cmp(&y),
        (Dt::TimestampTz(x, _), Dt::TimestampTz(y, _)) => x.cmp(&y),
        (Dt::Timestamp(x), Dt::TimestampTz(y, _)) => {
            check("timestamp", "timestamptz")?;
            x.cmp(&y)
        }
        (Dt::TimestampTz(x, _), Dt::Timestamp(y)) => {
            check("timestamp", "timestamptz")?;
            x.cmp(&y)
        }
        (Dt::Time(x), Dt::Time(y)) => x.cmp(&y),
        (Dt::TimeTz(x, zx), Dt::TimeTz(y, zy)) => tz_key(x, zx).cmp(&tz_key(y, zy)),
        (Dt::Time(x), Dt::TimeTz(y, zy)) => {
            check("time", "timetz")?;
            tz_key(x, 0).cmp(&tz_key(y, zy))
        }
        (Dt::TimeTz(x, zx), Dt::Time(y)) => {
            check("time", "timetz")?;
            tz_key(x, zx).cmp(&tz_key(y, 0))
        }
        _ => return Ok(None),
    }))
}

/// What a datetime method makes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Any,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Target::Any => "datetime",
            Target::Date => "date",
            Target::Time => "time",
            Target::TimeTz => "time_tz",
            Target::Timestamp => "timestamp",
            Target::TimestampTz => "timestamp_tz",
        }
    }
}

/// A failure: `Err(Ok(e))` one the silent mode hides, `Err(Err(e))` one
/// it does not (a cast that needs a zone).
pub type Outcome<T> = std::result::Result<T, std::result::Result<Error, Error>>;

fn soft<T>(e: Error) -> Outcome<T> {
    Err(Ok(e))
}

/// `.datetime(template)`, `.date()`, `.time(p)` …: the string read and
/// cast to `target`, rounded to `precision` fractional digits.
pub fn method(
    s: &str,
    template: Option<&str>,
    target: Target,
    precision: Option<i32>,
    use_tz: bool,
) -> Outcome<Dt> {
    let v = match template {
        Some(t) => read(s, &compile(t).or_else(soft)?).or_else(soft)?,
        None => match DEFAULTS
            .iter()
            .find_map(|f| compile(f).ok().and_then(|nodes| read(s, &nodes).ok()))
        {
            Some(v) => v,
            None => {
                return soft(
                    Error::json_datetime(format!(
                        "{} format is not recognized: \"{s}\"",
                        target.name()
                    ))
                    .with_hint(
                        "Use a datetime template argument to specify the input data format.",
                    ),
                );
            }
        },
    };
    let unrecognized = || {
        soft(Error::json_datetime(format!(
            "{} format is not recognized: \"{s}\"",
            target.name()
        )))
    };
    let check = |from: &str, to: &str| -> Outcome<()> {
        if use_tz {
            Ok(())
        } else {
            Err(Err(needs_zone(from, to)))
        }
    };
    let time_of = |ts: i64| ts.rem_euclid(US_PER_DAY);
    let v = match (target, v) {
        (Target::Any, v) => v,
        (Target::Date, Dt::Date(d)) => Dt::Date(d),
        (Target::Date, Dt::Timestamp(ts)) => Dt::Date(ts.div_euclid(US_PER_DAY) as i32),
        (Target::Date, Dt::TimestampTz(ts, _)) => {
            check("timestamptz", "date")?;
            Dt::Date(ts.div_euclid(US_PER_DAY) as i32)
        }
        (Target::Time, Dt::Time(t)) => Dt::Time(t),
        (Target::Time, Dt::TimeTz(t, _)) => {
            check("timetz", "time")?;
            Dt::Time(t)
        }
        (Target::Time, Dt::Timestamp(ts)) => Dt::Time(time_of(ts)),
        (Target::Time, Dt::TimestampTz(ts, _)) => {
            check("timestamptz", "time")?;
            Dt::Time(time_of(ts))
        }
        (Target::TimeTz, Dt::Time(t)) => {
            check("time", "timetz")?;
            Dt::TimeTz(t, 0)
        }
        (Target::TimeTz, Dt::TimeTz(t, z)) => Dt::TimeTz(t, z),
        (Target::TimeTz, Dt::TimestampTz(ts, _)) => Dt::TimeTz(time_of(ts), 0),
        (Target::Timestamp, Dt::Date(d)) => Dt::Timestamp(i64::from(d) * US_PER_DAY),
        (Target::Timestamp, Dt::Timestamp(ts)) => Dt::Timestamp(ts),
        (Target::Timestamp, Dt::TimestampTz(ts, _)) => {
            check("timestamptz", "timestamp")?;
            Dt::Timestamp(ts)
        }
        (Target::TimestampTz, Dt::Date(d)) => {
            check("date", "timestamptz")?;
            Dt::TimestampTz(i64::from(d) * US_PER_DAY, 0)
        }
        (Target::TimestampTz, Dt::Timestamp(ts)) => {
            check("timestamp", "timestamptz")?;
            Dt::TimestampTz(ts, 0)
        }
        (Target::TimestampTz, Dt::TimestampTz(ts, z)) => Dt::TimestampTz(ts, z),
        _ => return unrecognized(),
    };
    let Some(p) = precision else {
        return Ok(v);
    };
    if p < 0 {
        return soft(Error::invalid_parameter(format!(
            "time precision of jsonpath item method .{}() is invalid",
            target.name()
        )));
    }
    let round = |us: i64| {
        let scale = 10i64.pow(6 - p.min(6) as u32);
        (us + scale / 2).div_euclid(scale) * scale
    };
    Ok(match v {
        Dt::Time(t) => Dt::Time(round(t)),
        Dt::TimeTz(t, z) => Dt::TimeTz(round(t), z),
        Dt::Timestamp(ts) => Dt::Timestamp(round(ts)),
        Dt::TimestampTz(ts, z) => Dt::TimestampTz(round(ts), z),
        d => d,
    })
}

/// The forms `.datetime()` tries, in order.
const DEFAULTS: &[&str] = &[
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZ",
    "HH24:MI:SSTZ",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZ",
    "yyyy-mm-dd HH24:MI:SSTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZ",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Year(u8),
    Month,
    MonthName(bool),
    DayName(bool),
    Day,
    Hour24,
    Hour12,
    Minute,
    Second,
    Ms,
    /// `US` (6) or `FFn`.
    Fraction(u8),
    Meridiem(bool),
    Tzh,
    Tzm,
    Tz,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Year(4) => "YYYY",
            Field::Year(3) => "YYY",
            Field::Year(2) => "YY",
            Field::Year(_) => "Y",
            Field::Month => "MM",
            Field::MonthName(_) => "Mon",
            Field::DayName(_) => "Dy",
            Field::Day => "DD",
            Field::Hour24 => "HH24",
            Field::Hour12 => "HH12",
            Field::Minute => "MI",
            Field::Second => "SS",
            Field::Ms => "MS",
            Field::Fraction(_) => "US",
            Field::Meridiem(_) => "AM",
            Field::Tzh => "TZH",
            Field::Tzm => "TZM",
            Field::Tz => "TZ",
        }
    }

    /// Its width, for a numeric field.
    fn width(self) -> Option<usize> {
        Some(match self {
            Field::Year(n) | Field::Fraction(n) => n as usize,
            Field::Month | Field::Day | Field::Hour24 | Field::Hour12 => 2,
            Field::Minute | Field::Second => 2,
            Field::Ms => 3,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Field(Field),
    /// A space or a separator: matched exactly.
    Sep(char),
    /// Text: consumes a character, whatever it is.
    Text,
}

/// The template's patterns, longest first; matched in upper or lower
/// case (and, for names, capitalized).
const PATTERNS: &[(&str, Field)] = &[
    ("A.M.", Field::Meridiem(true)),
    ("P.M.", Field::Meridiem(true)),
    ("MONTH", Field::MonthName(true)),
    ("HH24", Field::Hour24),
    ("HH12", Field::Hour12),
    ("YYYY", Field::Year(4)),
    ("FF1", Field::Fraction(1)),
    ("FF2", Field::Fraction(2)),
    ("FF3", Field::Fraction(3)),
    ("FF4", Field::Fraction(4)),
    ("FF5", Field::Fraction(5)),
    ("FF6", Field::Fraction(6)),
    ("YYY", Field::Year(3)),
    ("TZH", Field::Tzh),
    ("TZM", Field::Tzm),
    ("MON", Field::MonthName(false)),
    ("DAY", Field::DayName(true)),
    ("AM", Field::Meridiem(false)),
    ("PM", Field::Meridiem(false)),
    ("HH", Field::Hour12),
    ("MI", Field::Minute),
    ("MM", Field::Month),
    ("MS", Field::Ms),
    ("SS", Field::Second),
    ("US", Field::Fraction(6)),
    ("DD", Field::Day),
    ("DY", Field::DayName(false)),
    ("TZ", Field::Tz),
    ("YY", Field::Year(2)),
    ("Y", Field::Year(1)),
];

fn compile(t: &str) -> Result<Vec<Node>> {
    let mut out = Vec::new();
    let mut rest = t;
    'next: while let Some(c) = rest.chars().next() {
        if c == '"' {
            // Quoted text: each character consumes one.
            let mut it = rest[1..].char_indices();
            let mut end = rest.len();
            while let Some((i, c)) = it.next() {
                match c {
                    '"' => {
                        end = i + 2;
                        break;
                    }
                    '\\' => {
                        it.next();
                        out.push(Node::Text);
                    }
                    _ => out.push(Node::Text),
                }
            }
            rest = &rest[end.min(rest.len())..];
            continue;
        }
        for (p, f) in PATTERNS {
            if let Some(head) = rest.get(..p.len()) {
                let title = |s: &str| {
                    let mut c = s.chars();
                    c.next().is_some_and(|f| f.is_ascii_uppercase())
                        && c.as_str() == c.as_str().to_ascii_lowercase()
                };
                let names = matches!(f, Field::MonthName(_) | Field::DayName(_));
                if head == *p
                    || head == p.to_ascii_lowercase()
                    || (names && title(head) && head.eq_ignore_ascii_case(p))
                {
                    out.push(Node::Field(*f));
                    rest = &rest[p.len()..];
                    continue 'next;
                }
            }
        }
        out.push(
            if c == ' ' || (c.is_ascii_graphic() && !c.is_ascii_alphanumeric()) {
                Node::Sep(c)
            } else {
                Node::Text
            },
        );
        rest = &rest[c.len_utf8()..];
    }
    Ok(out)
}

fn bad_value(v: &str, f: Field, detail: &str) -> Error {
    let v: String = v.chars().take(f.width().unwrap_or(4).max(1)).collect();
    Error::datetime_format(format!("invalid value \"{v}\" for \"{}\"", f.name())).with_hint(detail)
}

/// What the fields read.
#[derive(Default)]
struct Parts {
    year: Option<i64>,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<i64>,
    twelve: bool,
    pm: Option<bool>,
    minute: i64,
    second: i64,
    us: i64,
    tz: Option<i32>,
    dated: bool,
    timed: bool,
    zoned: bool,
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const DAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// Whether the node after `i` cannot continue a number (so a number
/// field reads all its digits).
fn separated_after(nodes: &[Node], i: usize) -> bool {
    match nodes.get(i + 1) {
        None => true,
        Some(Node::Field(f)) => f.width().is_none() && !matches!(f, Field::Tzh | Field::Tzm),
        Some(_) => true,
    }
}

/// `strtol`: spaces, a sign, digits; the value and the bytes used.
fn strtol(s: &str) -> Option<(i64, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut v: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add(i64::from(b[i] - b'0'));
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((if neg { -v } else { v }, i))
}

/// A number field at `s`: its value and the digits read.
fn number(s: &mut &str, f: Field, slurp: bool) -> Result<(i64, usize)> {
    let t = s.trim_start();
    let (v, used) = if slurp {
        strtol(t).ok_or_else(|| bad_value(t, f, "Value must be an integer."))?
    } else {
        let width = f.width().unwrap_or(2);
        let head: String = t.chars().take(width).collect();
        if head.chars().count() < width {
            return Err(Error::datetime_format(format!(
                "source string too short for \"{}\" formatting field",
                f.name()
            )));
        }
        let (v, used) =
            strtol(&head).ok_or_else(|| bad_value(t, f, "Value must be an integer."))?;
        if used < width {
            return Err(bad_value(
                t,
                f,
                "Field requires more characters than could be parsed.",
            ));
        }
        (v, used)
    };
    let digits = t[..used].bytes().filter(u8::is_ascii_digit).count();
    *s = &t[used..];
    Ok((v, digits))
}

fn out_of_range(s: &str) -> Error {
    Error::datetime_overflow(format!("date/time field value out of range: \"{s}\""))
}

/// Reads `input` with a compiled template.
fn read(input: &str, nodes: &[Node]) -> Result<Dt> {
    let mut p = Parts::default();
    let mut s = input;
    let mut extra = 0usize;
    let mut i = 0;
    while i < nodes.len() && !s.is_empty() {
        let n = nodes[i];
        if matches!(n, Node::Field(_)) || i == 0 {
            let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
            extra += s.len() - t.len();
            s = t;
            if s.is_empty() {
                break;
            }
        }
        match n {
            Node::Sep(c) => {
                if s.starts_with(c) {
                    s = &s[c.len_utf8()..];
                } else {
                    return Err(Error::datetime_format(format!(
                        "unmatched format separator \"{c}\""
                    )));
                }
            }
            Node::Text => {
                if extra > 0 {
                    extra -= 1;
                } else {
                    let c = s.chars().next().expect("not empty");
                    s = &s[c.len_utf8()..];
                }
            }
            Node::Field(f) => field(&mut p, f, &mut s, separated_after(nodes, i), input)?,
        }
        i += 1;
    }
    if i < nodes.len() {
        return Err(Error::datetime_format(
            "input string is too short for datetime format",
        ));
    }
    if !s.trim_start().is_empty() {
        return Err(Error::datetime_format(
            "trailing characters remain in input string after datetime format",
        ));
    }
    build(p, input)
}

fn field(p: &mut Parts, f: Field, s: &mut &str, slurp: bool, input: &str) -> Result<()> {
    match f {
        Field::Year(n) => {
            let (v, _) = number(s, f, slurp)?;
            p.year = Some(if n < 4 { adjust_year(v) } else { v });
            p.dated = true;
        }
        Field::Month => {
            p.month = Some(u32::try_from(number(s, f, slurp)?.0).map_err(|_| out_of_range(input))?);
            p.dated = true;
        }
        Field::Day => {
            p.day = Some(u32::try_from(number(s, f, slurp)?.0).map_err(|_| out_of_range(input))?);
            p.dated = true;
        }
        Field::MonthName(full) => {
            let lower = s.to_ascii_lowercase();
            let (m, len) = MONTHS
                .iter()
                .enumerate()
                .find_map(|(i, name)| {
                    let n = if full { name.len() } else { 3 };
                    lower.starts_with(&name[..n]).then_some((i as u32 + 1, n))
                })
                .ok_or_else(|| {
                    bad_value(
                        s,
                        f,
                        "The given value did not match any of the allowed values for this field.",
                    )
                })?;
            p.month = Some(m);
            p.dated = true;
            *s = &s[len..];
        }
        Field::DayName(full) => {
            let lower = s.to_ascii_lowercase();
            let len = DAYS
                .iter()
                .find_map(|name| {
                    let n = if full { name.len() } else { 3 };
                    lower.starts_with(&name[..n]).then_some(n)
                })
                .ok_or_else(|| {
                    bad_value(
                        s,
                        f,
                        "The given value did not match any of the allowed values for this field.",
                    )
                })?;
            *s = &s[len..];
        }
        Field::Hour24 | Field::Hour12 => {
            p.hour = Some(number(s, f, slurp)?.0);
            p.twelve = f == Field::Hour12;
            p.timed = true;
        }
        Field::Minute => {
            p.minute = number(s, f, slurp)?.0;
            p.timed = true;
        }
        Field::Second => {
            p.second = number(s, f, slurp)?.0;
            p.timed = true;
        }
        Field::Ms => {
            let (v, d) = number(s, f, slurp)?;
            let scale = match d {
                1 => 100,
                2 => 10,
                _ => 1,
            };
            p.us = v * scale * 1000;
            p.timed = true;
        }
        Field::Fraction(_) => {
            let (v, d) = number(s, f, slurp)?;
            p.us = if d < 6 {
                v * 10i64.pow(6 - d as u32)
            } else {
                v
            };
            p.timed = true;
        }
        Field::Meridiem(dotted) => {
            let lower = s.to_ascii_lowercase();
            let (am, pm) = if dotted {
                ("a.m.", "p.m.")
            } else {
                ("am", "pm")
            };
            p.pm = Some(if lower.starts_with(am) {
                false
            } else if lower.starts_with(pm) {
                true
            } else {
                return Err(bad_value(
                    s,
                    f,
                    "The given value did not match any of the allowed values for this field.",
                ));
            });
            *s = &s[am.len()..];
            p.timed = true;
        }
        Field::Tzh => {
            let sign = sign(s);
            let (h, _) = number(s, f, slurp)?;
            let m = p.tz.map_or(0, |z| z.unsigned_abs() % 3600);
            p.tz = Some(sign * (h as i32 * 3600 + m as i32));
            p.zoned = true;
        }
        Field::Tzm => {
            let (m, _) = number(s, f, slurp)?;
            let z = p.tz.unwrap_or(0);
            let sign = if z < 0 { -1 } else { 1 };
            p.tz = Some(z + sign * m as i32 * 60);
            p.zoned = true;
        }
        Field::Tz => {
            let lower = s.to_ascii_lowercase();
            if let Some(a) = ["utc", "gmt", "ut", "z"].iter().find(|a| {
                lower.starts_with(**a)
                    && !lower[a.len()..].starts_with(|c: char| c.is_ascii_alphabetic())
            }) {
                p.tz = Some(0);
                *s = &s[a.len()..];
            } else if s.starts_with(|c: char| c.is_ascii_alphabetic()) {
                return Err(bad_value(s, f, "Time zone abbreviation is not recognized."));
            } else {
                let sign = sign(s);
                let (h, _) = number(s, Field::Tzh, slurp)?;
                let mut z = h as i32 * 3600;
                if let Some(rest) = s.strip_prefix(':') {
                    *s = rest;
                    let (m, _) = number(s, Field::Tzm, slurp)?;
                    z += m as i32 * 60;
                }
                p.tz = Some(sign * z);
            }
            p.zoned = true;
        }
    }
    Ok(())
}

/// A zone's sign, read: `+`, `-` or a space (positive), else positive.
fn sign(s: &mut &str) -> i32 {
    match s.as_bytes().first() {
        Some(b'-') => {
            *s = &s[1..];
            -1
        }
        Some(b'+' | b' ') => {
            *s = &s[1..];
            1
        }
        _ => 1,
    }
}

/// A year of fewer than four digits, toward 2020 (`70` is 1970, `69`
/// 2069), as PostgreSQL reads it.
fn adjust_year(y: i64) -> i64 {
    if y < 70 {
        y + 2000
    } else if y < 100 {
        y + 1900
    } else if y < 520 {
        y + 2000
    } else if y < 1000 {
        y + 1000
    } else {
        y
    }
}

fn build(p: Parts, input: &str) -> Result<Dt> {
    let mut hour = p.hour.unwrap_or(0);
    if p.twelve || p.pm.is_some() {
        if !(1..=12).contains(&hour) {
            return Err(Error::datetime_overflow(format!(
                "hour \"{hour}\" is invalid for the 12-hour clock"
            ))
            .with_hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        if p.pm == Some(true) && hour < 12 {
            hour += 12;
        } else if p.pm != Some(true) && hour == 12 {
            hour = 0;
        }
    }
    if !(0..24).contains(&hour)
        || !(0..60).contains(&p.minute)
        || !(0..60).contains(&p.second)
        || !(0..US_PER_SEC).contains(&p.us)
    {
        return Err(out_of_range(input));
    }
    let time = ((hour * 60 + p.minute) * 60 + p.second) * US_PER_SEC + p.us;
    if let Some(z) = p.tz
        && z.abs() >= 16 * 3600
    {
        return Err(Error::datetime_overflow(format!(
            "time zone displacement out of range: \"{input}\""
        )));
    }
    let zone = p.tz.unwrap_or(0);
    if p.dated {
        let date = date_from_ymd(
            p.year.unwrap_or(1),
            p.month.unwrap_or(1),
            p.day.unwrap_or(1),
        )
        .map_err(|_| out_of_range(input))?;
        let ts = i64::from(date) * US_PER_DAY + time;
        return Ok(if p.zoned {
            Dt::TimestampTz(ts - i64::from(zone) * US_PER_SEC, zone)
        } else if p.timed {
            Dt::Timestamp(ts)
        } else {
            Dt::Date(date)
        });
    }
    if p.timed {
        return Ok(if p.zoned {
            Dt::TimeTz(time, zone)
        } else {
            Dt::Time(time)
        });
    }
    Err(Error::datetime_format(
        "datetime format is not dated and not timed",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str, t: Option<&str>) -> String {
        match method(s, t, Target::Any, None, false) {
            Ok(v) => format!("{} {}", v.text(), v.type_name()),
            Err(Ok(e)) | Err(Err(e)) => format!("error {}", e.code),
        }
    }

    #[test]
    fn default_forms() {
        assert_eq!(text("2020-1-1", None), "2020-01-01 date");
        assert_eq!(
            text("2020-01-01T10:00:00Z", None),
            "2020-01-01T10:00:00+00:00 timestamp with time zone"
        );
        assert_eq!(text("2020-01-01T10:00:00+0300", None), "error 22031");
        assert_eq!(
            text("2020-01-01T10:00:00+03:30", None),
            "2020-01-01T10:00:00+03:30 timestamp with time zone"
        );
        assert_eq!(text("2020-01-01T10:00:00-03:30:15", None), "error 22031");
        assert_eq!(text("2020-01-01 10:00:00.123456789", None), "error 22031");
        assert_eq!(
            text("2020-01-01 10:00:00.5", None),
            "2020-01-01T10:00:00.5 timestamp without time zone"
        );
        assert_eq!(
            text("2020-01-01  10:00:00", None),
            "2020-01-01T10:00:00 timestamp without time zone"
        );
        assert_eq!(text("10:00", None), "error 22031");
        assert_eq!(
            text("10:00:00.25-1", None),
            "10:00:00.25-01:00 time with time zone"
        );
        assert_eq!(text("20200101", None), "error 22031");
        assert_eq!(text("2020/01/01", None), "error 22031");
        assert_eq!(
            text("2020-01-01 10:00:00 +03", None),
            "2020-01-01T10:00:00+03:00 timestamp with time zone"
        );
        assert_eq!(text("  2020-01-01", None), "2020-01-01 date");
        assert_eq!(text("2020-13-01", None), "error 22031");
        assert_eq!(text("2020-02-30", None), "error 22031");
        assert_eq!(text("24:00:00", None), "error 22031");
        assert_eq!(text("0099-01-01", None), "0099-01-01 date");
        assert_eq!(text("12345-01-01", None), "12345-01-01 date");
    }

    #[test]
    fn templates() {
        assert_eq!(text("x", Some("YYYY")), "error 22007");
        assert_eq!(text("2020", Some("YYYY")), "2020-01-01 date");
        assert_eq!(
            text("10:30", Some("HH24:MI")),
            "10:30:00 time without time zone"
        );
        assert_eq!(
            text("12/31/2020 5pm", Some("MM/DD/YYYY HH12am")),
            "2020-12-31T17:00:00 timestamp without time zone"
        );
        assert_eq!(
            text(
                "2020-01-01 10:00:00+03:30",
                Some("YYYY-MM-DD HH24:MI:SSTZH:TZM")
            ),
            "2020-01-01T10:00:00+03:30 timestamp with time zone"
        );
        assert_eq!(text("2020-01-01", Some("DD/MM")), "error 22007");
        assert_eq!(text("05 Mar 2021", Some("DD Mon YYYY")), "2021-03-05 date");
    }
}
