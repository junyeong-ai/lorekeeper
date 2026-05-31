//! Minimal cron schedule evaluation: the next fire time at or after a given instant.
//!
//! Mirrors the 5-field crontab syntax that [`crate::config`] validates at load
//! (`minute hour day-of-month month day-of-week`), including `*`, lists (`a,b`),
//! ranges (`a-b`), and steps (`*/n`, `a-b/n`), with Vixie OR-semantics when BOTH
//! day-of-month and day-of-week are restricted. Used to derive a source's expected
//! cadence (e.g. for staleness reporting) from its own schedule instead of a flat
//! hardcoded window. Inputs are assumed already validated by `config::validate_cron`.

use jiff::civil::Weekday;
use jiff::tz::TimeZone;

struct Schedule {
    minute: Vec<u8>,
    hour: Vec<u8>,
    dom: Vec<u8>,
    month: Vec<u8>,
    dow: Vec<u8>,
    dom_restricted: bool,
    dow_restricted: bool,
}

fn parse_field(field: &str, min: u8, max: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u8>().ok().filter(|n| *n > 0)?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let v = range.parse().ok()?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return None;
        }
        // Step in a wider integer so a large step (validation permits any `n > 0`, e.g.
        // `58-59/250`) can't overflow `u8` — that would panic in debug and wrap into an
        // infinite loop in release.
        let mut v = lo as u32;
        let step = step as u32;
        let hi = hi as u32;
        while v <= hi {
            out.push(v as u8);
            v += step;
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

impl Schedule {
    fn parse(expr: &str) -> Option<Self> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return None;
        }
        // day-of-week 7 is a Sunday alias for 0; normalize after parsing 0..=7.
        let mut dow = parse_field(f[4], 0, 7)?;
        for d in &mut dow {
            if *d == 7 {
                *d = 0;
            }
        }
        dow.sort_unstable();
        dow.dedup();
        Some(Self {
            minute: parse_field(f[0], 0, 59)?,
            hour: parse_field(f[1], 0, 23)?,
            dom: parse_field(f[2], 1, 31)?,
            month: parse_field(f[3], 1, 12)?,
            dow,
            dom_restricted: f[2] != "*",
            dow_restricted: f[4] != "*",
        })
    }

    fn matches(&self, dt: jiff::civil::DateTime) -> bool {
        if !self.minute.contains(&(dt.minute() as u8))
            || !self.hour.contains(&(dt.hour() as u8))
            || !self.month.contains(&(dt.month() as u8))
        {
            return false;
        }
        let dom_ok = self.dom.contains(&(dt.day() as u8));
        let wd = match dt.weekday() {
            Weekday::Sunday => 0,
            Weekday::Monday => 1,
            Weekday::Tuesday => 2,
            Weekday::Wednesday => 3,
            Weekday::Thursday => 4,
            Weekday::Friday => 5,
            Weekday::Saturday => 6,
        };
        let dow_ok = self.dow.contains(&wd);
        // Vixie cron: when both day fields are restricted, a day matches if EITHER does;
        // otherwise the restricted one (or both wildcards) governs.
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }
}

