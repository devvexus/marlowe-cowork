//! A 5-field cron parser and next-fire calculator (BG5), in-house: minute, hour, day of month,
//! month, day of week, in the machine's local time.
//!
//! Fields take `*`, `?` (same as `*`), numbers, ranges `a-b`, steps `*/n` and `a-b/n` and `a/n`,
//! comma lists, and three-letter month and weekday names. Weekday 7 is Sunday. As in Vixie cron,
//! when both day-of-month and day-of-week are restricted a day matches if either does. The
//! `@hourly`, `@daily`, `@midnight`, `@weekly`, `@monthly`, `@yearly` and `@annually` shorthands
//! are accepted.
//!
//! [`CronExpr::next_after`] works on naive local time; [`next_fire_ms`] maps that through the
//! local time zone (a time that does not exist in a spring-forward gap is skipped, an ambiguous
//! one fires at its first occurrence).

use chrono::{Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike};

/// A parsed cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64,
    hours: u32,
    /// Bit `d` for day `d` (1-31).
    days: u32,
    /// Bit `m` for month `m` (1-12).
    months: u16,
    /// Bit `w` for weekday `w` (0 = Sunday).
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
    source: String,
}

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// The farthest ahead a fire is searched for (a `0 0 29 2 *` waits for the next leap day).
const HORIZON_YEARS: i32 = 8;

impl CronExpr {
    /// Parses `expr`; the error names the field and what is wrong with it.
    pub fn parse(expr: &str) -> Result<CronExpr, String> {
        let trimmed = expr.trim();
        let expanded = match trimmed.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            _ => trimmed,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a cron expression has 5 fields (minute hour day-of-month month day-of-week), not {}: `{trimmed}`",
                fields.len()
            ));
        }
        let minutes = field(fields[0], "minute", 0, 59, &[])?;
        let hours = field(fields[1], "hour", 0, 23, &[])?;
        let days = field(fields[2], "day-of-month", 1, 31, &[])?;
        let months = field(fields[3], "month", 1, 12, &MONTH_NAMES)?;
        let mut weekdays = field(fields[4], "day-of-week", 0, 7, &WEEKDAY_NAMES)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        let restricted = |f: &str| !f.starts_with('*') && f != "?";
        Ok(CronExpr {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_restricted: restricted(fields[2]),
            weekdays_restricted: restricted(fields[4]),
            source: expanded.to_string(),
        })
    }

    /// The expression as written (shorthands expanded).
    pub fn as_str(&self) -> &str {
        &self.source
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let dom = self.days & (1 << date.day()) != 0;
        let dow = self.weekdays & (1 << date.weekday().num_days_from_sunday()) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// Whether the minute containing `t` matches.
    pub fn matches(&self, t: NaiveDateTime) -> bool {
        self.day_matches(t.date()) && self.hours & (1 << t.hour()) != 0 && self.minutes & (1 << t.minute()) != 0
    }

    /// The first matching minute strictly after `t` (seconds ignored), or `None` within
    /// [`HORIZON_YEARS`] years (an impossible date such as `0 0 31 2 *`).
    pub fn next_after(&self, t: NaiveDateTime) -> Option<NaiveDateTime> {
        let start = t.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let last_year = start.year() + HORIZON_YEARS;
        let mut date = start.date();
        let mut first_day = true;
        while date.year() <= last_year {
            if self.day_matches(date) {
                let (from_h, from_m) = if first_day { (start.hour(), start.minute()) } else { (0, 0) };
                for h in from_h..24 {
                    if self.hours & (1 << h) == 0 {
                        continue;
                    }
                    let m0 = if h == from_h { from_m } else { 0 };
                    if let Some(m) = (m0..60).find(|m| self.minutes & (1 << m) != 0) {
                        return date.and_hms_opt(h, m, 0);
                    }
                }
            }
            date = date.succ_opt()?;
            first_day = false;
        }
        None
    }
}

