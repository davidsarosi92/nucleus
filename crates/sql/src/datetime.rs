//! `DATE`, `TIMESTAMP`, `TIMESTAMPTZ` and `INTERVAL`: representation,
//! text input and output (PostgreSQL's ISO style), arithmetic and fields.
//!
//! - A date is days since 2000-01-01; a timestamp is microseconds since
//!   2000-01-01 00:00 (PostgreSQL's epoch, so the binary wire formats are
//!   the stored values).  Proleptic Gregorian calendar; years 4713 BC to
//!   294276 AD.
//! - `TIMESTAMPTZ` stores UTC.  Sessions run in UTC: input offsets are
//!   applied, output is `+00`.  `TIMESTAMP` ignores an input offset, as
//!   PostgreSQL does.
//! - An interval is months, days and microseconds, kept apart (a month is
//!   not a fixed number of days); for comparison a month counts 30 days and
//!   a day 24 hours, as in PostgreSQL.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nucleus_kv::Kv;

use crate::decimal::Decimal;
use crate::error::{Error, Result};

/// Microseconds per day.
pub const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
/// Days from 1970-01-01 to 2000-01-01.
const UNIX_TO_PG_DAYS: i64 = 10_957;
const MIN_YEAR: i64 = -4712; // 4713 BC
const MAX_YEAR: i64 = 294_276;

// ------------------------------------------------------------- calendar

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// algorithm; year 0 is 1 BC).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 2000-01-01.
pub fn date_from_ymd(y: i64, m: u32, d: u32) -> Result<i32> {
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(Error::datetime_overflow(format!(
            "date/time field value out of range: {y}-{m:02}-{d:02}"
        )));
    }
    if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
        return Err(Error::datetime_overflow("date out of range"));
    }
    Ok((days_from_civil(y, m, d) - UNIX_TO_PG_DAYS) as i32)
}

/// The year, month and day of a date.
pub fn ymd(date: i32) -> (i64, u32, u32) {
    civil_from_days(i64::from(date) + UNIX_TO_PG_DAYS)
}

fn check_date(days: i64) -> Result<i32> {
    let (y, _, _) = civil_from_days(days + UNIX_TO_PG_DAYS);
    if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
        return Err(Error::datetime_overflow("date out of range"));
    }
    Ok(days as i32)
}

fn check_ts(ts: i64) -> Result<i64> {
    let (y, _, _) = ymd(ts.div_euclid(USECS_PER_DAY) as i32);
    if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
        return Err(Error::datetime_overflow("timestamp out of range"));
    }
    Ok(ts)
}

/// The broken-down fields of a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields {
    /// Year (0 = 1 BC).
    pub year: i64,
    /// 1–12.
    pub month: u32,
    /// 1–31.
    pub day: u32,
    /// 0–23.
    pub hour: u32,
    /// 0–59.
    pub minute: u32,
    /// 0–59.
    pub second: u32,
    /// 0–999999.
    pub micros: u32,
}

/// The fields of a timestamp.
pub fn fields(ts: i64) -> Fields {
    let days = ts.div_euclid(USECS_PER_DAY);
    let t = ts.rem_euclid(USECS_PER_DAY);
    let (year, month, day) = ymd(days as i32);
    Fields {
        year,
        month,
        day,
        hour: (t / USECS_PER_HOUR) as u32,
        minute: (t % USECS_PER_HOUR / USECS_PER_MINUTE) as u32,
        second: (t % USECS_PER_MINUTE / USECS_PER_SEC) as u32,
        micros: (t % USECS_PER_SEC) as u32,
    }
}

/// The timestamp of a date at midnight.
pub fn date_to_ts(date: i32) -> i64 {
    i64::from(date).saturating_mul(USECS_PER_DAY)
}

/// The date of a timestamp.
pub fn ts_to_date(ts: i64) -> i32 {
    ts.div_euclid(USECS_PER_DAY) as i32
}

// ------------------------------------------------------------------- now

thread_local! {
    static NOW: RefCell<Option<(i64, Arc<dyn Kv>)>> = const { RefCell::new(None) };
}

/// A wall-clock time as a timestamp (µs since 2000-01-01).
pub fn from_system(t: SystemTime) -> i64 {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    since.as_micros() as i64 - UNIX_TO_PG_DAYS * USECS_PER_DAY
}

fn no_clock() -> Error {
    Error::internal("now() outside a statement")
}

/// The current transaction's timestamp (`now()`), fixed by [`with_now`].
pub fn now() -> Result<i64> {
    NOW.with(|n| n.borrow().as_ref().map(|(ts, _)| *ts))
        .ok_or_else(no_clock)
}

/// The time now (`clock_timestamp()`), from the database's clock
/// ([`Kv::wall_clock`]: never the host's directly, principle 4).
pub fn clock() -> Result<i64> {
    NOW.with(|n| {
        n.borrow()
            .as_ref()
            .map(|(_, kv)| from_system(kv.wall_clock()))
    })
    .ok_or_else(no_clock)
}

/// Unpredictable bytes from the database's platform ([`Kv::random_bytes`]).
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    NOW.with(|n| match n.borrow().as_ref() {
        Some((_, kv)) => {
            kv.random_bytes(buf);
            Ok(())
        }
        None => Err(no_clock()),
    })
}

/// The database the current statement runs in.
pub fn current_kv() -> Option<Arc<dyn Kv>> {
    NOW.with(|n| n.borrow().as_ref().map(|(_, kv)| kv.clone()))
}

/// Runs `f` with `now()` fixed at `ts` and `clock()` reading `kv`'s clock.
pub fn with_now<R>(ts: i64, kv: &Arc<dyn Kv>, f: impl FnOnce() -> R) -> R {
    let prev = NOW.with(|n| n.replace(Some((ts, kv.clone()))));
    let r = f();
    NOW.with(|n| n.replace(prev));
    r
}

// ----------------------------------------------------------------- input

fn bad_input(ty: &str, s: &str) -> Error {
    Error::datetime_format(format!("invalid input syntax for type {ty}: \"{s}\""))
}

