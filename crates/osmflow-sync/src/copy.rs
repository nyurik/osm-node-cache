//! Rows in PostgreSQL's binary `COPY` format, encoded straight into a buffer: a row is its field count, then each
//! field as its length and its bytes, big-endian. Geometries are EWKB with SRID 4326.

use osmflow::{Info, Location, Member, MemberType, Timestamp, osc};

/// The start of a binary `COPY` stream: its signature, no flags, and no header extension.
pub(crate) const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";
/// The end of a binary `COPY` stream: a field count of -1.
pub(crate) const TRAILER: &[u8] = &[0xff, 0xff];

/// PostgreSQL's epoch, 2000-01-01T00:00:00Z, in milliseconds since 1970.
const PG_EPOCH_MS: i64 = 946_684_800_000;
const CHAR_OID: i32 = 18;
const INT8_OID: i32 = 20;
const TEXT_OID: i32 = 25;
/// EWKB's flag for a geometry that carries its SRID.
const EWKB_SRID: u32 = 0x2000_0000;
const SRID: u32 = 4326;

/// An object's metadata, as both PBF files and osmChange files may give it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Meta {
    pub(crate) version: Option<i32>,
    pub(crate) changeset: Option<i64>,
    pub(crate) timestamp: Option<Timestamp>,
}

impl From<Option<Info<'_>>> for Meta {
    /// A PBF file's metadata, leaving out what it marks as missing.
    fn from(info: Option<Info<'_>>) -> Self {
        info.map_or_else(Self::default, |i| Self {
            version: (i.version >= 0).then_some(i.version),
            changeset: (i.changeset != 0).then_some(i.changeset),
            timestamp: (i.timestamp != Timestamp::EPOCH).then_some(i.timestamp),
        })
    }
}

impl From<&osc::Meta> for Meta {
    fn from(m: &osc::Meta) -> Self {
        Self {
            version: m.version,
            changeset: m.changeset,
            timestamp: m.timestamp,
        }
    }
}

/// Rows of one table, in the binary `COPY` format, without the stream's header and trailer.
#[derive(Debug, Default)]
pub(crate) struct Rows {
    pub(crate) buf: Vec<u8>,
    pub(crate) count: u64,
}