/// One field's bit set. `names` are the three-letter names for values starting at `lo` (months
/// start at 1, weekdays at 0).
fn field(text: &str, what: &str, lo: u32, hi: u32, names: &[&str]) -> Result<u64, String> {
    let bad = |why: String| format!("{what} field `{text}`: {why}");
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + if what == "month" { 1 } else { 0 });
        }
        let v: u32 = s.parse().map_err(|_| bad(format!("`{s}` is not a number{}", if names.is_empty() { "" } else { " or name" })))?;
        if v < lo || v > hi {
            return Err(bad(format!("{v} is outside {lo}-{hi}")));
        }
        Ok(v)
    };
    let mut set = 0u64;
    for part in text.split(',') {
        if part.is_empty() {
            return Err(bad("an empty list item".into()));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| bad(format!("step `{s}` is not a number")))?;
                if step == 0 {
                    return Err(bad("a step of 0".into()));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (a, b) = if range == "*" || range == "?" {
            (lo, if what == "day-of-week" { 6 } else { hi })
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let a = value(range)?;
            // `5/15` runs from 5 to the end of the field.
            (a, if part.contains('/') { hi } else { a })
        };
        if a > b {
            return Err(bad(format!("the range {a}-{b} runs backwards")));
        }
        let mut v = a;
        while v <= b {
            set |= 1 << v;
            v += step;
        }
    }
    Ok(set)
}

/// The local-time instant (Unix ms) for a naive local time: the first occurrence of an ambiguous
/// one, `None` for one in a spring-forward gap.
pub fn local_to_ms(t: NaiveDateTime) -> Option<i64> {
    match Local.from_local_datetime(&t) {
        LocalResult::Single(d) => Some(d.timestamp_millis()),
        LocalResult::Ambiguous(first, _) => Some(first.timestamp_millis()),
        LocalResult::None => None,
    }
}

/// A local time as the model writes it (`2026-10-12T09:00`, with or without seconds, `T` or a
/// space), in Unix ms.
pub fn parse_local_time(text: &str) -> Option<i64> {
    let t = text.trim();
    let formats = ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"];
    let naive = formats.iter().find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())?;
    local_to_ms(naive)
}

/// Naive local time of a Unix-ms instant.
pub fn ms_to_local(ms: i64) -> Option<NaiveDateTime> {
    Local.timestamp_millis_opt(ms).single().map(|d| d.naive_local())
}

/// The next fire strictly after `after_ms`, in Unix ms, in the machine's local time zone.
pub fn next_fire_ms(expr: &CronExpr, after_ms: i64) -> Option<i64> {
    let mut t = ms_to_local(after_ms)?;
    // A gap hour has no instants: keep searching past it.
    for _ in 0..64 {
        let next = expr.next_after(t)?;
        if let Some(ms) = local_to_ms(next) {
            if ms > after_ms {
                return Some(ms);
            }
        }
        t = next;
    }
    None
}

/// How many fires fall in `(from_ms, to_ms]`, counting at most `cap` (a laptop that slept).
pub fn count_fires(expr: &CronExpr, from_ms: i64, to_ms: i64, cap: u32) -> u32 {
    let mut n = 0;
    let mut at = from_ms;
    while n < cap {
        match next_fire_ms(expr, at) {
            Some(t) if t <= to_ms => {
                n += 1;
                at = t;
            }
            _ => break,
        }
    }
    n
}