/// Splits off a trailing ` BC` / ` AD`; returns the rest and whether BC.
fn era(s: &str) -> (&str, bool) {
    let lower = s.to_ascii_lowercase();
    if lower.ends_with(" bc") {
        (s[..s.len() - 3].trim_end(), true)
    } else if lower.ends_with(" ad") {
        (s[..s.len() - 3].trim_end(), false)
    } else {
        (s, false)
    }
}

/// `YYYY-MM-DD` → year, month, day.
fn parse_ymd(s: &str, bc: bool) -> Option<(i64, u32, u32)> {
    let mut parts = s.splitn(3, '-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    fn num(p: &str) -> Option<&str> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then_some(p)
    }
    let y: i64 = num(y)?.parse().ok()?;
    let y = if bc { 1 - y } else { y };
    Some((y, num(m)?.parse().ok()?, num(d)?.parse().ok()?))
}

/// Parses a date: `YYYY-MM-DD` (a time after it is ignored), `epoch`, an
/// optional ` BC`.
pub fn parse_date(s: &str) -> Result<i32> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("epoch") {
        return Ok(-UNIX_TO_PG_DAYS as i32);
    }
    let (t, bc) = era(t);
    let day = t.split([' ', 'T', 't']).next().unwrap_or("");
    let (y, m, d) = parse_ymd(day, bc).ok_or_else(|| bad_input("date", s))?;
    date_from_ymd(y, m, d)
}

/// `HH:MM[:SS[.ffffff]]` → microseconds of the day (24:00:00 allowed).
fn parse_time(t: &str) -> Option<i64> {
    let mut parts = t.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec = parts.next().unwrap_or("0");
    if parts.next().is_some() || !(0..60).contains(&m) {
        return None;
    }
    let (whole, frac) = match sec.split_once('.') {
        Some((w, f)) => (w, f),
        None => (sec, ""),
    };
    let s: i64 = whole.parse().ok()?;
    if !(0..60).contains(&s) {
        return None;
    }
    let us = frac_micros(frac)?;
    let total = h * USECS_PER_HOUR + m * USECS_PER_MINUTE + s * USECS_PER_SEC + us;
    (0..=USECS_PER_DAY).contains(&total).then_some(total)
}

/// A second's fraction (`5` → 500 000 µs), rounded half up beyond six
/// digits.
fn frac_micros(frac: &str) -> Option<i64> {
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut us: i64 = 0;
    for (i, b) in frac.bytes().enumerate() {
        let digit = i64::from(b - b'0');
        match i.cmp(&6) {
            Ordering::Less => us = us * 10 + digit,
            Ordering::Equal => {
                if digit >= 5 {
                    us += 1;
                }
            }
            Ordering::Greater => {}
        }
    }
    for _ in frac.len()..6 {
        us *= 10;
    }
    Some(us)
}

/// A zone suffix: `Z`, `UTC`, `GMT`, `±HH`, `±HH:MM`, `±HHMM` → seconds
/// east of UTC.
fn parse_zone(z: &str) -> Option<i64> {
    let z = z.trim();
    if z.is_empty() {
        return Some(0);
    }
    if ["z", "utc", "gmt", "zulu"].contains(&z.to_ascii_lowercase().as_str()) {
        return Some(0);
    }
    let (sign, rest) = match z.as_bytes()[0] {
        b'+' => (1, &z[1..]),
        b'-' => (-1, &z[1..]),
        _ => return None,
    };
    let (h, m) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        rest.split_at(2)
    } else {
        (rest, "0")
    };
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    if h > 15 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

/// Parses a timestamp: `YYYY-MM-DD[( |T)HH:MM[:SS[.f]]][zone]`, `epoch`,
/// an optional ` BC`.  `with_tz`: the zone is applied (else ignored).
pub fn parse_timestamp(s: &str, with_tz: bool) -> Result<i64> {
    let ty = if with_tz {
        "timestamp with time zone"
    } else {
        "timestamp without time zone"
    };
    let t = s.trim();
    if t.eq_ignore_ascii_case("epoch") {
        return Ok(-UNIX_TO_PG_DAYS * USECS_PER_DAY);
    }
    let (t, bc) = era(t);
    let split = t.find([' ', 'T', 't']).unwrap_or(t.len());
    let (day, rest) = (&t[..split], t[split..].trim_start_matches([' ', 'T', 't']));
    let (y, m, d) = parse_ymd(day, bc).ok_or_else(|| bad_input(ty, s))?;
    let date = date_from_ymd(y, m, d)?;
    // The time runs up to the zone (a sign, a letter or a space).
    let end = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == ':' || *c == '.'))
        .map_or(rest.len(), |(i, _)| i);
    let (time, zone) = rest.split_at(end);
    let micros = if time.is_empty() {
        0
    } else {
        parse_time(time).ok_or_else(|| bad_input(ty, s))?
    };
    let offset = parse_zone(zone).ok_or_else(|| bad_input(ty, s))?;
    let local = date_to_ts(date) + micros;
    check_ts(if with_tz {
        local - offset * USECS_PER_SEC
    } else {
        local
    })
}

// ---------------------------------------------------------------- output

fn year_text(y: i64) -> (String, &'static str) {
    if y <= 0 {
        (format!("{:04}", 1 - y), " BC")
    } else {
        (format!("{y:04}"), "")
    }
}

/// A date as `YYYY-MM-DD`.
pub fn format_date(date: i32) -> String {
    let (y, m, d) = ymd(date);
    let (year, bc) = year_text(y);
    format!("{year}-{m:02}-{d:02}{bc}")
}

fn seconds_text(sec: u32, micros: u32) -> String {
    if micros == 0 {
        format!("{sec:02}")
    } else {
        let frac = format!("{micros:06}");
        format!("{sec:02}.{}", frac.trim_end_matches('0'))
    }
}

