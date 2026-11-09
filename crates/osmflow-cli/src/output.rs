//! Writing values as JSON and as text: strings, coordinates, timestamps, counts and sizes.

use std::io::{self, Write};

/// Writes `s` as a quoted JSON string.
pub fn json_str(out: &mut impl Write, s: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let escaped: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f => b"",
            _ => continue,
        };
        out.write_all(&bytes[start..i])?;
        if escaped.is_empty() {
            write!(out, "\\u{b:04x}")?;
        } else {
            out.write_all(escaped)?;
        }
        start = i + 1;
    }
    out.write_all(&bytes[start..])?;
    out.write_all(b"\"")
}

/// Writes a coordinate given in 1e-7 degrees as degrees, exactly, without trailing zeros.
pub fn degrees(out: &mut impl Write, decimicro: i32) -> io::Result<()> {
    fixed(out, i64::from(decimicro), 7)
}

/// Writes a coordinate given in nanodegrees, as a file's header bounding box has them, as degrees.
pub fn nanodegrees(out: &mut impl Write, nano: i64) -> io::Result<()> {
    fixed(out, nano, 9)
}

/// Writes `value / 10^decimals` exactly, without trailing zeros.
fn fixed(out: &mut impl Write, value: i64, decimals: usize) -> io::Result<()> {
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    let scale = 10_u64.pow(u32::try_from(decimals).unwrap_or(0));
    let (whole, mut digits, mut width) = (abs / scale, abs % scale, decimals);
    if digits == 0 {
        return write!(out, "{sign}{whole}");
    }
    while digits % 10 == 0 {
        digits /= 10;
        width -= 1;
    }
    write!(out, "{sign}{whole}.{digits:0width$}")
}

/// A coordinate in degrees, as a geometry holds it, back in the 1e-7 degrees the file stores.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a coordinate in 1e-7 degrees fits an i32, as it did in the file"
)]
pub fn to_decimicro(deg: f64) -> i32 {
    (deg * 1e7).round() as i32
}

/// Writes milliseconds since 1970 as an ISO 8601 UTC time, with milliseconds only when there are some.
pub fn timestamp(out: &mut impl Write, ms: i64) -> io::Result<()> {
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, day_secs) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (day_secs / 3600, day_secs / 60 % 60, day_secs % 60);
    write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    )?;
    if millis != 0 {
        write!(out, ".{millis:03}")?;
    }
    out.write_all(b"Z")
}

/// The date of a day counted from 1970-01-01, in the proleptic Gregorian calendar (Howard Hinnant's algorithm).
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

/// A count with thousands separators: `1,958,977`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A size in decimal units, as disks and downloads give them: `21.3 MB`.
#[expect(clippy::cast_precision_loss, reason = "for display")]
pub fn size(bytes: u64) -> String {
    let units = [(1e12, "TB"), (1e9, "GB"), (1e6, "MB"), (1e3, "kB")];
    let b = bytes as f64;
    units.iter().find(|&&(unit, _)| b >= unit).map_or_else(
        || format!("{bytes} B"),
        |&(unit, name)| format!("{:.1} {name}", b / unit),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(
            text(|o| json_str(o, "a\"b\\c\nd\u{1}é")),
            r#""a\"b\\c\nd\u0001é""#
        );
    }

    #[test]
    fn degrees_are_exact() {
        assert_eq!(text(|o| degrees(o, 0)), "0");
        assert_eq!(text(|o| degrees(o, 388_977_000)), "38.8977");
        assert_eq!(text(|o| degrees(o, -770_365_123)), "-77.0365123");
        assert_eq!(text(|o| degrees(o, -5)), "-0.0000005");
        assert_eq!(text(|o| degrees(o, i32::MIN)), "-214.7483648");
        assert_eq!(to_decimicro(-77.036_512_3), -770_365_123);
        assert_eq!(text(|o| nanodegrees(o, -180_000_000_000)), "-180");
        assert_eq!(text(|o| nanodegrees(o, 38_791_600_100)), "38.7916001");
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(text(|o| timestamp(o, 0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            text(|o| timestamp(o, 1_794_124_821_000)),
            "2026-11-08T08:00:21Z"
        );
        assert_eq!(
            text(|o| timestamp(o, 951_782_400_123)),
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(text(|o| timestamp(o, -1000)), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn counts_and_sizes() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_958_977), "1,958,977");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(21_300_000), "21.3 MB");
        assert_eq!(size(95_500_000_000), "95.5 GB");
    }
}