/// The next fire time strictly after `after` (in `tz`), or `None` if the expression
/// is malformed or no fire occurs within a bounded look-ahead (~13 months — long
/// enough to cover any annual schedule). Minute-resolution, matching crontab.
pub fn next_fire_after(
    expr: &str,
    after: jiff::Timestamp,
    tz: &TimeZone,
) -> Option<jiff::Timestamp> {
    let schedule = Schedule::parse(expr)?;
    // Start at the next whole minute after `after`.
    let mut cursor = after
        .to_zoned(tz.clone())
        .round(
            jiff::ZonedRound::new()
                .smallest(jiff::Unit::Minute)
                .mode(jiff::RoundMode::Trunc),
        )
        .ok()?
        .checked_add(jiff::Span::new().minutes(1))
        .ok()?;
    // Bound the search so a never-matching field combination can't loop forever.
    const MAX_MINUTES: i64 = 60 * 24 * 400;
    for _ in 0..MAX_MINUTES {
        if schedule.matches(cursor.datetime()) {
            return Some(cursor.timestamp());
        }
        cursor = cursor.checked_add(jiff::Span::new().minutes(1)).ok()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    /// The fire after the fire after `from` — the "second run due", used by health
    /// staleness (one missed-run of grace).
    fn second_after(expr: &str, from: &str, tz: &TimeZone) -> jiff::Timestamp {
        let first = next_fire_after(expr, ts(from), tz).unwrap();
        next_fire_after(expr, first, tz).unwrap()
    }

    #[test]
    fn daily_second_fire_is_two_days_out() {
        let tz = TimeZone::UTC;
        // Last run 09:00 → next 09:00 (next day) → second 09:00 (day after).
        assert_eq!(
            second_after("0 9 * * *", "2026-05-23T09:00:00Z", &tz),
            ts("2026-05-25T09:00:00Z")
        );
    }

    #[test]
    fn hourly_next_fire_is_one_hour() {
        let tz = TimeZone::UTC;
        let next = next_fire_after("0 * * * *", ts("2026-05-23T00:30:00Z"), &tz).unwrap();
        assert_eq!(next, ts("2026-05-23T01:00:00Z"));
    }

    #[test]
    fn weekday_next_fire_skips_weekend() {
        let tz = TimeZone::UTC;
        // Mon–Fri 09:00. 2026-05-23 is a Saturday → next fire is Monday 2026-05-25 09:00.
        let next = next_fire_after("0 9 * * 1-5", ts("2026-05-23T12:00:00Z"), &tz).unwrap();
        assert_eq!(next, ts("2026-05-25T09:00:00Z"));
    }

    #[test]
    fn weekday_second_fire_after_friday_spans_weekend() {
        // The health-staleness anchor: a Friday-morning success on a `0 9 * * 1-5`
        // schedule is not "two fires due" until Tuesday — so a weekend check is never
        // a false STALE. First fire after Fri = Mon, second = Tue.
        let tz = TimeZone::UTC;
        // 2026-05-22 is a Friday.
        assert_eq!(
            second_after("0 9 * * 1-5", "2026-05-22T09:00:00Z", &tz),
            ts("2026-05-26T09:00:00Z") // Tuesday
        );
    }

    #[test]
    fn step_field_every_15_min() {
        let tz = TimeZone::UTC;
        let next = next_fire_after("*/15 * * * *", ts("2026-05-23T00:02:00Z"), &tz).unwrap();
        assert_eq!(next, ts("2026-05-23T00:15:00Z"));
    }

    #[test]
    fn malformed_expr_is_none() {
        let tz = TimeZone::UTC;
        assert!(next_fire_after("not a cron", ts("2026-05-23T00:00:00Z"), &tz).is_none());
        // Wrong field count.
        assert!(next_fire_after("0 9 * *", ts("2026-05-23T00:00:00Z"), &tz).is_none());
    }

    #[test]
    fn vixie_or_semantics_when_both_day_fields_restricted() {
        // `0 9 13 * 5` fires on the 13th OR any Friday (Vixie OR). 2026-05-23 is a
        // Saturday; the next match from there is the next Friday (2026-05-29), well
        // before the next 13th.
        let tz = TimeZone::UTC;
        let next = next_fire_after("0 9 13 * 5", ts("2026-05-23T00:00:00Z"), &tz).unwrap();
        assert_eq!(next, ts("2026-05-29T09:00:00Z"));
        // And the 13th itself matches even when it isn't a Friday (2026-06-13 is a Sat).
        let on_13th = next_fire_after("0 9 13 * 5", ts("2026-06-10T00:00:00Z"), &tz).unwrap();
        assert_eq!(on_13th, ts("2026-06-12T09:00:00Z")); // Fri the 12th comes before the 13th
    }

    #[test]
    fn list_and_range_fields() {
        let tz = TimeZone::UTC;
        // 09:00 and 17:00 only.
        let next = next_fire_after("0 9,17 * * *", ts("2026-05-23T10:00:00Z"), &tz).unwrap();
        assert_eq!(next, ts("2026-05-23T17:00:00Z"));
        // Minutes 0,15,30,45 via range+step.
        let q = next_fire_after("0-59/15 * * * *", ts("2026-05-23T00:20:00Z"), &tz).unwrap();
        assert_eq!(q, ts("2026-05-23T00:30:00Z"));
    }

    #[test]
    fn never_matching_schedule_returns_none() {
        // Feb 30 never exists → no fire within the bounded look-ahead.
        let tz = TimeZone::UTC;
        assert!(next_fire_after("0 9 30 2 *", ts("2026-05-23T00:00:00Z"), &tz).is_none());
    }

    #[test]
    fn large_step_does_not_panic_or_loop() {
        // `validate_cron` permits any step > 0; a step wider than the range must parse
        // to just the low bound (one value) without overflowing u8.
        let tz = TimeZone::UTC;
        let next = next_fire_after("58-59/250 * * * *", ts("2026-05-23T00:00:00Z"), &tz).unwrap();
        // Only minute 58 qualifies (58 + 250 overflows the range, so no second value).
        assert_eq!(next, ts("2026-05-23T00:58:00Z"));
    }
}