/// A timestamp as `YYYY-MM-DD HH:MM:SS[.ffffff]` (and `+00` with a zone).
pub fn format_timestamp(ts: i64, with_tz: bool) -> String {
    let f = fields(ts);
    let (year, bc) = year_text(f.year);
    format!(
        "{year}-{:02}-{:02} {:02}:{:02}:{}{}{bc}",
        f.month,
        f.day,
        f.hour,
        f.minute,
        seconds_text(f.second, f.micros),
        if with_tz { "+00" } else { "" }
    )
}

// ------------------------------------------------------------------ time

/// Parses a time of day (`time`): `HH:MM[:SS[.f]]`, `HHMM`, `HHMMSS`, an
/// optional `AM` / `PM`, `allballs`; a zone after it is ignored, as
/// PostgreSQL's `time` does.  `24:00:00` is the end of the day.
pub fn parse_time_of_day(s: &str) -> Result<i64> {
    let bad = || Error::datetime_format(format!("invalid input syntax for type time: \"{s}\""));
    let range = || Error::datetime_overflow(format!("date/time field value out of range: \"{s}\""));
    let lower = s.trim().to_ascii_lowercase();
    if lower == "allballs" {
        return Ok(0);
    }
    let (body, pm) = if let Some(b) = lower.strip_suffix("am") {
        (b.trim_end(), Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b.trim_end(), Some(true))
    } else {
        (lower.as_str(), None)
    };
    let end = body
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == ':' || *c == '.'))
        .map_or(body.len(), |(i, _)| i);
    let (time, zone) = body.split_at(end);
    if time.is_empty() || parse_zone(zone).is_none() {
        return Err(bad());
    }
    let (clock, frac) = time.split_once('.').unwrap_or((time, ""));
    let parts: Vec<&str> = if clock.contains(':') {
        clock.split(':').collect()
    } else if clock.len() == 4 {
        vec![&clock[..2], &clock[2..]]
    } else if clock.len() == 6 {
        vec![&clock[..2], &clock[2..4], &clock[4..]]
    } else {
        return Err(bad());
    };
    if !(2..=3).contains(&parts.len()) || (parts.len() == 2 && !frac.is_empty()) {
        return Err(bad());
    }
    let num = |p: &str| -> Result<i64> {
        if p.is_empty() || p.len() > 9 {
            return Err(bad());
        }
        p.parse().map_err(|_| bad())
    };
    let (mut h, m) = (num(parts[0])?, num(parts[1])?);
    let sec = parts.get(2).map(|p| num(p)).transpose()?.unwrap_or(0);
    let us = frac_micros(frac).ok_or_else(bad)?;
    if m > 59 || sec > 60 {
        return Err(range());
    }
    if let Some(pm) = pm {
        if !(1..=12).contains(&h) {
            return Err(range());
        }
        h = h % 12 + if pm { 12 } else { 0 };
    }
    let total = h * USECS_PER_HOUR + m * USECS_PER_MINUTE + sec * USECS_PER_SEC + us;
    if total > USECS_PER_DAY {
        return Err(range());
    }
    Ok(total)
}

/// A time of day as `HH:MM:SS[.ffffff]`.
pub fn format_time(t: i64) -> String {
    let (h, m) = (t / USECS_PER_HOUR, t / USECS_PER_MINUTE % 60);
    let (s, us) = (t / USECS_PER_SEC % 60, t % USECS_PER_SEC);
    format!("{h:02}:{m:02}:{}", seconds_text(s as u32, us as u32))
}

/// `time + interval`: the interval's time part, around the clock (its
/// months and days do not move a time of day).
pub fn time_add(t: i64, iv: Interval) -> i64 {
    (i128::from(t) + i128::from(iv.micros)).rem_euclid(i128::from(USECS_PER_DAY)) as i64
}

/// A timestamp's time of day.
pub fn time_of_ts(ts: i64) -> i64 {
    ts.rem_euclid(USECS_PER_DAY)
}

/// `make_time(hour, min, sec)`.
pub fn make_time(h: i64, m: i64, s: f64) -> Result<i64> {
    let us = (s * 1e6).round();
    let total = h
        .checked_mul(USECS_PER_HOUR)
        .and_then(|x| x.checked_add(m.checked_mul(USECS_PER_MINUTE)?));
    match total {
        Some(t)
            if (0..=24).contains(&h)
                && (0..60).contains(&m)
                && (0.0..=60e6).contains(&us)
                && t + us as i64 <= USECS_PER_DAY =>
        {
            Ok(t + us as i64)
        }
        _ => Err(Error::datetime_overflow(format!(
            "time field value out of range: {h:02}:{m:02}:{}",
            if s.fract() == 0.0 {
                format!("{:02}", s as i64)
            } else {
                format!("{s:09.6}")
            }
        ))),
    }
}

/// `extract(unit FROM time)`.
pub fn extract_time(unit: &str, t: i64) -> Result<Decimal> {
    let unit = canonical_unit(unit);
    Ok(match unit.as_str() {
        "epoch" => scaled(t, 6),
        "microseconds" => int(t % USECS_PER_MINUTE),
        "milliseconds" => scaled(t % USECS_PER_MINUTE, 3),
        "second" => scaled(t % USECS_PER_MINUTE, 6),
        "minute" => int(t / USECS_PER_MINUTE % 60),
        "hour" => int(t / USECS_PER_HOUR),
        _ => return Err(unsupported(&unit, "time without time zone")),
    })
}

// -------------------------------------------------------------- interval

/// An interval.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interval {
    /// Months.
    pub months: i32,
    /// Days.
    pub days: i32,
    /// Microseconds.
    pub micros: i64,
}

fn interval_overflow() -> Error {
    Error::datetime_overflow("interval out of range")
}

impl Interval {
    /// For comparison: months as 30 days, days as 24 hours.
    pub fn span(self) -> i128 {
        (i128::from(self.months) * 30 + i128::from(self.days)) * i128::from(USECS_PER_DAY)
            + i128::from(self.micros)
    }

