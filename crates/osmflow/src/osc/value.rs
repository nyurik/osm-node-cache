//! Attribute values parsed straight from their bytes: integers, coordinates and escaped strings.

use crate::osc::error::XmlError;

/// The value of an ASCII digit.
fn digit(b: u8) -> Option<i64> {
    let d = b.wrapping_sub(b'0');
    (d < 10).then_some(i64::from(d))
}

/// A decimal integer: an optional `-` and at least one digit, nothing else.
pub(crate) fn int<T: TryFrom<i64>>(s: &[u8]) -> Option<T> {
    let (negative, digits) = match s.split_first()? {
        (b'-', rest) => (true, rest),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    // The magnitude is a u64, so that `i64::MIN`, one past `i64::MAX`, fits before its sign is applied.
    let mut v = 0u64;
    if digits.len() <= 19 {
        // 19 digits cannot overflow a u64.
        for &b in digits {
            v = v * 10 + digit(b)?.unsigned_abs();
        }
    } else {
        for &b in digits {
            v = v.checked_mul(10)?.checked_add(digit(b)?.unsigned_abs())?;
        }
    }
    let v = if negative {
        0i64.checked_sub_unsigned(v)?
    } else {
        i64::try_from(v).ok()?
    };
    T::try_from(v).ok()
}

/// A coordinate in degrees as 1e-7 degrees, the unit PBF files store, parsed as a fixed-point decimal and never as a
/// floating-point number: an optional `-`, digits, and an optional `.` with more digits (at least one digit in all).
/// Digits past the seventh decimal round half away from zero, as PBF writers round. A value outside `i32` is `None`.
pub(crate) fn decimicro(s: &[u8]) -> Option<i32> {
    let (negative, s) = match s.split_first()? {
        (b'-', rest) => (true, rest),
        _ => (false, s),
    };
    let (int, frac) = match s.iter().position(|&b| b == b'.') {
        Some(dot) => (&s[..dot], &s[dot + 1..]),
        None => (s, &[][..]),
    };
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0i64;
    for &b in int {
        v = v * 10 + digit(b)?;
        if v > 1000 {
            return None; // far outside i32 once scaled, and stops an overflow
        }
    }
    let mut scale = 10_000_000;
    for (i, &b) in frac.iter().enumerate() {
        let d = digit(b)?;
        if i < 7 {
            scale /= 10;
            v = v * 10 + d;
        } else if i == 7 && d >= 5 {
            v += 1; // scaled below, so this is the unit of the seventh decimal
        }
    }
    // With seven decimals or more, `scale` is 1 and the rounding unit added above is one 1e-7 degree. With fewer, no
    // rounding happened and the value is scaled up to seven decimals.
    v *= scale;
    i32::try_from(if negative { -v } else { v }).ok()
}

/// Whether an attribute value's text is its raw bytes: it has no `&`, `<` or control character, which [`unescape`]
/// replaces or rejects.
pub(crate) fn is_plain(raw: &[u8]) -> bool {
    !raw.iter().any(|&b| b == b'&' || b == b'<' || b < b' ')
}

/// Appends an attribute value's text to `out`, checking that it is UTF-8. A value that is not plain takes the slow
/// path, [`unescape`].
pub(crate) fn push_text(raw: &[u8], out: &mut String) -> Result<(), XmlError> {
    if is_plain(raw) {
        out.push_str(str::from_utf8(raw).map_err(|_| XmlError::Utf8)?);
        Ok(())
    } else {
        unescape(raw, out)
    }
}

/// Appends an attribute value with its references replaced (the five predefined entities and character references)
/// and its white space normalized as XML requires: a tab, a line feed, a carriage return, or a carriage return and line
/// feed become one space.
fn unescape(raw: &[u8], out: &mut String) -> Result<(), XmlError> {
    let text = str::from_utf8(raw).map_err(|_| XmlError::Utf8)?;
    if text
        .bytes()
        .any(|b| b < b' ' && !matches!(b, b'\t' | b'\n' | b'\r'))
    {
        return Err(XmlError::Escape);
    }
    let mut rest = text;
    while let Some(i) = rest.find(['&', '<', '\t', '\n', '\r']) {
        out.push_str(&rest[..i]);
        let b = rest.as_bytes()[i];
        rest = &rest[i + 1..];
        match b {
            b'&' => {
                let end = rest.find(';').ok_or(XmlError::Escape)?;
                out.push(reference(&rest[..end]).ok_or(XmlError::Escape)?);
                rest = &rest[end + 1..];
            }
            b'\r' => {
                out.push(' ');
                rest = rest.strip_prefix('\n').unwrap_or(rest);
            }
            b'\t' | b'\n' => out.push(' '),
            _ => return Err(XmlError::Escape),
        }
    }
    out.push_str(rest);
    Ok(())
}

/// The character a reference names, without its `&` and `;`. Character references must name a character XML allows.
fn reference(name: &str) -> Option<char> {
    let code = match name {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => match name.strip_prefix("#x") {
            Some(hex)
                if (1..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                u32::from_str_radix(hex, 16).ok()?
            }
            Some(_) => return None,
            None => {
                let dec = name.strip_prefix('#')?;
                if dec.is_empty() || dec.len() > 7 || !dec.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                dec.parse().ok()?
            }
        },
    };
    let c = char::from_u32(code)?;
    (c >= ' ' || matches!(c, '\t' | '\n' | '\r')).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(int::<i64>(b"0"), Some(0));
        assert_eq!(int::<i64>(b"-17"), Some(-17));
        assert_eq!(int::<i64>(b"0042"), Some(42));
        assert_eq!(int::<i64>(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(int::<i64>(b"-9223372036854775807"), Some(-i64::MAX));
        assert_eq!(int::<i64>(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(int::<i64>(b"-9223372036854775809"), None);
        assert_eq!(int::<i64>(b"9223372036854775808"), None);
        assert_eq!(int::<i64>(b"18446744073709551615"), None);
        assert_eq!(int::<i64>(b"-18446744073709551616"), None);
        assert_eq!(int::<i32>(b"-2147483648"), Some(i32::MIN));
        assert_eq!(int::<i64>(b"00000000000000000000001"), Some(1));
        assert_eq!(int::<i32>(b"2147483647"), Some(i32::MAX));
        assert_eq!(int::<i32>(b"2147483648"), None);
        for bad in [
            &b""[..],
            b"-",
            b"+1",
            b"1 ",
            b" 1",
            b"1.0",
            b"0x1",
            b"1e3",
            b"--1",
        ] {
            assert_eq!(int::<i64>(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn coordinates() {
        let cases: &[(&str, Option<i32>)] = &[
            ("0", Some(0)),
            ("-0", Some(0)),
            ("-0.0000000", Some(0)),
            ("1", Some(10_000_000)),
            ("1.", Some(10_000_000)),
            (".5", Some(5_000_000)),
            ("-.5", Some(-5_000_000)),
            ("48.7486381", Some(487_486_381)),
            ("-31.8771988", Some(-318_771_988)),
            ("49.127249", Some(491_272_490)),
            ("180", Some(1_800_000_000)),
            ("-180.0000000", Some(-1_800_000_000)),
            ("90", Some(900_000_000)),
            ("214.7483647", Some(i32::MAX)),
            ("-214.7483648", Some(i32::MIN)),
            ("214.7483648", None),
            ("-214.74836485", None),
            ("1000", None),
            ("99999999999999999999", None),
            // The eighth decimal rounds half away from zero; later digits do not matter.
            ("0.00000004", Some(0)),
            ("0.00000005", Some(1)),
            ("-0.00000005", Some(-1)),
            ("0.000000049999", Some(0)),
            ("1.23456789", Some(12_345_679)),
            ("1.23456785", Some(12_345_679)),
            ("1.23456784999", Some(12_345_678)),
            ("-1.23456785", Some(-12_345_679)),
            ("0.99999999", Some(10_000_000)),
            ("214.74836474", Some(i32::MAX)),
            ("214.74836475", None),
            ("", None),
            ("-", None),
            (".", None),
            ("-.", None),
            ("+1", None),
            ("1e5", None),
            ("1.5e1", None),
            ("1..5", None),
            ("1.2.3", None),
            (" 1", None),
            ("1 ", None),
            ("1,5", None),
            ("1.0000000x", None),
            ("nan", None),
        ];
        for &(s, expected) in cases {
            assert_eq!(decimicro(s.as_bytes()), expected, "{s:?}");
        }
    }

    #[test]
    fn coordinates_match_rounding_of_doubles() {
        // Every value with eight decimals rounds as `round(x * 1e7)` does, away from the halfway cases a double cannot
        // hold exactly.
        for i in (0..2_000_000_000i64).step_by(7_919_993) {
            for last in [0, 1, 4, 6, 9] {
                let text = format!(
                    "{}.{:08}",
                    i / 100_000_000,
                    i % 100_000_000 / 10 * 10 + last
                );
                let expected = (text.parse::<f64>().unwrap() * 1e7).round();
                assert_eq!(
                    f64::from(decimicro(text.as_bytes()).unwrap()),
                    expected,
                    "{text}"
                );
            }
        }
    }

    fn text(raw: &str) -> Result<String, XmlError> {
        let mut out = String::from(">");
        push_text(raw.as_bytes(), &mut out).map(|()| out[1..].to_owned())
    }

    #[test]
    fn escapes() {
        assert_eq!(text("plain ünïcode"), Ok("plain ünïcode".to_owned()));
        assert_eq!(text(""), Ok(String::new()));
        assert_eq!(text("&amp;&lt;&gt;&quot;&apos;"), Ok("&<>\"'".to_owned()));
        assert_eq!(text("a&#65;&#x42;&#x1F600;c"), Ok("aAB😀c".to_owned()));
        assert_eq!(text("&#10;&#9;&#13;"), Ok("\n\t\r".to_owned()));
        assert_eq!(text("a\tb\nc\r\nd\re"), Ok("a b c d e".to_owned()));
        assert_eq!(text("&amp;amp;"), Ok("&amp;".to_owned()));
        for bad in [
            "&",
            "&amp",
            "&foo;",
            "&#;",
            "&#x;",
            "&#0;",
            "&#1;",
            "&#xD800;",
            "&#x110000;",
            "&#-1;",
            "&#+65;",
            "&#1234567890;",
            "&#x+41;",
            "a<b",
            "a\u{1}b",
            "a\u{1}&amp;",
            "&#x1F600",
        ] {
            assert_eq!(text(bad), Err(XmlError::Escape), "{bad:?}");
        }
        let mut out = String::new();
        assert_eq!(push_text(b"\xff", &mut out), Err(XmlError::Utf8));
        assert_eq!(push_text(b"&amp;\xff", &mut out), Err(XmlError::Utf8));
    }
}
