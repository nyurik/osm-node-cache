//! Reading the start of a block: its string table and the type of its first group, from as few inflated bytes as
//! possible, so a block no filter wants can be skipped before the rest of it is inflated.

use std::ops::Range;

use crate::packed::varint;

/// The object type of a group, from its first field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Nodes,
    Ways,
    Relations,
    /// Changesets, an empty group, or a field osmflow does not know.
    Other,
}

/// What the start of a block tells.
#[derive(Debug)]
pub(crate) enum Peek {
    /// The block starts with its string table, then a group of this kind. `alone` is true when nothing follows that
    /// group, so the block holds that one group, and so one kind of object.
    Found {
        strings: Vec<Range<usize>>,
        kind: Kind,
        alone: bool,
    },
    /// At least this many bytes of the block are needed to tell.
    NeedMore(usize),
    /// The block does not start with its string table and a group, as the format allows: it is read whole.
    Unknown,
}

/// Reads the start of a block of `raw_size` bytes, of which `start` holds the first ones.
pub(crate) fn peek(start: &[u8], raw_size: usize) -> Peek {
    // Room for a field's key and length, beyond what is known to be needed.
    const SLACK: usize = 32;
    let mut p = 0;
    let Some(key) = varint(start, &mut p) else {
        return Peek::NeedMore(SLACK);
    };
    if key != (1 << 3 | 2) {
        return Peek::Unknown;
    }
    let Some(len) = varint(start, &mut p).and_then(|l| usize::try_from(l).ok()) else {
        return Peek::NeedMore(p + SLACK);
    };
    let Some(table_end) = p.checked_add(len).filter(|&e| e <= raw_size) else {
        return Peek::Unknown;
    };
    if start.len() < table_end + SLACK.min(raw_size - table_end) {
        return Peek::NeedMore(table_end + SLACK);
    }
    let Some(strings) = strings(start, p, table_end) else {
        return Peek::Unknown;
    };
    let mut p = table_end;
    if varint(start, &mut p) != Some(2 << 3 | 2) {
        return Peek::Unknown;
    }
    let Some(group_len) = varint(start, &mut p).and_then(|l| usize::try_from(l).ok()) else {
        return Peek::Unknown;
    };
    let group_end = p.checked_add(group_len);
    let kind = match varint(start, &mut p)
        .filter(|_| group_len > 0)
        .map(|k| k >> 3)
    {
        Some(1 | 2) => Kind::Nodes,
        Some(3) => Kind::Ways,
        Some(4) => Kind::Relations,
        _ => Kind::Other,
    };
    Peek::Found {
        strings,
        kind,
        alone: group_end == Some(raw_size),
    }
}

/// The strings of a string table message at `start..end`: every field 1, as the format writes them.
fn strings(block: &[u8], start: usize, end: usize) -> Option<Vec<Range<usize>>> {
    let mut out = Vec::new();
    let mut p = start;
    while p < end {
        let key = varint(block, &mut p)?;
        if key & 7 != 2 {
            return None;
        }
        let len = usize::try_from(varint(block, &mut p)?).ok()?;
        let e = p.checked_add(len).filter(|&e| e <= end)?;
        if key >> 3 == 1 {
            out.push(p..e);
        }
        p = e;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block: a string table with `strings`, then `groups`, each `(field, payload)` of one group.
    fn block(strings: &[&[u8]], groups: &[(u8, &[u8])]) -> Vec<u8> {
        let mut table = Vec::new();
        for s in strings {
            table.extend([0x0a, u8::try_from(s.len()).unwrap()]);
            table.extend_from_slice(s);
        }
        let mut out = vec![0x0a, u8::try_from(table.len()).unwrap()];
        out.extend(table);
        for (field, payload) in groups {
            let mut group = vec![field << 3 | 2, u8::try_from(payload.len()).unwrap()];
            group.extend_from_slice(payload);
            out.extend([0x12, u8::try_from(group.len()).unwrap()]);
            out.extend(group);
        }
        out
    }

    #[test]
    fn a_block_start_tells_its_strings_and_first_type() {
        let data = block(&[b"", b"highway"], &[(3, b"\x08\x01"), (4, b"\x08\x02")]);
        let Peek::Found {
            strings,
            kind,
            alone,
        } = peek(&data, data.len())
        else {
            panic!("not found");
        };
        assert_eq!(
            strings.iter().map(|r| &data[r.clone()]).collect::<Vec<_>>(),
            [&b""[..], b"highway"]
        );
        assert_eq!((kind, alone), (Kind::Ways, false));

        let one = block(&[b""], &[(2, b"\x0a\x01\x02")]);
        assert!(matches!(
            peek(&one, one.len()),
            Peek::Found {
                kind: Kind::Nodes,
                alone: true,
                ..
            }
        ));
    }

    #[test]
    fn a_short_or_unusual_start_is_not_a_peek() {
        let data = block(&[b"", b"highway"], &[(3, b"\x08\x01")]);
        assert!(matches!(peek(&data[..0], data.len()), Peek::NeedMore(_)));
        assert!(matches!(peek(&data[..5], data.len()), Peek::NeedMore(_)));
        // Granularity first, not the string table.
        assert!(matches!(peek(&[0x88, 0x01, 0x64], 3), Peek::Unknown));
        // A string table, then no group.
        let no_group = block(&[b""], &[]);
        assert!(matches!(peek(&no_group, no_group.len()), Peek::Unknown));
        // An empty group has no type.
        let empty = [&block(&[b""], &[])[..], &[0x12, 0x00]].concat();
        assert!(matches!(
            peek(&empty, empty.len()),
            Peek::Found {
                kind: Kind::Other,
                ..
            }
        ));
    }
}