    /// `-self`.
    pub fn checked_neg(self) -> Result<Self> {
        Ok(Interval {
            months: self.months.checked_neg().ok_or_else(interval_overflow)?,
            days: self.days.checked_neg().ok_or_else(interval_overflow)?,
            micros: self.micros.checked_neg().ok_or_else(interval_overflow)?,
        })
    }

    /// `self + other`, field by field.
    pub fn checked_add(self, other: Self) -> Result<Self> {
        Ok(Interval {
            months: self
                .months
                .checked_add(other.months)
                .ok_or_else(interval_overflow)?,
            days: self
                .days
                .checked_add(other.days)
                .ok_or_else(interval_overflow)?,
            micros: self
                .micros
                .checked_add(other.micros)
                .ok_or_else(interval_overflow)?,
        })
    }

    /// `self - other`.
    pub fn checked_sub(self, other: Self) -> Result<Self> {
        self.checked_add(other.checked_neg()?)
    }

    /// `self × factor`, with fractional months spilling into days and
    /// fractional days into time (PostgreSQL's `interval_mul`).
    pub fn checked_mul(self, factor: f64) -> Result<Self> {
        let round6 = |x: f64| (x * 1e6).round() / 1e6;
        let months_f = f64::from(self.months) * factor;
        let days_f = f64::from(self.days) * factor;
        if !months_f.is_finite()
            || months_f.abs() > f64::from(i32::MAX)
            || days_f.abs() > f64::from(i32::MAX)
        {
            return Err(interval_overflow());
        }
        let months = months_f.trunc() as i32;
        let mut days = days_f.trunc() as i32;
        let month_remainder_days = round6((months_f - f64::from(months)) * 30.0);
        let mut sec_remainder = round6(
            (days_f - f64::from(days) + month_remainder_days - month_remainder_days.trunc())
                * 86_400.0,
        );
        if sec_remainder.abs() >= 86_400.0 {
            let whole = (sec_remainder / 86_400.0).trunc();
            days = days.saturating_add(whole as i32);
            sec_remainder -= whole * 86_400.0;
        }
        days = days
            .checked_add(month_remainder_days.trunc() as i32)
            .ok_or_else(interval_overflow)?;
        let micros = (self.micros as f64 * factor + sec_remainder * 1e6).round();
        if !micros.is_finite() || micros.abs() >= i64::MAX as f64 {
            return Err(interval_overflow());
        }
        Ok(Interval {
            months,
            days,
            micros: micros as i64,
        })
    }

    /// `self / divisor`.
    pub fn checked_div(self, divisor: f64) -> Result<Self> {
        if divisor == 0.0 {
            return Err(Error::division_by_zero("division by zero"));
        }
        self.checked_mul(1.0 / divisor)
    }

    /// Whole days moved out of the time part (`justify_hours`), keeping
    /// the signs of the two parts consistent.
    pub fn justify_hours(self) -> Self {
        let mut r = self;
        let whole = r.micros / USECS_PER_DAY;
        r.days = r.days.saturating_add(whole as i32);
        r.micros -= whole * USECS_PER_DAY;
        if r.days > 0 && r.micros < 0 {
            r.micros += USECS_PER_DAY;
            r.days -= 1;
        } else if r.days < 0 && r.micros > 0 {
            r.micros -= USECS_PER_DAY;
            r.days += 1;
        }
        r
    }
}

impl PartialEq for Interval {
    fn eq(&self, other: &Self) -> bool {
        self.span() == other.span()
    }
}

impl Eq for Interval {}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        self.span().cmp(&other.span())
    }
}

impl fmt::Display for Interval {
    /// PostgreSQL's `postgres` interval style: `1 year 2 mons 3 days
    /// 04:05:06`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        let mut is_zero = true;
        let mut is_before = false;
        let mut part = |v: i64, unit: &str, out: &mut String| {
            if v == 0 {
                return;
            }
            if !is_zero {
                out.push(' ');
            }
            let plus = if is_before && v > 0 { "+" } else { "" };
            let s = if v != 1 { "s" } else { "" };
            out.push_str(&format!("{plus}{v} {unit}{s}"));
            is_before = v < 0;
            is_zero = false;
        };
        let (years, months) = (self.months / 12, self.months % 12);
        part(i64::from(years), "year", &mut out);
        part(i64::from(months), "mon", &mut out);
        part(i64::from(self.days), "day", &mut out);
        if is_zero || self.micros != 0 {
            let t = self.micros;
            let minus = t < 0;
            let a = t.unsigned_abs();
            let (h, m) = (
                a / USECS_PER_HOUR as u64,
                a % USECS_PER_HOUR as u64 / USECS_PER_MINUTE as u64,
            );
            let s = (a % USECS_PER_MINUTE as u64 / USECS_PER_SEC as u64) as u32;
            let us = (a % USECS_PER_SEC as u64) as u32;
            let sign = if minus {
                "-"
            } else if is_before {
                "+"
            } else {
                ""
            };
            out.push_str(&format!(
                "{}{sign}{h:02}:{m:02}:{}",
                if is_zero { "" } else { " " },
                seconds_text(s, us)
            ));
        }
        f.write_str(&out)
    }
}