impl Rows {
    /// A row of `osm.nodes`, its columns as `Table::Nodes` lists them.
    pub(crate) fn node<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        loc: Option<Location>,
    ) {
        self.row(7)
            .i64(id)
            .meta(meta)
            .i64(rev)
            .tags(tags)
            .point(loc);
    }

    /// A row of `osm.ways`, with `minor` 0, its columns as `Table::Ways` lists them.
    pub(crate) fn way<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        nodes: impl ExactSizeIterator<Item = i64>,
        line: Option<&[Location]>,
    ) {
        self.row(9)
            .i64(id)
            .meta(meta)
            .i32(0)
            .i64(rev)
            .tags(tags)
            .ids(nodes)
            .line(line);
    }

    /// A row of `osm.relations`, its columns as `Table::Relations` lists them.
    pub(crate) fn relation<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        members: &[Member<'_>],
    ) {
        self.row(9)
            .i64(id)
            .meta(meta)
            .i64(rev)
            .tags(tags)
            .members(members);
    }

    /// Starts a row of `fields` fields.
    pub(crate) fn row(&mut self, fields: u16) -> &mut Self {
        self.count += 1;
        self.buf.extend(fields.to_be_bytes());
        self
    }

    pub(crate) fn null(&mut self) -> &mut Self {
        self.buf.extend((-1_i32).to_be_bytes());
        self
    }

    pub(crate) fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend(8_i32.to_be_bytes());
        self.buf.extend(v.to_be_bytes());
        self
    }

    pub(crate) fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend(4_i32.to_be_bytes());
        self.buf.extend(v.to_be_bytes());
        self
    }

    /// A `"char"`.
    pub(crate) fn char(&mut self, c: u8) -> &mut Self {
        self.buf.extend(1_i32.to_be_bytes());
        self.buf.push(c);
        self
    }

    /// The version, changeset and timestamp columns.
    pub(crate) fn meta(&mut self, meta: Meta) -> &mut Self {
        match meta.version {
            Some(v) => self.i32(v),
            None => self.null(),
        };
        match meta.changeset {
            Some(c) => self.i64(c),
            None => self.null(),
        };
        match meta.timestamp {
            Some(t) => self.i64((t.as_millis() - PG_EPOCH_MS) * 1000),
            None => self.null(),
        }
    }

    /// Tags as a `jsonb` object; of a key given twice, PostgreSQL keeps the last.
    pub(crate) fn tags<'a>(&mut self, tags: impl Iterator<Item = (&'a str, &'a str)>) -> &mut Self {
        self.var(|b| {
            // jsonb's binary format: version 1, then the JSON text
            b.extend(b"\x01{");
            for (i, (k, v)) in tags.enumerate() {
                if i > 0 {
                    b.push(b',');
                }
                json_str(b, k);
                b.push(b':');
                json_str(b, v);
            }
            b.push(b'}');
        })
    }

    /// An `int8[]`.
    pub(crate) fn ids(&mut self, ids: impl ExactSizeIterator<Item = i64>) -> &mut Self {
        self.var(|b| {
            array_header(b, INT8_OID, ids.len());
            for id in ids {
                b.extend(8_i32.to_be_bytes());
                b.extend(id.to_be_bytes());
            }
        })
    }

    /// A relation's members, as three columns: `"char"[]` types, `int8[]` IDs, `text[]` roles.
    pub(crate) fn members(&mut self, members: &[Member<'_>]) -> &mut Self {
        let n = members.len();
        self.var(|b| {
            array_header(b, CHAR_OID, n);
            for m in members {
                b.extend(1_i32.to_be_bytes());
                b.push(match m.kind {
                    MemberType::Node => b'n',
                    MemberType::Way => b'w',
                    MemberType::Relation => b'r',
                });
            }
        });
        self.ids(members.iter().map(|m| m.id));
        self.var(|b| {
            array_header(b, TEXT_OID, n);
            for m in members {
                b.extend(len(m.role.len()).to_be_bytes());
                b.extend(m.role.as_bytes());
            }
        })
    }

    /// A point, or NULL.
    pub(crate) fn point(&mut self, loc: Option<Location>) -> &mut Self {
        let Some(loc) = loc else {
            return self.null();
        };
        self.var(|b| {
            ewkb_header(b, 1);
            push_location(b, loc);
        })
    }

    /// A line string, or NULL for fewer than two points, which the database rejects.
    pub(crate) fn line(&mut self, locs: Option<&[Location]>) -> &mut Self {
        let Some(locs) = locs.filter(|l| l.len() >= 2) else {
            return self.null();
        };
        self.var(|b| {
            ewkb_header(b, 2);
            b.extend(len(locs.len()).to_le_bytes());
            for &loc in locs {
                push_location(b, loc);
            }
        })
    }

    /// A field whose length is known once it is written.
    fn var(&mut self, f: impl FnOnce(&mut Vec<u8>)) -> &mut Self {
        let start = self.buf.len();
        self.buf.extend([0; 4]);
        f(&mut self.buf);
        let n = len(self.buf.len() - start - 4);
        self.buf[start..start + 4].copy_from_slice(&n.to_be_bytes());
        self
    }
}

/// A length as the format stores it. OSM limits keep every field far below 2 GiB.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "fields are far below 2 GiB"
)]
fn len(n: usize) -> i32 {
    n as i32
}

/// A one-dimensional array's header, or an empty array's.
fn array_header(b: &mut Vec<u8>, oid: i32, n: usize) {
    if n == 0 {
        b.extend([0_i32, 0, oid].iter().flat_map(|v| v.to_be_bytes()));
    } else {
        b.extend(
            [1_i32, 0, oid, len(n), 1]
                .iter()
                .flat_map(|v| v.to_be_bytes()),
        );
    }
}

/// Little-endian EWKB's byte order, type with the SRID flag, and SRID.
fn ewkb_header(b: &mut Vec<u8>, kind: u32) {
    b.push(1);
    b.extend((kind | EWKB_SRID).to_le_bytes());
    b.extend(SRID.to_le_bytes());
}

fn push_location(b: &mut Vec<u8>, loc: Location) {
    b.extend(loc.lon_degrees().to_le_bytes());
    b.extend(loc.lat_degrees().to_le_bytes());
}

/// Writes `s` as a JSON string.
fn json_str(b: &mut Vec<u8>, s: &str) {
    b.push(b'"');
    for &c in s.as_bytes() {
        match c {
            b'"' => b.extend(b"\\\""),
            b'\\' => b.extend(b"\\\\"),
            0..0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                b.extend(b"\\u00");
                b.push(HEX[usize::from(c >> 4)]);
                b.push(HEX[usize::from(c & 15)]);
            }
            _ => b.push(c),
        }
    }
    b.push(b'"');
}

