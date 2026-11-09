//! osmflow's own protobuf field reader, for plain nodes, ways, relations and their metadata, which are decoded straight
//! into columns. Dense nodes are read through buffa views.

use std::result;

use crate::error::PbfFormatError;

type Result<T> = result::Result<T, PbfFormatError>;

/// Reads one varint at `pos`. A varint is at most 10 bytes, and the tenth may only hold the top bit of a `u64`.
#[inline]
pub(crate) fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

/// Calls `f` for every varint of a packed array, in order; a malformed varint is a [`PbfFormatError::Packed`] error.
#[inline]
fn for_each(packed: &[u8], what: &'static str, mut f: impl FnMut(u64) -> Result<()>) -> Result<()> {
    let mut pos = 0;
    while pos < packed.len() {
        f(varint(packed, &mut pos).ok_or(PbfFormatError::Packed(what))?)?;
    }
    Ok(())
}

/// A field of a protobuf message, as osmflow's own field reader gives it.
#[derive(Clone, Copy)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    /// A fixed-width field (`fixed32`, `fixed64`, `float`, `double`, ...). No field osmflow reads has this wire
    /// type, so its value is not kept: a known field that arrives as one is an error, and an unknown one is skipped.
    Fixed,
}

/// Calls `f` with the number and value of every field of a message; the deprecated group wire types are an error.
pub(crate) fn fields<'a>(
    msg: &'a [u8],
    what: &'static str,
    mut f: impl FnMut(u32, Value<'a>) -> Result<()>,
) -> Result<()> {
    let err = || PbfFormatError::Message(what);
    let mut pos = 0;
    while pos < msg.len() {
        let key = varint(msg, &mut pos).ok_or_else(err)?;
        let field = u32::try_from(key >> 3)
            .ok()
            .filter(|&n| n != 0)
            .ok_or_else(err)?;
        let skip = match key & 7 {
            0 => {
                f(field, Value::Varint(varint(msg, &mut pos).ok_or_else(err)?))?;
                continue;
            }
            2 => {
                let len = varint(msg, &mut pos)
                    .and_then(|l| usize::try_from(l).ok())
                    .ok_or_else(err)?;
                let bytes = msg
                    .get(pos..)
                    .and_then(|rest| rest.get(..len))
                    .ok_or_else(err)?;
                f(field, Value::Bytes(bytes))?;
                len
            }
            1 | 5 => {
                f(field, Value::Fixed)?;
                if key & 7 == 1 { 8 } else { 4 }
            }
            _ => return Err(err()),
        };
        pos = pos
            .checked_add(skip)
            .filter(|&p| p <= msg.len())
            .ok_or_else(err)?;
    }
    Ok(())
}

/// Calls `f` for every value of one occurrence of a repeated scalar field: packed bytes, or one unpacked varint. A
/// fixed-width value is an error: osmflow's repeated fields are varints.
pub(crate) fn values(
    value: Value<'_>,
    what: &'static str,
    mut f: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    match value {
        Value::Bytes(bytes) => for_each(bytes, what, f),
        Value::Varint(v) => f(v),
        Value::Fixed => Err(PbfFormatError::Packed(what)),
    }
}

/// The number of varints in packed bytes, if every one is well formed: at most 10 bytes, the tenth holding only the
/// top bit of a `u64`, and the last one not cut off.
pub(crate) fn count(packed: &[u8]) -> Option<usize> {
    let (mut count, mut continued) = (0, 0);
    for &byte in packed {
        if continued == 9 && byte > 1 {
            return None;
        }
        if byte < 0x80 {
            count += 1;
            continued = 0;
        } else {
            continued += 1;
        }
    }
    (continued == 0).then_some(count)
}

/// Appends `v` as a varint.
pub(crate) fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    let low = |v: u64| u8::try_from(v & 0x7f).unwrap_or_default();
    while v >= 0x80 {
        out.push(low(v) | 0x80);
        v >>= 7;
    }
    out.push(low(v));
}

#[inline]
pub(crate) fn zigzag(v: u64) -> i64 {
    (v >> 1).cast_signed() ^ -(v & 1).cast_signed()
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "a test encoder truncates on purpose"
)]
mod tests {
    use super::*;

    fn encode(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            let mut v = v;
            while v >= 0x80 {
                out.push((v as u8) | 0x80);
                v >>= 7;
            }
            out.push(v as u8);
        }
        out
    }

    fn all(packed: &[u8]) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        for_each(packed, "t", |v| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn varint_edges() {
        let values = [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ];
        let bytes = encode(&values);
        assert_eq!(all(&bytes).unwrap(), values);
    }

    #[test]
    fn rejects_bad_varints() {
        // ends inside a varint
        all(&[0x80]).unwrap_err();
        all(&[0x01, 0x80]).unwrap_err();
        // eleven bytes
        let mut eleven = vec![0xff; 10];
        eleven.push(0x01);
        all(&eleven).unwrap_err();
        // a tenth byte with more than the top bit
        let mut ten = vec![0xff; 9];
        ten.push(0x02);
        all(&ten).unwrap_err();
        let mut ok = vec![0xff; 9];
        ok.push(0x01);
        assert_eq!(all(&ok).unwrap(), [u64::MAX]);
    }

    #[test]
    fn walks_fields() {
        // field 1 varint 150, field 2 bytes "ab", field 3 fixed64, field 4 fixed32, field 5 varint 1
        let msg = [
            0x08, 0x96, 0x01, 0x12, 2, b'a', b'b', 0x19, 0, 0, 0, 0, 0, 0, 0, 0, 0x25, 0, 0, 0, 0,
            0x28, 1,
        ];
        let mut seen = Vec::new();
        fields(&msg, "t", |f, v| {
            seen.push(match v {
                Value::Varint(v) => (f, Some(v), None),
                Value::Bytes(b) => (f, None, Some(b.to_vec())),
                Value::Fixed => (f, None, None),
            });
            Ok(())
        })
        .unwrap();
        // fixed-width fields are reported without their value, so a known one can be rejected
        assert_eq!(
            seen,
            [
                (1, Some(150), None),
                (2, None, Some(b"ab".to_vec())),
                (3, None, None),
                (4, None, None),
                (5, Some(1), None),
            ]
        );
        // a length beyond the message, a group wire type, field number 0, a truncated fixed64
        for bad in [&[0x12, 5, b'a'][..], &[0x0b], &[0x00, 1], &[0x09, 0, 0]] {
            assert!(fields(bad, "t", |_, _| Ok(())).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn count_checks_every_varint() {
        let mut packed = Vec::new();
        for v in [0, 1, 127, 128, u64::MAX] {
            push_varint(&mut packed, v);
        }
        assert_eq!(packed, encode(&[0, 1, 127, 128, u64::MAX]));
        assert_eq!(count(&packed), Some(5));
        assert_eq!(count(&[]), Some(0));
        assert_eq!(count(&[0x80]), None, "cut off");
        assert_eq!(
            count(&[0xff; 9].iter().copied().chain([2]).collect::<Vec<_>>()),
            None,
            "past 64 bits"
        );
        assert_eq!(count(&[0x80; 10]), None, "11 bytes");
    }

    #[test]
    fn values_packed_or_not() {
        let mut out = Vec::new();
        values(Value::Bytes(&encode(&[1, 300])), "t", |v| {
            out.push(v);
            Ok(())
        })
        .unwrap();
        values(Value::Varint(7), "t", |v| {
            out.push(v);
            Ok(())
        })
        .unwrap();
        assert_eq!(out, [1, 300, 7]);
        assert!(values(Value::Fixed, "t", |_| Ok(())).is_err());
    }
}