/// `amount` of `unit` as an interval (fractions spill into smaller units,
/// as PostgreSQL's input does).
fn unit_interval(amount: f64, unit: &str) -> Option<Interval> {
    let spill_days = |days: f64| -> Interval {
        let whole = days.trunc();
        Interval {
            months: 0,
            days: whole as i32,
            micros: ((days - whole) * USECS_PER_DAY as f64).round() as i64,
        }
    };
    let time = |usecs: f64| Interval {
        months: 0,
        days: 0,
        micros: usecs.round() as i64,
    };
    Some(match unit {
        "millennium" | "millennia" | "millenniums" | "mil" | "mils" => Interval {
            months: (amount * 12_000.0).round() as i32,
            ..Interval::default()
        },
        "century" | "centuries" | "c" | "cent" => Interval {
            months: (amount * 1200.0).round() as i32,
            ..Interval::default()
        },
        "decade" | "decades" | "dec" | "decs" => Interval {
            months: (amount * 120.0).round() as i32,
            ..Interval::default()
        },
        "year" | "years" | "y" | "yr" | "yrs" => Interval {
            months: (amount * 12.0).round() as i32,
            ..Interval::default()
        },
        "month" | "months" | "mon" | "mons" => {
            let whole = amount.trunc();
            let d = spill_days((amount - whole) * 30.0);
            Interval {
                months: whole as i32,
                ..d
            }
        }
        "week" | "weeks" | "w" => spill_days(amount * 7.0),
        "day" | "days" | "d" => spill_days(amount),
        "hour" | "hours" | "h" | "hr" | "hrs" => time(amount * USECS_PER_HOUR as f64),
        "minute" | "minutes" | "m" | "min" | "mins" => time(amount * USECS_PER_MINUTE as f64),
        "second" | "seconds" | "s" | "sec" | "secs" => time(amount * USECS_PER_SEC as f64),
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => time(amount * 1000.0),
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => time(amount),
        _ => return None,
    })
}

/// ISO 8601 `P[nY][nM][nW][nD][T[nH][nM][nS]]`.
fn parse_iso_interval(t: &str) -> Option<Interval> {
    let mut acc = Interval::default();
    let mut in_time = false;
    let mut num = String::new();
    for c in t[1..].chars() {
        match c {
            'T' | 't' => in_time = true,
            '0'..='9' | '.' | '-' => num.push(c),
            _ => {
                let v: f64 = num.parse().ok()?;
                num.clear();
                let unit = match (c.to_ascii_uppercase(), in_time) {
                    ('Y', false) => "year",
                    ('M', false) => "month",
                    ('W', false) => "week",
                    ('D', false) => "day",
                    ('H', true) => "hour",
                    ('M', true) => "minute",
                    ('S', true) => "second",
                    _ => return None,
                };
                acc = acc.checked_add(unit_interval(v, unit)?).ok()?;
            }
        }
    }
    num.is_empty().then_some(acc)
}

/// Parses an interval: `N unit …` (units from microseconds to millennia,
/// fractions allowed), `[-]H:M[:S]`, `@ … ago`, or ISO 8601 `P…`.
pub fn parse_interval(s: &str) -> Result<Interval> {
    let bad = || bad_input("interval", s);
    let t = s.trim();
    if t.starts_with(['P', 'p']) && t.len() > 1 {
        return parse_iso_interval(t).ok_or_else(bad);
    }
    let lower = t.to_ascii_lowercase();
    let mut words: Vec<&str> = lower.split_whitespace().collect();
    if words.first() == Some(&"@") {
        words.remove(0);
    }
    let ago = words.last() == Some(&"ago");
    if ago {
        words.pop();
    }
    if words.is_empty() {
        return Err(bad());
    }
    let mut acc = Interval::default();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        i += 1;
        if w.contains(':') {
            let (neg, body) = match w.as_bytes()[0] {
                b'-' => (true, &w[1..]),
                b'+' => (false, &w[1..]),
                _ => (false, w),
            };
            // H:M or H:M:S; hours may exceed 24.
            let mut parts = body.splitn(2, ':');
            let h: i64 = parts.next().and_then(|h| h.parse().ok()).ok_or_else(bad)?;
            let rest = parts.next().ok_or_else(bad)?;
            let ms = parse_time(&format!("0:{rest}")).ok_or_else(bad)?;
            let us = h
                .checked_mul(USECS_PER_HOUR)
                .and_then(|x| x.checked_add(ms))
                .ok_or_else(interval_overflow)?;
            acc = acc.checked_add(Interval {
                months: 0,
                days: 0,
                micros: if neg { -us } else { us },
            })?;
            continue;
        }
        // Numbers, each with its unit: attached (`1h30m`) or the next word.
        let mut rest = w;
        while !rest.is_empty() {
            let split = rest
                .find(|c: char| c.is_ascii_alphabetic())
                .unwrap_or(rest.len());
            let (num, after) = rest.split_at(split);
            let amount: f64 = num.parse().map_err(|_| bad())?;
            let unit_len = after
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(after.len());
            let (attached, tail) = after.split_at(unit_len);
            rest = tail;
            let unit = if !attached.is_empty() {
                attached
            } else if i < words.len() && words[i].chars().all(|c| c.is_ascii_alphabetic()) {
                i += 1;
                words[i - 1]
            } else {
                "second"
            };
            acc = acc.checked_add(unit_interval(amount, unit).ok_or_else(bad)?)?;
        }
    }
    if ago { acc.checked_neg() } else { Ok(acc) }
}

// ------------------------------------------------------------ arithmetic

/// `date + days`.
pub fn date_add_days(date: i32, days: i64) -> Result<i32> {
    check_date(
        i64::from(date)
            .checked_add(days)
            .ok_or_else(|| Error::datetime_overflow("date out of range"))?,
    )
}

/// `timestamp + interval`: months by the calendar (the day clamped to the
/// month's end), then days, then time.
pub fn ts_add(ts: i64, iv: Interval) -> Result<i64> {
    let overflow = || Error::datetime_overflow("timestamp out of range");
    let mut ts = ts;
    if iv.months != 0 {
        let f = fields(ts);
        let total = f.year * 12 + i64::from(f.month) - 1 + i64::from(iv.months);
        let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
            return Err(overflow());
        }
        let d = f.day.min(days_in_month(y, m));
        let date = date_from_ymd(y, m, d)?;
        ts = date_to_ts(date) + ts.rem_euclid(USECS_PER_DAY);
    }
    let ts = ts
        .checked_add(
            i64::from(iv.days)
                .checked_mul(USECS_PER_DAY)
                .ok_or_else(overflow)?,
        )
        .and_then(|t| t.checked_add(iv.micros))
        .ok_or_else(overflow)?;
    check_ts(ts)
}