/// The locations of a well-known binary line string or point, as `ST_AsBinary` gives them; empty for anything else.
pub(crate) fn wkb_locations(wkb: &[u8]) -> Vec<Location> {
    let u32_at = |i: usize| {
        wkb.get(i..i + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
    };
    let f64_at = |i: usize| {
        wkb.get(i..i + 8)
            .map(|b| f64::from_le_bytes(b.try_into().unwrap_or_default()))
    };
    let at = |i: usize| Some(Location::from_degrees(f64_at(i + 8)?, f64_at(i)?));
    // Only little-endian WKB, which PostGIS writes on every common platform
    if wkb.first() != Some(&1) {
        return Vec::new();
    }
    match u32_at(1) {
        Some(1) => at(5).into_iter().collect(),
        Some(2) => {
            let n = u32_at(5).unwrap_or(0) as usize;
            (0..n).map_while(|i| at(9 + i * 16)).collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Table;

    /// The fields of each row of `rows`, walked by their lengths.
    fn fields(rows: &Rows) -> Vec<usize> {
        let mut b = &rows.buf[..];
        let mut out = Vec::new();
        while let Some((n, rest)) = b.split_first_chunk::<2>() {
            let n = u16::from_be_bytes(*n);
            b = rest;
            for _ in 0..n {
                let (len, rest) = b.split_first_chunk::<4>().unwrap();
                let len = usize::try_from(i32::from_be_bytes(*len)).unwrap_or(0);
                b = &rest[len..];
            }
            out.push(usize::from(n));
        }
        out
    }

    #[test]
    fn rows_have_their_tables_columns() {
        let tags = [("a", "b")];
        let loc = Location::new(1, 2);
        let mut rows = Rows::default();
        rows.node(1, Meta::default(), 1, tags.into_iter(), Some(loc));
        assert_eq!(fields(&rows), [Table::Nodes.columns().len()]);
        let mut rows = Rows::default();
        rows.way(
            1,
            Meta::default(),
            1,
            tags.into_iter(),
            [1, 2].into_iter(),
            Some(&[loc, loc]),
        );
        assert_eq!(fields(&rows), [Table::Ways.columns().len()]);
        let mut rows = Rows::default();
        let members = [Member {
            kind: MemberType::Way,
            id: 3,
            role: "outer",
        }];
        rows.relation(1, Meta::default(), 1, tags.into_iter(), &members);
        assert_eq!(fields(&rows), [Table::Relations.columns().len()]);
    }

    #[test]
    fn json_strings_are_escaped() {
        let mut b = Vec::new();
        json_str(&mut b, "a\"b\\c\n\u{1}é");
        assert_eq!(String::from_utf8(b).unwrap(), r#""a\"b\\c\u000a\u0001é""#);
    }

    #[test]
    fn rows_are_encoded() {
        let mut rows = Rows::default();
        rows.row(3).i64(-2).null().ids([7_i64, 8].into_iter());
        assert_eq!(rows.count, 1);
        let mut expected = vec![0, 3];
        expected.extend([
            0, 0, 0, 8, 255, 255, 255, 255, 255, 255, 255, 254, 255, 255, 255, 255,
        ]);
        expected.extend(44_i32.to_be_bytes());
        expected.extend([1_i32, 0, 20, 2, 1, 8].iter().flat_map(|v| v.to_be_bytes()));
        expected.extend(7_i64.to_be_bytes());
        expected.extend(8_i32.to_be_bytes());
        expected.extend(8_i64.to_be_bytes());
        assert_eq!(rows.buf, expected);
    }

    #[test]
    fn lines_read_back_from_wkb() {
        let locs = [
            Location::new(389_000_001, -770_000_002),
            Location::new(-1, 1_800_000_000),
        ];
        let mut rows = Rows::default();
        rows.line(Some(&locs));
        // ST_AsBinary drops EWKB's SRID: the same bytes without the SRID flag and value
        let ewkb = &rows.buf[4..];
        let mut wkb = vec![1];
        wkb.extend(2_u32.to_le_bytes());
        wkb.extend(&ewkb[9..]);
        assert_eq!(wkb_locations(&wkb), locs);
        let mut point = Rows::default();
        point.point(Some(locs[0]));
        let mut wkb = vec![1];
        wkb.extend(1_u32.to_le_bytes());
        wkb.extend(&point.buf[13..]);
        assert_eq!(wkb_locations(&wkb), [locs[0]]);
        let mut short = Rows::default();
        short.line(Some(&locs[..1]));
        assert_eq!(short.buf, (-1_i32).to_be_bytes());
    }
}
