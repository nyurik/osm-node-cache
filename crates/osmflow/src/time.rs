//! Points in time, as OSM data gives them: milliseconds since 1970, UTC.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// A point in time, to the millisecond, as milliseconds since 1970-01-01T00:00:00Z: the finest precision OSM data
/// has, as PBF files give it in milliseconds and osmChange and state files in seconds. It displays as an ISO 8601
/// UTC time, `2026-11-09T21:00:00Z`, with milliseconds only when there are some, and parses from the form osmChange
/// and state files write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 1970-01-01T00:00:00Z, which PBF files also give an object whose timestamp they leave out.
    pub const EPOCH: Self = Self(0);

    /// The time `ms` milliseconds after 1970, or before it if negative.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// The time `secs` seconds after 1970, as PBF headers give a replication time; the earliest or the latest time
    /// for one too large to be in milliseconds.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1000))
    }

    /// Milliseconds since 1970.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// The time `d` earlier, or the earliest time there is.
    #[must_use]
    pub fn saturating_sub(self, d: Duration) -> Self {
        Self(self.0.saturating_sub(millis(d)))
    }

    /// The time `d` later, or the latest time there is.
    #[must_use]
    pub fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(millis(d)))
    }

    /// The time from `earlier` to this one, in milliseconds: negative when `earlier` is later.
    #[must_use]
    pub const fn millis_since(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// A duration in milliseconds, or the most there are.
fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (secs, millis) = (self.0.div_euclid(1000), self.0.rem_euclid(1000));
        let (days, day_secs) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let (year, month, day) = civil_from_days(days);
        let (hour, minute, second) = (day_secs / 3600, day_secs / 60 % 60, day_secs % 60);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        )?;
        if millis != 0 {
            write!(f, ".{millis:03}")?;
        }
        f.write_str("Z")
    }
}

/// A text that is not a time written `YYYY-MM-DDThh:mm:ssZ`, or that names no real date or time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a time written YYYY-MM-DDThh:mm:ssZ")]
pub struct TimestampError(String);

impl FromStr for Timestamp {
    type Err = TimestampError;

    /// Parses a time in the fixed form `YYYY-MM-DDThh:mm:ssZ`, as osmChange and state files write it, with every
    /// field checked against the calendar.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s.as_bytes()).ok_or_else(|| TimestampError(s.to_owned()))
    }
}

/// [`Timestamp::from_str`] on bytes, for parsers that read them.
pub(crate) fn parse(s: &[u8]) -> Option<Timestamp> {
    let s: &[u8; 20] = s.try_into().ok()?;
    if s[4] != b'-'
        || s[7] != b'-'
        || s[10] != b'T'
        || s[13] != b':'
        || s[16] != b':'
        || s[19] != b'Z'
    {
        return None;
    }
    let digit = |b: u8| {
        let d = b.wrapping_sub(b'0');
        (d < 10).then_some(i64::from(d))
    };
    let two = |i: usize| Some(digit(s[i])? * 10 + digit(s[i + 1])?);
    let (year, month, day) = (two(0)? * 100 + two(2)?, two(5)?, two(8)?);
    let (hour, minute, second) = (two(11)?, two(14)?, two(17)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(Timestamp(seconds * 1000))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar: years are counted from March, so a leap day
/// ends its year, and 400-year eras repeat exactly.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day counted from 1970-01-01, in the proleptic Gregorian calendar: [`days_from_civil`] backwards
/// (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_in_the_fixed_form_only() {
        let ts = |s: &str| parse(s.as_bytes()).map(Timestamp::as_millis);
        assert_eq!(ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ts("1969-12-31T23:59:59Z"), Some(-1000));
        assert_eq!(ts("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        assert_eq!(ts("2026-11-08T08:00:13Z"), Some(1_794_124_813_000));
        assert_eq!(ts("2038-01-19T03:14:08Z"), Some(2_147_483_648_000));
        assert_eq!(ts("2106-02-07T06:28:16Z"), Some(4_294_967_296_000));
        assert_eq!(ts("9999-12-31T23:59:59Z"), Some(253_402_300_799_000));
        assert_eq!(ts("0000-01-01T00:00:00Z"), Some(-62_167_219_200_000));
        assert_eq!(ts("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        for bad in [
            "",
            "2026-11-08T08:00:13",
            "2026-11-08T08:00:13ZZ",
            "2026-11-08 08:00:13Z",
            "2026-11-08T08:00:13+00:00",
            "2026-11-08T08:00:13.5Z",
            "2026/11/08T08:00:13Z",
            "26-11-08T08:00:13Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-32T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T0a:00:00Z",
            "-026-01-01T00:00:00Z",
            "2026-1-01T00:00:00Z ",
        ] {
            assert_eq!(ts(bad), None, "{bad:?}");
            assert!(bad.parse::<Timestamp>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn timestamps_display_as_they_parse() {
        for s in [
            "1970-01-01T00:00:00Z",
            "2026-11-09T21:00:00Z",
            "0000-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
        ] {
            assert_eq!(s.parse::<Timestamp>().unwrap().to_string(), s);
        }
        assert_eq!(
            Timestamp::from_millis(1250).to_string(),
            "1970-01-01T00:00:01.250Z"
        );
        assert_eq!(
            Timestamp::from_millis(-1).to_string(),
            "1969-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn timestamps_move_by_durations_without_overflow() {
        let t = Timestamp::from_secs(60);
        assert_eq!(
            t.saturating_sub(Duration::from_secs(90)).as_millis(),
            -30_000
        );
        assert_eq!(t.saturating_add(Duration::MAX).as_millis(), i64::MAX);
        assert_eq!(Timestamp::from_secs(i64::MAX).as_millis(), i64::MAX);
        assert_eq!(t.millis_since(Timestamp::EPOCH), 60_000);
    }

    #[test]
    fn days_match_a_day_by_day_count() {
        let mut days = days_from_civil(1600, 1, 1);
        for year in 1600..2500 {
            for month in 1..=12 {
                for day in 1..=days_in_month(year, month) {
                    assert_eq!(
                        days_from_civil(year, month, day),
                        days,
                        "{year}-{month}-{day}"
                    );
                    days += 1;
                }
            }
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}