/// `a - b` for timestamps: the difference in days and time.
pub fn ts_diff(a: i64, b: i64) -> Result<Interval> {
    let micros = a
        .checked_sub(b)
        .ok_or_else(|| Error::datetime_overflow("interval out of range"))?;
    Ok(Interval {
        months: 0,
        days: 0,
        micros,
    }
    .justify_hours())
}

/// A timestamp from microseconds since 1970-01-01 UTC.
pub fn from_unix_micros(us: i64) -> Result<i64> {
    check_ts(
        us.checked_sub(UNIX_TO_PG_DAYS * USECS_PER_DAY)
            .ok_or_else(|| Error::datetime_overflow("timestamp out of range"))?,
    )
}

/// Whole months moved out of the days (`justify_days`: 30 days a month),
/// the signs kept consistent.
pub fn justify_days(i: Interval) -> Interval {
    let mut r = i;
    let whole = r.days / 30;
    r.months = r.months.saturating_add(whole);
    r.days -= whole * 30;
    if r.months > 0 && r.days < 0 {
        r.days += 30;
        r.months -= 1;
    } else if r.months < 0 && r.days > 0 {
        r.days -= 30;
        r.months += 1;
    }
    r
}

/// `justify_interval`: whole days out of the time, whole months out of the
/// days, all parts given one sign (PostgreSQL's `interval_justify_interval`).
pub fn justify_interval(i: Interval) -> Interval {
    let mut r = i;
    let whole_day = r.micros / USECS_PER_DAY;
    r.micros -= whole_day * USECS_PER_DAY;
    r.days = r.days.saturating_add(whole_day as i32);
    let whole_month = r.days / 30;
    r.days -= whole_month * 30;
    r.months = r.months.saturating_add(whole_month);
    if r.months > 0 && (r.days < 0 || (r.days == 0 && r.micros < 0)) {
        r.days += 30;
        r.months -= 1;
    } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.micros > 0)) {
        r.days -= 30;
        r.months += 1;
    }
    if r.days > 0 && r.micros < 0 {
        r.micros += USECS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= USECS_PER_DAY;
        r.days += 1;
    }
    r
}

/// `age(a, b)`: `a - b` in years, months, days and time, by the calendar
/// (PostgreSQL's `timestamp_age`).
pub fn age(a: i64, b: i64) -> Interval {
    let (f1, f2) = (fields(a), fields(b));
    let mut us = i64::from(f1.micros) - i64::from(f2.micros);
    let mut sec = i64::from(f1.second) - i64::from(f2.second);
    let mut min = i64::from(f1.minute) - i64::from(f2.minute);
    let mut hour = i64::from(f1.hour) - i64::from(f2.hour);
    let mut day = i64::from(f1.day) - i64::from(f2.day);
    let mut mon = i64::from(f1.month) - i64::from(f2.month);
    let mut year = f1.year - f2.year;
    let flip = a < b;
    if flip {
        for v in [
            &mut us, &mut sec, &mut min, &mut hour, &mut day, &mut mon, &mut year,
        ] {
            *v = -*v;
        }
    }
    while us < 0 {
        us += USECS_PER_SEC;
        sec -= 1;
    }
    while sec < 0 {
        sec += 60;
        min -= 1;
    }
    while min < 0 {
        min += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        day -= 1;
    }
    while day < 0 {
        let (y, m) = if flip {
            (f1.year, f1.month)
        } else {
            (f2.year, f2.month)
        };
        day += i64::from(days_in_month(y, m));
        mon -= 1;
    }
    while mon < 0 {
        mon += 12;
        year -= 1;
    }
    if flip {
        for v in [
            &mut us, &mut sec, &mut min, &mut hour, &mut day, &mut mon, &mut year,
        ] {
            *v = -*v;
        }
    }
    Interval {
        months: (year * 12 + mon) as i32,
        days: day as i32,
        micros: hour * USECS_PER_HOUR + min * USECS_PER_MINUTE + sec * USECS_PER_SEC + us,
    }
}

// ---------------------------------------------------------------- fields

/// The day of the week, 0 = Sunday.
fn dow(date: i32) -> i64 {
    // 2000-01-01 was a Saturday.
    (i64::from(date) + 6).rem_euclid(7)
}

fn doy(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d) - days_from_civil(y, 1, 1) + 1
}

/// ISO 8601 week-numbering year and week.
fn iso_week(date: i32) -> (i64, i64) {
    let (y, m, d) = ymd(date);
    let isodow = (dow(date) + 6) % 7 + 1; // Monday = 1
    let week = (doy(y, m, d) - isodow + 10) / 7;
    let weeks_in = |y: i64| {
        let jan1 = (days_from_civil(y, 1, 1) - UNIX_TO_PG_DAYS) as i32;
        let wd = (dow(jan1) + 6) % 7 + 1;
        if wd == 4 || (wd == 3 && is_leap(y)) {
            53
        } else {
            52
        }
    };
    if week < 1 {
        (y - 1, weeks_in(y - 1))
    } else if week > weeks_in(y) {
        (y + 1, 1)
    } else {
        (y, week)
    }
}

fn int(v: i64) -> Decimal {
    Decimal::from_i64(v)
}

fn scaled(v: i64, scale: u32) -> Decimal {
    Decimal::new(i128::from(v), scale).expect("in range")
}

fn unsupported(unit: &str, ty: &str) -> Error {
    Error::feature(format!("unit \"{unit}\" not supported for type {ty}"))
}

fn date_field(unit: &str, date: i32, ty: &str) -> Result<Option<Decimal>> {
    let (y, m, d) = ymd(date);
    Ok(Some(match unit {
        "year" | "years" | "y" => int(y),
        "month" | "months" | "mon" => int(i64::from(m)),
        "day" | "days" | "d" => int(i64::from(d)),
        "quarter" => int((i64::from(m) - 1) / 3 + 1),
        "week" => int(iso_week(date).1),
        "isoyear" => int(iso_week(date).0),
        "dow" => int(dow(date)),
        "isodow" => int((dow(date) + 6) % 7 + 1),
        "doy" => int(doy(y, m, d)),
        "decade" => int(y.div_euclid(10)),
        "century" => int(if y > 0 {
            (y + 99) / 100
        } else {
            -((99 - (y - 1)) / 100)
        }),
        "millennium" => int(if y > 0 {
            (y + 999) / 1000
        } else {
            -((999 - (y - 1)) / 1000)
        }),
        "julian" => int(i64::from(date) + 2_451_545),
        _ => {
            let _ = ty;
            return Ok(None);
        }
    }))
}

