//! Writing values as JSON and as text: strings, coordinates, timestamps, counts, sizes and group kinds; and widening
//! ranges of them.

use std::io::{self, Write};

use osmflow::{GroupKind, Timestamp};

/// How `stats` and `seq` write what they find.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum SummaryFormat {
    /// Lines of text.
    #[default]
    Text,
    /// One JSON object.
    Json,
}

/// The names of the kinds of groups, in the order of [`group_index`].
pub const GROUP_NAMES: [&str; 5] = ["plain nodes", "dense nodes", "ways", "relations", "empty"];

/// The name of a group kind, from [`GROUP_NAMES`].
pub fn group_name(kind: GroupKind) -> &'static str {
    GROUP_NAMES[group_index(kind)]
}

/// The position of a group kind in [`GROUP_NAMES`].
pub fn group_index(kind: GroupKind) -> usize {
    match kind {
        GroupKind::Nodes => 0,
        GroupKind::DenseNodes => 1,
        GroupKind::Ways => 2,
        GroupKind::Relations => 3,
        GroupKind::Empty | _ => 4,
    }
}

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

/// Widens `range` to hold `low` and `high`.
pub fn widen<T: Ord + Copy>(range: &mut Option<(T, T)>, low: T, high: T) {
    *range = Some(match *range {
        Some((a, b)) => (a.min(low), b.max(high)),
        None => (low, high),
    });
}

/// Writes `s` as a JSON string, or `null`.
pub fn opt_str(o: &mut impl Write, s: Option<&str>) -> io::Result<()> {
    match s {
        Some(s) => json_str(o, s),
        None => o.write_all(b"null"),
    }
}

/// Writes `n` as a JSON number, or `null`.
pub fn opt_num(o: &mut impl Write, n: Option<i64>) -> io::Result<()> {
    match n {
        Some(n) => write!(o, "{n}"),
        None => o.write_all(b"null"),
    }
}

/// West, south, east and north, from the ranges of latitudes and longitudes.
fn bounds(lat: Option<(i32, i32)>, lon: Option<(i32, i32)>) -> Option<[i32; 4]> {
    let ((south, north), (west, east)) = (lat?, lon?);
    Some([west, south, east, north])
}

/// Writes the line `node extent   lon W .. E, lat S .. N` of a summary, if there are nodes.
pub fn extent_line(
    o: &mut impl Write,
    lat: Option<(i32, i32)>,
    lon: Option<(i32, i32)>,
) -> io::Result<()> {
    let Some([west, south, east, north]) = bounds(lat, lon) else {
        return Ok(());
    };
    o.write_all(b"node extent   lon ")?;
    degrees(o, west)?;
    o.write_all(b" .. ")?;
    degrees(o, east)?;
    o.write_all(b", lat ")?;
    degrees(o, south)?;
    o.write_all(b" .. ")?;
    degrees(o, north)?;
    writeln!(o)
}

/// Writes the extent of a JSON summary: `[west,south,east,north]`, or `null` without nodes.
pub fn extent_json(
    o: &mut impl Write,
    lat: Option<(i32, i32)>,
    lon: Option<(i32, i32)>,
) -> io::Result<()> {
    let Some(bounds) = bounds(lat, lon) else {
        return o.write_all(b"null");
    };
    o.write_all(b"[")?;
    for (i, v) in bounds.into_iter().enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        degrees(o, v)?;
    }
    o.write_all(b"]")
}

/// Writes `"first_timestamp":…,"last_timestamp":…` of a JSON summary, `null` without timestamps.
pub fn time_span_json(o: &mut impl Write, time: Option<(Timestamp, Timestamp)>) -> io::Result<()> {
    o.write_all(br#""first_timestamp":"#)?;
    opt_time(o, time.map(|t| t.0))?;
    o.write_all(br#","last_timestamp":"#)?;
    opt_time(o, time.map(|t| t.1))
}

/// Writes the start of a summary's timestamps line, `timestamps    FIRST .. LAST`, without its end of line.
pub fn time_span(o: &mut impl Write, (first, last): (Timestamp, Timestamp)) -> io::Result<()> {
    write!(o, "timestamps    {first} .. {last}")
}

/// Writes a time as a quoted ISO 8601 time, or `null`.
pub fn opt_time(o: &mut impl Write, time: Option<Timestamp>) -> io::Result<()> {
    match time {
        Some(time) => write!(o, "\"{time}\""),
        None => o.write_all(b"null"),
    }
}

/// Writes strings as a JSON array.
pub fn str_array(o: &mut impl Write, items: &[String]) -> io::Result<()> {
    o.write_all(b"[")?;
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        json_str(o, s)?;
    }
    o.write_all(b"]")
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
    fn counts_and_sizes() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_958_977), "1,958,977");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(21_300_000), "21.3 MB");
        assert_eq!(size(95_500_000_000), "95.5 GB");
    }
}