/// `Sat 09:00` style text for a fire time, local.
pub fn describe_ms(ms: i64) -> String {
    match ms_to_local(ms) {
        Some(t) => t.format("%a %Y-%m-%d %H:%M").to_string(),
        None => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn next(expr: &str, from: &str) -> String {
        CronExpr::parse(expr).unwrap().next_after(at(from)).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or("none".into())
    }

    #[test]
    fn parse_table() {
        // (expression, from, next fire)
        let table = [
            ("* * * * *", "2026-10-10 12:00", "2026-10-10 12:01"),
            ("*/15 * * * *", "2026-10-10 12:00", "2026-10-10 12:15"),
            ("*/15 * * * *", "2026-10-10 12:45", "2026-10-10 13:00"),
            ("0 3 * * *", "2026-10-10 03:00", "2026-10-11 03:00"),
            ("0 3 * * *", "2026-10-10 02:59", "2026-10-10 03:00"),
            ("30 9 * * 1-5", "2026-10-10 12:00", "2026-10-12 09:30"), // Sat -> Mon
            ("0 0 1 * *", "2026-10-10 12:00", "2026-11-01 00:00"),
            ("0 0 1 1 *", "2026-10-10 12:00", "2027-01-01 00:00"),
            ("0 12 * * mon", "2026-10-10 12:00", "2026-10-12 12:00"),
            ("0 12 * * 7", "2026-10-10 12:00", "2026-10-11 12:00"), // 7 is Sunday
            ("0 12 * * 0", "2026-10-10 12:00", "2026-10-11 12:00"),
            ("0 0 * jan,jul *", "2026-10-10 12:00", "2027-01-01 00:00"),
            ("5,35 8-10 * * *", "2026-10-10 09:35", "2026-10-10 10:05"),
            ("10-20/5 * * * *", "2026-10-10 12:12", "2026-10-10 12:15"),
            ("5/20 * * * *", "2026-10-10 12:06", "2026-10-10 12:25"),
            ("0 0 29 2 *", "2026-10-10 12:00", "2028-02-29 00:00"), // the next leap day
            ("@hourly", "2026-10-10 12:30", "2026-10-10 13:00"),
            ("@daily", "2026-10-10 12:30", "2026-10-11 00:00"),
            ("@weekly", "2026-10-10 12:30", "2026-10-11 00:00"),
            ("0 0 31 2 *", "2026-10-10 12:00", "none"), // never
        ];
        for (expr, from, want) in table {
            assert_eq!(next(expr, from), want, "{expr} after {from}");
        }
    }

    #[test]
    fn day_of_month_and_weekday_both_restricted_match_either() {
        // The 13th, or any Friday.
        let e = CronExpr::parse("0 0 13 * fri").unwrap();
        assert_eq!(e.next_after(at("2026-10-10 12:00")).unwrap(), at("2026-10-13 00:00"));
        assert_eq!(e.next_after(at("2026-10-13 00:00")).unwrap(), at("2026-10-16 00:00"));
        // Only one restricted: both must hold (a star matches everything).
        let e = CronExpr::parse("0 0 13 * *").unwrap();
        assert_eq!(e.next_after(at("2026-10-14 00:00")).unwrap(), at("2026-11-13 00:00"));
    }

    #[test]
    fn the_error_names_the_field() {
        let cases = [
            ("* * * *", "5 fields"),
            ("60 * * * *", "minute field `60`: 60 is outside 0-59"),
            ("* 24 * * *", "hour field"),
            ("* * 0 * *", "day-of-month field"),
            ("* * * 13 *", "month field"),
            ("* * * * 8", "day-of-week field"),
            ("*/0 * * * *", "a step of 0"),
            ("30-10 * * * *", "runs backwards"),
            ("a * * * *", "not a number"),
            ("* * * foo *", "not a number or name"),
            (", * * * *", "empty list item"),
            ("", "5 fields"),
        ];
        for (expr, want) in cases {
            let err = CronExpr::parse(expr).unwrap_err();
            assert!(err.contains(want), "`{expr}` -> {err}");
        }
    }

    #[test]
    fn matches_agrees_with_next_after() {
        let e = CronExpr::parse("*/20 6-7 * * *").unwrap();
        let mut t = at("2026-10-10 05:50");
        for _ in 0..8 {
            let n = e.next_after(t).unwrap();
            assert!(e.matches(n), "{n}");
            assert!(!e.matches(n + Duration::minutes(1)), "{n}");
            t = n;
        }
        // 06:00 06:20 06:40 07:00 07:20 07:40, then the next morning.
        assert_eq!(t, at("2026-10-11 06:20"));
    }

    #[test]
    fn local_time_round_trip_and_fire_counting() {
        let e = CronExpr::parse("0 * * * *").unwrap();
        let now = local_to_ms(at("2026-10-10 12:30")).unwrap();
        let n = next_fire_ms(&e, now).unwrap();
        assert_eq!(ms_to_local(n).unwrap(), at("2026-10-10 13:00"));
        // A machine that slept 5 hours missed 5 hourly fires.
        assert_eq!(count_fires(&e, now, now + 5 * 3_600_000, 99), 5);
        assert_eq!(count_fires(&e, now, now + 500 * 3_600_000, 99), 99, "capped");
        assert_eq!(count_fires(&e, now, now + 10, 99), 0);
    }
}