fn canonical_unit(unit: &str) -> String {
    let u = unit.trim().to_ascii_lowercase();
    match u.as_str() {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => "microseconds".into(),
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => "milliseconds".into(),
        "second" | "seconds" | "s" | "sec" | "secs" => "second".into(),
        "minute" | "minutes" | "m" | "min" | "mins" => "minute".into(),
        "hour" | "hours" | "h" | "hr" | "hrs" => "hour".into(),
        "day" | "days" | "d" => "day".into(),
        "week" | "weeks" | "w" => "week".into(),
        "month" | "months" | "mon" | "mons" => "month".into(),
        "year" | "years" | "y" | "yr" | "yrs" => "year".into(),
        "decade" | "decades" | "dec" | "decs" => "decade".into(),
        "century" | "centuries" | "c" | "cent" => "century".into(),
        "millennium" | "millennia" | "millenniums" | "mil" | "mils" => "millennium".into(),
        _ => u,
    }
}

/// `extract(unit FROM date)`.
pub fn extract_date(unit: &str, date: i32) -> Result<Decimal> {
    let unit = canonical_unit(unit);
    if unit == "epoch" {
        return Ok(int((i64::from(date) + UNIX_TO_PG_DAYS) * 86_400));
    }
    if matches!(
        unit.as_str(),
        "hour" | "minute" | "second" | "milliseconds" | "microseconds"
    ) {
        return Err(unsupported(&unit, "date"));
    }
    date_field(&unit, date, "date")?.ok_or_else(|| unsupported(&unit, "date"))
}

/// `extract(unit FROM timestamp)`.
pub fn extract_ts(unit: &str, ts: i64, with_tz: bool) -> Result<Decimal> {
    let ty = if with_tz {
        "timestamp with time zone"
    } else {
        "timestamp without time zone"
    };
    let unit = canonical_unit(unit);
    let f = fields(ts);
    let usec_of_minute = i64::from(f.second) * USECS_PER_SEC + i64::from(f.micros);
    Ok(match unit.as_str() {
        "epoch" => scaled(ts + UNIX_TO_PG_DAYS * USECS_PER_DAY, 6),
        "microseconds" => int(usec_of_minute),
        "milliseconds" => scaled(usec_of_minute, 3),
        "second" => scaled(usec_of_minute, 6),
        "minute" => int(i64::from(f.minute)),
        "hour" => int(i64::from(f.hour)),
        "timezone" | "timezone_hour" | "timezone_minute" if with_tz => int(0),
        _ => date_field(&unit, ts_to_date(ts), ty)?.ok_or_else(|| unsupported(&unit, ty))?,
    })
}

/// `extract(unit FROM interval)`.
pub fn extract_interval(unit: &str, iv: Interval) -> Result<Decimal> {
    let unit = canonical_unit(unit);
    let t = iv.micros;
    Ok(match unit.as_str() {
        "epoch" => {
            let m = i64::from(iv.months);
            let secs = (1461 * (m / 12) + 120 * (m % 12) + 4 * i64::from(iv.days)) * (86_400 / 4);
            let micros = i128::from(secs) * i128::from(USECS_PER_SEC) + i128::from(t);
            Decimal::new(micros, 6)?
        }
        "microseconds" => int(t % USECS_PER_MINUTE),
        "milliseconds" => scaled(t % USECS_PER_MINUTE, 3),
        "second" => scaled(t % USECS_PER_MINUTE, 6),
        "minute" => int(t / USECS_PER_MINUTE % 60),
        "hour" => int(t / USECS_PER_HOUR),
        "day" => int(i64::from(iv.days)),
        "month" => int(i64::from(iv.months % 12)),
        "quarter" => int(i64::from(iv.months % 12) / 3 + 1),
        "year" => int(i64::from(iv.months / 12)),
        "decade" => int(i64::from(iv.months / 120)),
        "century" => int(i64::from(iv.months / 1200)),
        "millennium" => int(i64::from(iv.months / 12_000)),
        _ => return Err(unsupported(&unit, "interval")),
    })
}

/// `date_trunc(unit, timestamp)`.
pub fn date_trunc(unit: &str, ts: i64) -> Result<i64> {
    let unit = canonical_unit(unit);
    let f = fields(ts);
    let date = ts_to_date(ts);
    let at = |y: i64, m: u32, d: u32| -> Result<i64> { Ok(date_to_ts(date_from_ymd(y, m, d)?)) };
    let day_start = date_to_ts(date);
    Ok(match unit.as_str() {
        "microseconds" => ts,
        "milliseconds" => ts - ts.rem_euclid(1000),
        "second" => ts - ts.rem_euclid(USECS_PER_SEC),
        "minute" => ts - ts.rem_euclid(USECS_PER_MINUTE),
        "hour" => ts - ts.rem_euclid(USECS_PER_HOUR),
        "day" => day_start,
        "week" => date_to_ts(date - ((dow(date) + 6) % 7) as i32),
        "month" => at(f.year, f.month, 1)?,
        "quarter" => at(f.year, (f.month - 1) / 3 * 3 + 1, 1)?,
        "year" => at(f.year, 1, 1)?,
        "decade" => at(f.year.div_euclid(10) * 10, 1, 1)?,
        "century" => {
            let c = if f.year > 0 {
                (f.year - 1) / 100 * 100 + 1
            } else {
                f.year
            };
            at(c, 1, 1)?
        }
        "millennium" => {
            let c = if f.year > 0 {
                (f.year - 1) / 1000 * 1000 + 1
            } else {
                f.year
            };
            at(c, 1, 1)?
        }
        _ => {
            return Err(Error::feature(format!(
                "unit \"{unit}\" not supported for type timestamp"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        for days in (-2_500_000..2_500_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(date_from_ymd(2000, 1, 1).unwrap(), 0);
        assert_eq!(dow(0), 6);
        assert_eq!(
            parse_date("2024-02-29").unwrap(),
            date_from_ymd(2024, 2, 29).unwrap()
        );
        assert!(parse_date("2023-02-29").is_err());
        assert_eq!(format_date(parse_date("1999-12-31").unwrap()), "1999-12-31");
        assert_eq!(
            format_date(parse_date("0044-03-15 BC").unwrap()),
            "0044-03-15 BC"
        );
        assert_eq!(format_date(parse_date("epoch").unwrap()), "1970-01-01");
    }

    #[test]
    fn timestamps() {
        let ts = parse_timestamp("2024-03-10 14:05:06.5", false).unwrap();
        assert_eq!(format_timestamp(ts, false), "2024-03-10 14:05:06.5");
        let tz = parse_timestamp("2024-03-10T14:05:06+02:00", true).unwrap();
        assert_eq!(format_timestamp(tz, true), "2024-03-10 12:05:06+00");
        assert_eq!(
            parse_timestamp("2024-03-10 14:05:06+02", false).unwrap(),
            parse_timestamp("2024-03-10 14:05:06", false).unwrap()
        );
        assert_eq!(
            format_timestamp(
                parse_timestamp("2024-01-01 23:59:59.9999995", false).unwrap(),
                false
            ),
            "2024-01-02 00:00:00"
        );
        assert_eq!(
            format_timestamp(parse_timestamp("2024-01-01 24:00", false).unwrap(), false),
            "2024-01-02 00:00:00"
        );
        assert!(parse_timestamp("2024-01-01 25:00", false).is_err());
        assert_eq!(
            parse_timestamp("2024-01-01 x", true).unwrap_err().code,
            "22007"
        );
    }

    #[test]
    fn intervals() {
        for (input, out) in [
            ("1 day", "1 day"),
            ("2 days 3 hours", "2 days 03:00:00"),
            (
                "1 year 2 months 3 days 04:05:06.789",
                "1 year 2 mons 3 days 04:05:06.789",
            ),
            ("-1 day", "-1 days"),
            ("1 day -01:00:00", "1 day -01:00:00"),
            ("-1 days +02:00:00", "-1 days +02:00:00"),
            ("0", "00:00:00"),
            ("90 minutes", "01:30:00"),
            ("1.5 days", "1 day 12:00:00"),
            ("1.5 months", "1 mon 15 days"),
            ("1.5 years", "1 year 6 mons"),
            ("2 weeks", "14 days"),
            ("36:00:00", "36:00:00"),
            ("@ 3 days ago", "-3 days"),
            ("P1Y2M3DT4H5M6S", "1 year 2 mons 3 days 04:05:06"),
            ("10 sec", "00:00:10"),
            ("1h30m", "01:30:00"),
        ] {
            assert_eq!(parse_interval(input).unwrap().to_string(), out, "{input}");
        }
        assert!(parse_interval("1 fortnight").is_err());
        assert_eq!(
            parse_interval("1 mon").unwrap(),
            parse_interval("30 days").unwrap()
        );
        assert!(parse_interval("1 day").unwrap() > parse_interval("23:59:59").unwrap());
        let mul = parse_interval("1 mon 1 day")
            .unwrap()
            .checked_mul(1.5)
            .unwrap();
        assert_eq!(mul.to_string(), "1 mon 16 days 12:00:00");
        assert_eq!(
            ts_diff(
                parse_timestamp("2024-03-02 01:00", false).unwrap(),
                parse_timestamp("2024-03-01 00:00", false).unwrap()
            )
            .unwrap()
            .to_string(),
            "1 day 01:00:00"
        );
        let jan31 = parse_timestamp("2024-01-31", false).unwrap();
        let feb = ts_add(jan31, parse_interval("1 month").unwrap()).unwrap();
        assert_eq!(format_timestamp(feb, false), "2024-02-29 00:00:00");
    }

    #[test]
    fn fields_and_truncation() {
        let ts = parse_timestamp("2024-12-30 13:14:15.25", false).unwrap();
        assert_eq!(extract_ts("year", ts, false).unwrap().to_string(), "2024");
        assert_eq!(extract_ts("week", ts, false).unwrap().to_string(), "1");
        assert_eq!(
            extract_ts("isoyear", ts, false).unwrap().to_string(),
            "2025"
        );
        assert_eq!(extract_ts("dow", ts, false).unwrap().to_string(), "1");
        assert_eq!(extract_ts("doy", ts, false).unwrap().to_string(), "365");
        assert_eq!(
            extract_ts("second", ts, false).unwrap().to_string(),
            "15.250000"
        );
        assert_eq!(
            extract_ts("milliseconds", ts, false).unwrap().to_string(),
            "15250.000"
        );
        assert_eq!(
            extract_ts(
                "epoch",
                parse_timestamp("1970-01-02", false).unwrap(),
                false
            )
            .unwrap()
            .to_string(),
            "86400.000000"
        );
        assert!(extract_date("hour", 0).is_err());
        assert_eq!(
            format_timestamp(date_trunc("week", ts).unwrap(), false),
            "2024-12-30 00:00:00"
        );
        assert_eq!(
            format_timestamp(date_trunc("quarter", ts).unwrap(), false),
            "2024-10-01 00:00:00"
        );
        assert_eq!(
            format_timestamp(date_trunc("century", ts).unwrap(), false),
            "2001-01-01 00:00:00"
        );
        let iv = parse_interval("1 year 2 mons 3 days 04:05:06").unwrap();
        assert_eq!(
            extract_interval("epoch", iv).unwrap().to_string(),
            "37015506.000000"
        );
        assert_eq!(extract_interval("hour", iv).unwrap().to_string(), "4");
    }
}
